//! Turn ingress: accepting a host turn as durable admission evidence and
//! claiming the row it just wrote (ADR 0069).
//!
//! Everything here runs before the prepare phase and hands it a claim, an
//! execution lane, and the input the claim actually materialized.

use super::*;
use crate::TurnId;

fn clear_process_invocation_correlation_for_ordinary_turn(
    input: &mut TurnInput,
    execution_scope: &crate::ExecutionScope,
) {
    if !matches!(execution_scope, crate::ExecutionScope::Process { .. }) {
        lash_core_execution::core_internal::clear_process_invocation_correlation(
            &mut input.turn_context,
        );
    }
}

impl LashRuntime {
    /// Accept `input` as durable admission evidence, then drive it to a terminal turn ([ADR
    /// 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
    ///
    /// Identical to [`stream_turn_with_agent_frames`](Self::stream_turn_with_agent_frames)
    /// except that it returns only the run's terminal physical turn.
    #[expect(
        clippy::expect_used,
        reason = "a logical turn always ends in a physical turn"
    )]
    pub async fn stream_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.stream_turn_with_agent_frames(input, opts)
            .await
            .map(|run| {
                run.into_final_turn()
                    .expect("logical turn always contains a terminal physical turn")
            })
    }

    pub(in crate::runtime) async fn stream_turn_with_scoped_effect_controller_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            mut input,
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel,
            queued_claims,
            turn_input_claims,
            materialize_initial_claims,
            lease:
                TurnLeaseScope {
                    guard: session_execution_lease,
                    release_policy: session_execution_lease_release_policy,
                },
        } = context;
        if queued_claims.is_empty()
            && turn_input_claims.is_empty()
            && let Some(lease) = session_execution_lease
        {
            while self
                .drain_next_session_command_with_cancellation(
                    &lease.fence(),
                    cancel.clone(),
                    scoped_effect_controller.controller(),
                )
                .await?
                .is_some()
            {}
        }
        let turn_id = input
            .trace_turn_id
            .get_or_insert_with(|| TurnId::from(scoped_effect_controller.scope_id()))
            .clone();
        // The scope identifies the authority that admitted this run. Physical
        // turn ids remain separate routing and trace attribution, including for
        // queued drains, runtime operations, processes, and follow-on frames.
        // Re-scoping a borrowed or shared controller would change only the
        // outer address while leaving the host's inner fence on the admitted
        // scope, so preserve the controller unchanged for the complete run.
        // The stable execution-scope turn id is attached to every write-ahead
        // intent before ingress, tools, plugins, or envelope normalization can
        // put bytes. Replays bind the same id; no live pending-id state is used.
        let _attachment_owner_binding = self
            .host
            .core
            .durability
            .attachment_store
            .bind_turn_scoped(turn_id);
        Box::pin(self.stream_turn_inner(TurnPrepareContext {
            input: input.clone(),
            sinks: TurnSinks {
                events,
                turn_events,
            },
            scoped_effect_controller,
            cancel: cancel.clone(),
            queued_claims,
            turn_input_claims,
            materialize_initial_claims,
            lease: TurnLeaseScope {
                guard: session_execution_lease,
                release_policy: session_execution_lease_release_policy,
            },
        }))
        .await
    }

    /// Stream one logical host turn, following foreground AgentFrame switches
    /// until a terminal outcome is reached.
    ///
    /// A protocol continuation creates a new frame in the same session. Hosts
    /// that only care about the benchmark/app answer should not need to
    /// special-case that intermediate outcome; this helper keeps driving the
    /// same session through each frame's task with the normal runtime turn
    /// guards.
    ///
    /// It is also where a turn is *accepted*: `input` becomes durable admission
    /// evidence before it is driven
    /// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
    ///
    /// This is the sole turn ingress. The call first commits `input` as a
    /// `NextTurn` Pending Turn Input row — the same admission evidence
    /// [`enqueue_turn_input`](Self::enqueue_turn_input) writes
    /// ([ADR 0010](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0010-pending-turn-input-is-admission-evidence.md))
    /// — and only then claims that row under the generation-fenced claim
    /// machinery of
    /// [ADR 0029](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0029-claims-are-generation-fenced-under-the-session-lease.md)
    /// and drives it. The acceptance identity rides back on
    /// [`AgentFrameRun::acceptance`] and on every assembled turn's
    /// [`AssembledTurn::turn_input_acceptance`].
    ///
    /// Consequences a caller must plan for:
    ///
    /// * **Durable backends pay one extra store commit per turn.** That is the
    ///   acceptance commit, and it is stated rather than gated.
    /// * **This caller is the first driver, not the owner.** Dropping the
    ///   returned future does not stop the turn; abandonment is expressed by
    ///   cancelling the accepted input, never by silence.
    /// * **The claim absorbs whatever else is already queued.** A direct turn
    ///   claims the head of the pending next-turn queue exactly as a drain
    ///   does, so inputs enqueued earlier join this turn instead of waiting for
    ///   another one. Only earlier ones: the claim window closes at this turn's
    ///   own journaled acceptance, so anything admitted after it waits for the
    ///   next turn (FIG-3078).
    /// * **An input behind a full claim is queued, not driven.** When more
    ///   earlier inputs wait than one claim absorbs
    ///   ([`QueuedWorkBatchingConfig::max_turn_input_claim`](crate::QueuedWorkBatchingConfig::max_turn_input_claim)),
    ///   the call drives nothing and succeeds with one turn whose outcome is
    ///   [`TurnOutcome::Queued`](crate::TurnOutcome::Queued), carrying the
    ///   acceptance and the number of inputs ahead; the queued-work drain
    ///   answers it in order. Retrying under a new turn id would admit it
    ///   twice; retrying under the same turn id names the same admission.
    /// * **Live per-turn context stays with this caller.** `protocol_extension`
    ///   and live `TurnContext` plugin inputs are process-local and cannot be
    ///   persisted, so a worker that recovers this accepted row drives its
    ///   durable projection.
    /// * **One turn id, one admission.** The accepted input's id is derived
    ///   from the turn's acceptance address (ADR 0069 §6), so re-running the
    ///   same turn id, whether a durable engine's redrive or a host retry,
    ///   names the same row: identical words adopt it, and different words are
    ///   refused as `durable_identity_conflict`. On the native tier a turn id
    ///   reused after its turn completed therefore cedes
    ///   (`accepted_turn_input_ceded`) instead of admitting a second turn. A
    ///   retry under a fresh turn id is a new turn; a caller that needs
    ///   at-most-once submission across turn ids names its own `source_key`
    ///   through [`enqueue_turn_input`](Self::enqueue_turn_input).
    ///
    /// A store-less runtime has no store to accept into and drives `input`
    /// directly; it is the one configuration with no durable ingress at all.
    pub async fn stream_turn_with_agent_frames(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        // FIG-3353: an enqueue-only (`PreservePersisted`) open may never run a
        // turn — refuse before the acceptance commit becomes admission
        // evidence.
        self.refuse_turn_execution_on_preserved_tool_surface()?;
        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            opts.scoped_effect_controller().execution_scope(),
        );
        use futures_util::FutureExt;

        // Keep the guard outside the unwinding body. Both streamed facade turns
        // and process-spawned child turns pass here, so their task JoinError cannot
        // surface before owner-side release has finished. Cancellation still
        // drops the guard and uses its best-effort cleanup / TTL fallback.
        let mut lease = None;
        let result = std::panic::AssertUnwindSafe(
            self.stream_turn_with_agent_frames_holding_lease(input, opts, &mut lease)
                .boxed(),
        )
        .catch_unwind()
        .await;
        match result {
            Ok(result) => result,
            Err(payload) => {
                if let Some(lease) = lease.as_ref()
                    && let Err(error) = lease.release_if_live().await
                {
                    tracing::warn!(%error, "failed to release session execution lease after turn panic");
                }
                std::panic::resume_unwind(payload)
            }
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "a store-backed turn holds its execution lease here"
    )]
    async fn stream_turn_with_agent_frames_holding_lease(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
        session_execution_lease: &mut Option<SessionExecutionLeaseGuard>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        if let Some(hint) = opts.local_cancel_origin_hint() {
            input.turn_context.set_local_cancel_origin_hint(hint);
        }
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let cancel = opts.cancel.clone();
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            *session_execution_lease = self.claim_session_execution_lease().await?;
            let scoped_effect_controller = opts.scoped_effect_controller();
            let result = Box::pin(self.drive_logical_turn(
                LogicalTurnStart::Input(input),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                scoped_effect_controller,
                cancel,
                LogicalTurnClaims::new(Vec::new(), Vec::new()),
                session_execution_lease,
                stopwatch,
            ))
            .await;
            return self
                .settle_session_execution_lease(session_execution_lease.as_ref(), result)
                .await;
        };

        // The row carries no source key, like a queued admission; its input id
        // is provisioned from the acceptance address below so every run of this
        // acceptance names the same row (ADR 0069 §6).
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        input.trace_turn_id = Some(trace_turn_id.clone());
        // Store-backed new turns acquire and admit the execution lane before
        // the acceptance effect writes mutable session payload (ADR 0077).
        *session_execution_lease = self.claim_session_execution_lease().await?;
        let activation_controller = opts.scoped_effect_controller();
        let activation_fence = session_execution_lease
            .as_ref()
            .map(SessionExecutionLeaseGuard::fence)
            .expect("a store-backed turn acquires its execution lease before activation");
        if let Err(error) = self
            .defer_orphaned_turn_inputs_before_drain(
                &store,
                &activation_fence,
                &trace_turn_id,
                &activation_controller,
            )
            .await
        {
            if let Some(lease) = session_execution_lease.as_ref() {
                let _ = lease.release_if_live().await;
            }
            return Err(error);
        }
        // Acceptance is journaled, not written directly: it happens before the
        // turn runs, which puts it inside a durable engine's replay window, and
        // a replayed handler must re-derive this admission rather than mint a
        // second one (ADR 0069 §6).
        let scoped_effect_controller = opts.scoped_effect_controller();
        let acceptance_invocation = super::causal::turn_acceptance_effect_invocation(
            scoped_effect_controller.execution_scope(),
            &self.state.session_id,
            &trace_turn_id,
            // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
            self.state.turn_index + 1,
        );
        let accepted = scoped_effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    acceptance_invocation.clone(),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        // The id is provisioned from the acceptance address
                        // before the body runs, so a body re-run because its
                        // outcome was never recorded names the row the first
                        // run wrote and the store adopts it (ADR 0069 §6).
                        draft: Box::new(
                            crate::PendingTurnInputDraft::new(
                                self.state.session_id.clone(),
                                crate::TurnInputIngress::next_turn(),
                                input.durable_projection(),
                            )
                            .with_input_id(
                                super::turn_input_ingress::provisioned_turn_input_id(
                                    acceptance_invocation.address(),
                                ),
                            ),
                        ),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::turn_acceptance(
                    Arc::clone(&store) as Arc<dyn crate::TurnInputStore>
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        // From here the input is durably accepted, so every abort names it: the
        // host withdraws or redrives the input by this receipt (FIG-3575).
        let aborted = |err: RuntimeError| err.with_turn_input_acceptance(acceptance.clone());
        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            lash_trace::TraceContext::default()
                .for_session(self.state.session_id.clone())
                // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                .for_turn_index(self.state.turn_index + 1)
                .for_turn(trace_turn_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "turn_input.accepted".to_string(),
                payload: serde_json::json!({ "input_id": &accepted.input_id }),
            },
            self.host.core.clock.as_ref(),
        );

        // The initial drive set is journaled too, exactly as checkpoint claims
        // are: a replaying engine returns the rows the first execution claimed,
        // with their settlement authority, and never reads pending rows, so
        // nothing `vacuum()` prunes can change what a replay drives (ADR 0069
        // §6). The commit then either settles those rows or finds the first
        // execution's receipt and replays it.
        let drive_fence = session_execution_lease
            .as_ref()
            .map(SessionExecutionLeaseGuard::fence)
            .expect("a store-backed turn acquires its execution lease before acceptance");
        let drive_generation = drive_fence.fencing_token;
        let drive = scoped_effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    super::causal::turn_input_drive_effect_invocation(&acceptance_invocation),
                    crate::RuntimeEffectCommand::ClaimAcceptedTurnInput {
                        input_id: accepted.input_id.clone(),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(super::initial_drive::AcceptedTurnInputDriveRunner {
                        store: Arc::clone(&store),
                        fence: drive_fence,
                        owner: self.runtime_lease_owner.clone(),
                        accepted: accepted.clone(),
                        max_inputs: self
                            .host
                            .core
                            .durability
                            .queued_work_batching
                            .max_turn_input_claim(),
                        trace: super::initial_drive::DriveTrace {
                            sink: self.host.core.tracing.trace_sink.clone(),
                            base: self.host.core.tracing.trace_context.clone(),
                            clock: Arc::clone(&self.host.core.clock),
                            session_id: self.state.session_id.clone(),
                            // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                            turn_index: self.state.turn_index + 1,
                            turn_id: trace_turn_id.clone(),
                        },
                    }),
                    None,
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input_drive)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error);
        let drive = match drive {
            Ok(crate::AcceptedTurnInputDrive::Claimed { claim }) => *claim,
            Ok(crate::AcceptedTurnInputDrive::Queued { ahead }) => {
                // No turn runs: the accepted row waits in arrival order and
                // the queued-work drain answers it. The call reports that as
                // an outcome, not a failure.
                if let Some(lease) = session_execution_lease.as_ref() {
                    let _ = lease.release_if_live().await;
                }
                let mut queued = crate::AssembledTurn {
                    state: self.export_state(),
                    outcome: crate::TurnOutcome::Queued { ahead },
                    assistant_output: crate::AssistantOutput {
                        safe_text: String::new(),
                        raw_text: String::new(),
                        state: crate::OutputState::EmptyOutput,
                    },
                    execution: crate::TurnExecutionMetrics::default(),
                    token_usage: crate::TokenUsage::default(),
                    llm_calls: Vec::new(),
                    tool_calls: Vec::new(),
                    omitted: None,
                    failure_evidence: Vec::new(),
                    errors: Vec::new(),
                    turn_input_acceptance: Some(acceptance.clone()),
                    turn_cancel_input_outcome: Default::default(),
                };
                stopwatch.stamp(&mut queued, self.host.core.clock.as_ref());
                return Ok(AgentFrameRun {
                    turns: vec![queued],
                    acceptance: Some(acceptance),
                });
            }
            Ok(crate::AcceptedTurnInputDrive::Refused { refusal }) => {
                if let Some(lease) = session_execution_lease.as_ref() {
                    let _ = lease.release_if_live().await;
                }
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    match refusal {
                        crate::AcceptedTurnInputRefusal::HeldByLiveClaim => format!(
                            "accepted turn input `{}` is held by another claim of this session's \
                             live lease generation, so this turn cannot drive it; it is \
                             answered once that claim settles or its generation turns over",
                            accepted.input_id
                        ),
                        crate::AcceptedTurnInputRefusal::SettledOrRemoved => format!(
                            "accepted turn input `{}` was no longer open when this turn tried to \
                             drive it: another driver settled it or the host cancelled it",
                            accepted.input_id
                        ),
                    },
                )));
            }
            Err(error) => {
                // The drive's body may have claimed rows before its outcome was
                // lost (a journal finalize fault, say): bind whatever the
                // accepted row's claim under this generation holds, while the
                // lease still stands (FIG-3589).
                self.bind_drive_claim_after_abort(
                    DriveClaimToBind::HeldUnder {
                        generation: drive_generation,
                    },
                    &accepted.input_id,
                    &trace_turn_id,
                )
                .await;
                if let Some(lease) = session_execution_lease.as_ref() {
                    let _ = lease.release_if_live().await;
                }
                return Err(aborted(error));
            }
        };

        // Drive the accepted rows, not the caller's copy: a claim may carry
        // inputs enqueued before this one, and dropping them would settle rows
        // whose content never reached a turn. Live per-turn state that cannot
        // cross the durable boundary is re-attached from the caller's input.
        let mut driven = drive.materialize_turn_input();
        driven.protocol_turn_options = input
            .protocol_turn_options
            .clone()
            .or(driven.protocol_turn_options);
        let bound_turn_id = trace_turn_id.clone();
        driven.trace_turn_id = Some(trace_turn_id);
        driven.protocol_extension = input.protocol_extension.clone();
        driven.turn_context = input.turn_context.clone();

        let drive_claim = drive.clone();
        // A replay carries the first execution's claim token; if another
        // driver reclaimed these rows meanwhile, the commit cedes instead of
        // dropping the settlement and answering them twice.
        self.journaled_drive_claims.insert(drive.claim_id.clone());
        let scoped_effect_controller = opts.scoped_effect_controller();
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(driven),
            opts.events_or_noop(),
            opts.turn_events_or_noop(),
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(Vec::new(), vec![drive]),
            session_execution_lease,
            stopwatch,
        ))
        .await;
        self.journaled_drive_claims.remove(&drive_claim.claim_id);
        if result.is_err() {
            // The aborted turn keeps its claim and its `Err` names the input:
            // bind the claim to the turn before the lease is released, so no
            // later generation folds the input into another turn (FIG-3589).
            self.bind_drive_claim_after_abort(
                DriveClaimToBind::Claim(&drive_claim),
                &acceptance.input_id,
                &bound_turn_id,
            )
            .await;
        }
        let mut run = self
            .settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
            .map_err(aborted)?;
        // Only the physical turn this acceptance admitted carries it. An
        // agent-frame run's follow-on turns were started by the frame switch,
        // not by this admission, and stamping them would report an acceptance
        // that never applied to them.
        if let Some(admitted) = run.turns.first_mut() {
            admitted.turn_input_acceptance = Some(acceptance.clone());
        }
        run.acceptance = Some(acceptance);
        Ok(run)
    }

    pub async fn run_turn_assembled(
        &mut self,
        input: TurnInput,
        cancel: CancellationToken,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        self.stream_turn(input, TurnOptions::new(cancel, scoped_effect_controller))
            .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "this is the published `LashRuntime::stream_prepared_turn` signature; \
                  folding these into a context struct would be a \
                  public API change, which this ticket forbids"
    )]
    #[expect(
        clippy::expect_used,
        reason = "the lease is held here and a logical turn ends in a physical turn"
    )]
    pub async fn stream_prepared_turn(
        &mut self,
        messages: crate::MessageSequence,
        previous_prompt_usage: Option<TokenUsage>,
        protocol_turn_options: Option<crate::ProtocolTurnOptions>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        initial_turn_causes: Vec<crate::TurnCause>,
        trace_turn_id: TurnId,
        turn_index: usize,
        events: &dyn EventSink,
        turn_events: &dyn TurnActivitySink,
        scoped_effect_controller: ScopedEffectController<'_>,
        cancel: CancellationToken,
        initial_queue_claim: Option<crate::QueuedWorkClaim>,
        initial_turn_input_claim: Option<crate::TurnInputClaim>,
    ) -> Result<AssembledTurn, RuntimeError> {
        // FIG-3353: queued/prepared drives are turn execution too; a
        // `PreservePersisted` open refuses before claiming the lane.
        self.refuse_turn_execution_on_preserved_tool_surface()?;
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let mut session_execution_lease = self.claim_session_execution_lease().await?;
        if let Some(store) = self.session.as_ref().and_then(Session::history_store) {
            let fence = session_execution_lease
                .as_ref()
                .map(SessionExecutionLeaseGuard::fence)
                .expect("a store-backed prepared turn acquires its execution lease");
            if let Err(error) = self
                .defer_orphaned_turn_inputs_before_drain(
                    &store,
                    &fence,
                    &trace_turn_id,
                    &scoped_effect_controller,
                )
                .await
            {
                if let Some(lease) = session_execution_lease.as_ref() {
                    let _ = lease.release_if_live().await;
                }
                return Err(error);
            }
        }
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Prepared(PreparedLogicalTurn {
                messages,
                previous_prompt_usage,
                protocol_turn_options,
                protocol_extension,
                turn_context,
                initial_turn_causes,
                trace_turn_id,
                turn_index,
            }),
            events,
            turn_events,
            scoped_effect_controller,
            cancel,
            LogicalTurnClaims::new(
                initial_queue_claim.into_iter().collect(),
                initial_turn_input_claim.into_iter().collect(),
            ),
            &mut session_execution_lease,
            stopwatch,
        ))
        .await
        .map(|run| {
            run.into_final_turn()
                .expect("logical turn always contains a terminal physical turn")
        });
        self.settle_session_execution_lease(session_execution_lease.as_ref(), result)
            .await
    }
}

