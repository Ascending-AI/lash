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
        // Registration is owned by this lease for the rest of the turn. Every
        // exit — return, error, panic, or a dropped future when the owning
        // process is cancelled — releases it, because release happens in `Drop`
        // rather than at a statement after the child await. It is claimed before
        // the event plumbing so a denied turn allocates nothing.
        let lease = ManagedTurnLease::register(
            &self.turns,
            &usage.child_turn_live_usage,
            &session_id,
            &turn_id,
        )?;
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
                turn_released: lease.released_flag(),
            }),
        };
        let event_drain =
            crate::task::spawn(async move { while event_rx.recv().await.is_some() {} });
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
        // finished, so this turn's own final usage is included. This widens the
        // window in which the session counts as "having a running turn" by the
        // drain, compared with the pre-guard code that removed the entry before
        // `drop(sink)`; `start_turn` itself is parked here, and the cancel path
        // is covered by the release flag the forwarder observes.
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

/// Process-wide registration nonce source. A nonce identifies one registration
/// attempt, which `(session_id, turn_id)` cannot: an id pair can be registered,
/// released and registered again while an older lease is still alive.
static NEXT_MANAGED_TURN_REGISTRATION: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Ownership of one managed child turn's registration.
///
/// A managed turn owns two shared entries: its active-turn registration (which
/// gates `close_session` and any further turn on that session) and its
/// live-usage accumulator. Both are released by `Drop`, so cancelling the
/// process that drives the turn — which drops the `start_turn` future at
/// whichever await it is parked on — cannot strand either one. Explicit
/// completion consumes the lease and hands back the live usage it reclaimed.
///
/// Release runs at most once per lease and is scoped to the exact registration
/// the lease created, identified by its `registration` nonce. A stale lease
/// whose id pair was since re-registered by a successor therefore releases
/// nothing — neither the registration nor the live usage — so no ABA sequence
/// can let one lease evict a live successor's turn.
struct ManagedTurnLease {
    turns: ManagedTurnRegistry,
    live_usage: ChildTurnLiveUsage,
    session_id: String,
    turn_id: String,
    registration: u64,
    /// Shared with this turn's [`LiveChildUsageForwarder`] so an in-flight child
    /// emit cannot re-create the live-usage entry after release.
    released: Arc<AtomicBool>,
}

impl ManagedTurnLease {
    fn register(
        turns: &ManagedTurnRegistry,
        live_usage: &ChildTurnLiveUsage,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Self, crate::PluginError> {
        let registration =
            NEXT_MANAGED_TURN_REGISTRATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut registered = lock_turns(turns);
            // A turn id identifies at most one live managed turn: live usage is
            // keyed by turn id alone, so admitting two concurrent turns under
            // one id would cross their usage accounting whatever the registry
            // is keyed by. Both denials emit their consulted state below.
            if let Some(occupied) = registered.get(turn_id) {
                tracing::debug!(
                    session_id,
                    turn_id,
                    registered_turns = registered.len(),
                    holder_session_id = %occupied.session_id,
                    holder_registration = occupied.registration,
                    outcome = "denied",
                    event = "managed_turn.admission",
                    "managed turn denied: turn id already registered"
                );
                return Err(crate::PluginError::Session(format!(
                    "turn `{turn_id}` is already running on session `{}`",
                    occupied.session_id
                )));
            }
            if let Some((holder_turn_id, holder)) = registered
                .iter()
                .find(|(_, turn)| turn.session_id == session_id)
            {
                tracing::debug!(
                    session_id,
                    turn_id,
                    registered_turns = registered.len(),
                    holder_turn_id = %holder_turn_id,
                    holder_registration = holder.registration,
                    outcome = "denied",
                    event = "managed_turn.admission",
                    "managed turn denied: session already has a running turn"
                );
                return Err(crate::PluginError::Session(format!(
                    "session `{session_id}` already has a running turn"
                )));
            }
            registered.insert(
                turn_id.to_string(),
                ManagedSessionTurn {
                    session_id: session_id.to_string(),
                    registration,
                },
            );
            tracing::debug!(
                session_id,
                turn_id,
                registration,
                registered_turns = registered.len(),
                outcome = "admitted",
                event = "managed_turn.admission",
                "managed turn admitted"
            );
        }
        Ok(Self {
            turns: Arc::clone(turns),
            live_usage: Arc::clone(live_usage),
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            registration,
            released: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Flag observed by this turn's live-usage forwarder. Set before the entries
    /// are removed, so any concurrent emit either reported before release or
    /// drops its report instead of resurrecting the entry.
    fn released_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.released)
    }

    /// Release this registration and take the live usage reported for it.
    fn release(&mut self, reason: &'static str) -> TokenUsage {
        if self.released.swap(true, Ordering::AcqRel) {
            return TokenUsage::default();
        }
        let owned = {
            let mut registered = lock_turns(&self.turns);
            let owned = registered
                .get(&self.turn_id)
                .is_some_and(|turn| turn.registration == self.registration);
            if owned {
                registered.remove(&self.turn_id);
            }
            owned
        };
        if !owned {
            // A successor owns this id pair now. Releasing either entry would
            // evict live work, so this lease releases nothing.
            tracing::debug!(
                session_id = %self.session_id,
                turn_id = %self.turn_id,
                registration = self.registration,
                reason,
                outcome = "superseded",
                event = "managed_turn.release",
                "managed turn release skipped: registration was superseded"
            );
            return TokenUsage::default();
        }
        let live_usage = self
            .live_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.turn_id)
            .unwrap_or_default();
        tracing::debug!(
            session_id = %self.session_id,
            turn_id = %self.turn_id,
            registration = self.registration,
            reason,
            live_usage_input_tokens = live_usage.input_tokens,
            live_usage_output_tokens = live_usage.output_tokens,
            outcome = "released",
            event = "managed_turn.release",
            "managed turn registration released"
        );
        live_usage
    }

