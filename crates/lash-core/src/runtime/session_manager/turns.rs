use super::*;
use crate::facade_support::RuntimeSessionStateFacadeOps;

impl ManagedSessionCapability {
    pub(in crate::runtime::session_manager) async fn start_turn(
        &self,
        current: &CurrentSessionCapability,
        usage: &UsageCapability,
        request: crate::SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, crate::PluginError> {
        let (
            crate::SessionTurnInput {
                session_id,
                turn_id,
                input,
            },
            scoped_effect_controller,
        ) = request.into_parts();
        let runtime = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned()
        }
        .ok_or_else(|| crate::PluginError::Session(format!("unknown session `{session_id}`")))?;
        let policy = {
            let runtime = runtime.runtime.lock().await;
            runtime.session_policy()
        };
        let cancel = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel::<SessionStreamEvent>(100);
        let usage_source = self.child_usage_source(usage, &session_id);
        let sink = ChannelEventSink {
            tx: event_tx,
            live_usage: Some(LiveChildUsageForwarder {
                turn_id: turn_id.to_string(),
                session_id: session_id.to_string(),
                source: usage_source,
                model: policy.model.id.clone(),
                token_ledger: Arc::clone(&usage.token_ledger),
                child_turn_live_usage: Arc::clone(&usage.child_turn_live_usage),
                relay: usage.child_usage_event_relay.clone(),
            }),
        };
        let event_drain =
            crate::task::spawn(async move { while event_rx.recv().await.is_some() {} });
        // Registration is owned by this lease for the rest of the turn. Every
        // exit — return, error, panic, or a dropped future when the owning
        // process is cancelled — releases it, because release happens in `Drop`
        // rather than at a statement after the child await.
        let lease = ManagedTurnLease::register(
            &self.turns,
            &usage.child_turn_live_usage,
            &session_id,
            &turn_id,
        )?;
        let turn = match scoped_effect_controller.into_static() {
            Ok(scoped_effect_controller) => {
                // Canonical recursion-growth seam: every shareable child turn
                // gets a fresh Tokio task stack here. Future turn-path growth
                // belongs behind this boundary, rather than in new boxes at
                // whichever recursive poll site happens to overflow next.
                let task = crate::task::spawn(
                    crate::runtime::process_worker::inherit_process_execution_permit(
                        run_managed_session_turn(
                            runtime,
                            input,
                            cancel,
                            scoped_effect_controller,
                            sink.clone(),
                        ),
                    ),
                );
                let mut abort_on_drop = AbortTaskOnDrop::new(task.abort_handle());
                let joined = task.await;
                abort_on_drop.disarm();
                match joined {
                    Ok(turn) => turn,
                    Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                    Err(err) => Err(crate::PluginError::Session(format!(
                        "child session turn task was cancelled: {err}"
                    ))),
                }
            }
            Err(scoped_effect_controller) => {
                // Handler-scoped durable controllers cannot outlive their host
                // invocation and therefore cannot cross Tokio's `'static`
                // spawn contract. Preserve their exact journal semantics by
                // retaining the scoped controller on the calling task.
                run_managed_session_turn(
                    runtime,
                    input,
                    cancel,
                    scoped_effect_controller,
                    sink.clone(),
                )
                .await
            }
        };
        drop(sink);
        let _ = event_drain.await;
        // Take the live usage only once the sink is closed and its drain has
        // finished: a live-usage forwarder still holding a sink clone could
        // otherwise re-report into the entry after it was reclaimed.
        let live_reported = lease.complete();
        if let Ok(turn) = &turn {
            let source = self.child_usage_source(usage, &session_id);
            if let Some(remainder) = subtract_usage(&live_reported, &turn.token_usage) {
                usage.record_token_usage(&source, &turn.state.policy.model.id, &remainder);
            }
        }
        usage
            .persist_current_usage_ledger(current, &turn_id)
            .await?;
        turn
    }