#[cfg(test)]
mod process_invocation_correlation_tests {
    use super::*;

    fn invocation_id(input: &TurnInput) -> Option<String> {
        crate::testing::TestExecutionContextBuilder::over_controller(std::sync::Arc::new(
            crate::testing::UnavailableEffectController,
        )
            as std::sync::Arc<dyn crate::RuntimeEffectController>)
        .turn_context(input.turn_context.clone())
        .build()
        .into_runtime()
        .restate_invocation_id()
        .map(str::to_owned)
    }

    fn correlated_input() -> TurnInput {
        let process_id = crate::ProcessId::from("process:subagent:call");
        let authority = crate::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "invocation:subagent:call",
        )
        .bind_attempt(2);
        let mut input = TurnInput::text("run child");
        lash_core_execution::core_internal::attach_process_invocation_correlation(
            &mut input.turn_context,
            &process_id,
            &authority,
        );
        input
    }

    #[test]
    fn ordinary_turn_clears_reused_process_invocation_correlation() {
        let mut input = correlated_input();
        assert_eq!(
            invocation_id(&input).as_deref(),
            Some("invocation:subagent:call")
        );

        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            &crate::ExecutionScope::turn("session:child", "turn:follow-up"),
        );

        assert_eq!(invocation_id(&input), None);
    }

    #[test]
    fn process_turn_preserves_attached_invocation_correlation() {
        let mut input = correlated_input();

        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            &crate::ExecutionScope::process("process:subagent:call"),
        );

        assert_eq!(
            invocation_id(&input).as_deref(),
            Some("invocation:subagent:call")
        );
    }
}