    fn complete(mut self) -> TokenUsage {
        self.release("completed")
    }
}

impl Drop for ManagedTurnLease {
    fn drop(&mut self) {
        self.release("dropped");
    }
}

/// Poison-tolerant lock for the active-turn registry. `Drop` runs this on the
/// unwind path of a panicking child turn (`turns.rs` resumes that panic), so a
/// poisoned mutex must not turn one panic into a double-panic abort. Nothing is
/// ever left half-written under this lock: every critical section is a single
/// map operation.
fn lock_turns(
    turns: &ManagedTurnRegistry,
) -> std::sync::MutexGuard<'_, HashMap<String, ManagedSessionTurn>> {
    turns
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// The ABA a `(session_id, turn_id)` check cannot see: the stale lease's own
    /// id pair is registered again by a successor, so identity has to come from
    /// the registration nonce. Reproduces the interleaving found in review —
    /// a foreign turn takes the same turn id, completes, the original session
    /// re-registers, and only then does the stale lease drop.
    #[test]
    fn stale_managed_turn_lease_does_not_evict_a_same_identity_successor() {
        let (turns, live_usage) = shared_maps();
        let stale =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");
        // Whatever removes the stale registration — here a foreign turn that
        // took the id and completed — leaves `stale` alive with a released slot.
        lock_turns(&turns).remove("turn");
        let successor = ManagedTurnLease::register(&turns, &live_usage, "session", "turn")
            .expect("re-register");
        live_usage.lock().expect("live usage").insert(
            "turn".to_string(),
            TokenUsage {
                input_tokens: 7,
                ..TokenUsage::default()
            },
        );

        drop(stale);

        assert_eq!(
            lock_turns(&turns).get("turn").map(|turn| turn.registration),
            Some(successor.registration),
            "a stale lease must not release the successor's registration"
        );
        assert_eq!(
            live_usage
                .lock()
                .expect("live usage")
                .get("turn")
                .map(|usage| usage.input_tokens),
            Some(7),
            "a stale lease must not take the successor's live usage"
        );

        // The successor still owns both entries and releases them itself.
        assert_eq!(successor.complete().input_tokens, 7);
        assert!(lock_turns(&turns).is_empty());
        assert!(live_usage.lock().expect("live usage").is_empty());
    }

    #[test]
    fn managed_turn_lease_rejects_a_second_turn_on_the_same_turn_id() {
        let (turns, live_usage) = shared_maps();
        let _lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");

        // Live usage is keyed by turn id alone, so a second session may not
        // register the same turn id even though it has no turn of its own.
        let err = match ManagedTurnLease::register(&turns, &live_usage, "other-session", "turn") {
            Ok(_) => panic!("a turn id identifies at most one live managed turn"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("turn `turn` is already running on session `session`"),
            "unexpected denial: {err}"
        );
        assert_eq!(
            lock_turns(&turns)
                .get("turn")
                .map(|turn| turn.session_id.clone()),
            Some("session".to_string()),
            "a denied registration must not overwrite the live one"
        );
    }

    /// A child turn's usage forwarder can still be mid-emit when the lease
    /// releases on the cancel path (`Drop` cannot await the event drain the
    /// completion path uses). Such a late emit must be dropped: reporting it
    /// would resurrect the released live-usage entry with a zero baseline and
    /// re-record the full cumulative into the shared ledger.
    #[tokio::test]
    async fn live_usage_emitted_after_release_neither_over_counts_nor_leaks() {
        let (turns, live_usage) = shared_maps();
        let ledger = Arc::new(StdMutex::new(Vec::new()));
        let lease =
            ManagedTurnLease::register(&turns, &live_usage, "session", "turn").expect("register");
        let forwarder = LiveChildUsageForwarder {
            turn_id: "turn".to_string(),
            session_id: "session".to_string(),
            source: "child".to_string(),
            model: "mock-model".to_string(),
            token_ledger: Arc::clone(&ledger),
            child_turn_live_usage: Arc::clone(&live_usage),
            relay: None,
            turn_released: lease.released_flag(),
        };
        let cumulative = TokenUsage {
            input_tokens: 5,
            output_tokens: 1,
            ..TokenUsage::default()
        };
        forwarder
            .relay_token_usage_for_test(0, &cumulative, &cumulative)
            .await;
        assert_eq!(ledger_input_tokens(&ledger), 5, "live usage is reported");

        // The cancellation: the lease releases while an emit is still in flight.
        drop(lease);

        forwarder
            .relay_token_usage_for_test(1, &cumulative, &cumulative)
            .await;

        assert_eq!(
            ledger_input_tokens(&ledger),
            5,
            "an emit after release must not re-record the cumulative usage"
        );
        assert!(
            live_usage.lock().expect("live usage").is_empty(),
            "an emit after release must not resurrect the live-usage entry"
        );
    }

    fn ledger_input_tokens(ledger: &Arc<StdMutex<Vec<TokenLedgerEntry>>>) -> i64 {
        ledger
            .lock()
            .expect("ledger")
            .iter()
            .map(|entry| entry.usage.input_tokens)
            .sum()
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