    fn child_usage_source(&self, usage: &UsageCapability, session_id: &str) -> String {
        usage
            .child_sources
            .lock()
            .expect("child usage sources lock")
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "child".to_string())
    }
}

type ManagedTurnRegistry = Arc<StdMutex<HashMap<String, ManagedSessionTurn>>>;
type ChildTurnLiveUsage = Arc<StdMutex<HashMap<String, TokenUsage>>>;

/// Ownership of one managed child turn's registration.
///
/// A managed turn owns two shared entries: its active-turn registration (which
/// gates `close_session` and any further turn on that session) and its
/// live-usage accumulator. Both are released by `Drop`, so cancelling the
/// process that drives the turn — which drops the `start_turn` future at
/// whichever await it is parked on — cannot strand either one. Explicit
/// completion consumes the lease and hands back the live usage it reclaimed.
///
/// Release is idempotent and identity-checked: it runs at most once per lease
/// and only removes a registration this lease still owns, so the completion
/// path and the drop path can never remove another turn's entry.
struct ManagedTurnLease {
    turns: ManagedTurnRegistry,
    live_usage: ChildTurnLiveUsage,
    session_id: String,
    turn_id: String,
    released: bool,
}

impl ManagedTurnLease {
    fn register(
        turns: &ManagedTurnRegistry,
        live_usage: &ChildTurnLiveUsage,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Self, crate::PluginError> {
        {
            let mut registered = turns.lock().expect("managed turns lock");
            if registered
                .values()
                .any(|turn| turn.session_id == session_id)
            {
                return Err(crate::PluginError::Session(format!(
                    "session `{session_id}` already has a running turn"
                )));
            }
            registered.insert(
                turn_id.to_string(),
                ManagedSessionTurn {
                    session_id: session_id.to_string(),
                },
            );
        }
        Ok(Self {
            turns: Arc::clone(turns),
            live_usage: Arc::clone(live_usage),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            released: false,
        })
    }

    /// Release the registration and take the live usage reported for this turn.
    fn release(&mut self) -> TokenUsage {
        if std::mem::replace(&mut self.released, true) {
            return TokenUsage::default();
        }
        {
            let mut registered = self.turns.lock().expect("managed turns lock");
            if registered
                .get(&self.turn_id)
                .is_some_and(|turn| turn.session_id == self.session_id)
            {
                registered.remove(&self.turn_id);
            }
        }
        self.live_usage
            .lock()
            .expect("child turn live usage lock")
            .remove(&self.turn_id)
            .unwrap_or_default()
    }

    fn complete(mut self) -> TokenUsage {
        self.release()
    }
}

impl Drop for ManagedTurnLease {
    fn drop(&mut self) {
        self.release();
    }
}

async fn run_managed_session_turn(
    runtime: RuntimeHandle,
    input: crate::TurnInput,
    cancel: CancellationToken,
    scoped_effect_controller: crate::ScopedEffectController<'_>,
    sink: ChannelEventSink,
) -> Result<AssembledTurn, crate::PluginError> {
    // This mutex is the managed runtime's single-writer boundary. Hold it for
    // the complete turn and publish from the guarded post-turn state before
    // releasing it, exactly as the former inline path did.
    let mut runtime_guard = runtime.runtime.lock().await;
    let scoped_effect_controller = scoped_effect_controller
        .rescope(
            runtime_guard.state.turn_scope(
                scoped_effect_controller
                    .turn_id()
                    .unwrap_or(scoped_effect_controller.scope_id()),
            ),
        )
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
    let result = runtime_guard
        .stream_turn_with_agent_frames(
            input,
            crate::runtime::TurnOptions::new(cancel, scoped_effect_controller).with_events(&sink),
        )
        .await
        .map_err(|err| crate::PluginError::Session(err.to_string()))
        .and_then(|run| {
            run.into_final_turn().ok_or_else(|| {
                crate::PluginError::Session("agent frame run completed without a turn".to_string())
            })
        });
    runtime.publish_from(&runtime_guard);
    result
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortTaskOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_maps() -> (ManagedTurnRegistry, ChildTurnLiveUsage) {
        (
            Arc::new(StdMutex::new(HashMap::new())),
            Arc::new(StdMutex::new(HashMap::new())),
        )
    }

    #[test]
    fn dropped_managed_turn_lease_releases_both_registration_and_live_usage() {
        let (turns, live_usage) = shared_maps();
        let lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");
        live_usage.lock().expect("live usage").insert(
            "turn".to_string(),
            TokenUsage {
                input_tokens: 3,
                ..TokenUsage::default()
            },
        );

        drop(lease);

        assert!(turns.lock().expect("turns").is_empty());
        assert!(live_usage.lock().expect("live usage").is_empty());
    }

    #[test]
    fn completed_managed_turn_lease_takes_live_usage_exactly_once() {
        let (turns, live_usage) = shared_maps();
        let lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");
        live_usage.lock().expect("live usage").insert(
            "turn".to_string(),
            TokenUsage {
                input_tokens: 3,
                ..TokenUsage::default()
            },
        );

        // `complete` consumes the lease, so its `Drop` runs immediately after
        // the explicit release: the double removal must be a no-op.
        let reported = lease.complete();

        assert_eq!(reported.input_tokens, 3);
        assert!(turns.lock().expect("turns").is_empty());
        assert!(live_usage.lock().expect("live usage").is_empty());
    }

    #[test]
    fn managed_turn_lease_release_never_removes_another_turns_registration() {
        let (turns, live_usage) = shared_maps();
        let lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");
        // A later turn took over this turn id for a different session.
        turns.lock().expect("turns").insert(
            "turn".to_string(),
            ManagedSessionTurn {
                session_id: "other-session".to_string(),
            },
        );

        drop(lease);

        assert_eq!(
            turns
                .lock()
                .expect("turns")
                .get("turn")
                .map(|turn| turn.session_id.clone()),
            Some("other-session".to_string()),
            "a stale lease must not release a registration it no longer owns"
        );
    }

    #[test]
    fn managed_turn_lease_rejects_a_second_turn_on_the_same_session() {
        let (turns, live_usage) = shared_maps();
        let _lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");

        let err = match ManagedTurnLease::register(&turns, &live_usage, "session", "other-turn") {
            Ok(_) => panic!("a session runs at most one managed turn"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("already has a running turn"));
    }

    #[test]
    fn session_turn_request_requires_matching_scope_and_sets_trace_turn_id() {
        let controller = crate::InlineRuntimeEffectController::default();
        let scoped_effect_controller = crate::ScopedEffectController::borrowed(
            &controller,
            crate::ExecutionScope::turn("child", "child-turn"),
        )
        .expect("turn scope");
        let request = crate::SessionTurnRequest::new(
            "child",
            "child-turn",
            crate::TurnInput::text("run child"),
            scoped_effect_controller,
        )
        .expect("valid child turn request");

        assert_eq!(request.session_id(), "child");
        assert_eq!(request.turn_id(), "child-turn");
        assert_eq!(request.input().trace_turn_id.as_deref(), Some("child-turn"));
    }

    #[test]
    fn session_turn_request_rejects_mismatched_execution_scope() {
        let controller = crate::InlineRuntimeEffectController::default();
        let scoped_effect_controller = crate::ScopedEffectController::borrowed(
            &controller,
            crate::ExecutionScope::turn("child", "other-turn"),
        )
        .expect("turn scope");
        let err = match crate::SessionTurnRequest::new(
            "child",
            "child-turn",
            crate::TurnInput::text("run child"),
            scoped_effect_controller,
        ) {
            Ok(_) => panic!("mismatched turn scope should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("same id"));
    }
}
