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
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop,
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
                    local_stop.immediate_token(),
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
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop: local_stop.clone(),
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
    /// * **The session drive runs it, in arrival order (FIG-3600).** The
    ///   accepted row is driven by the drive body
    ///   ([`drive_session`](crate::drive::drive_session)): work admitted ahead
    ///   of it runs first, each root under its own recorded admission, and a
    ///   root's claim takes the claimable prefix up to
    ///   [`QueuedWorkBatchingConfig::max_turn_input_claim`](crate::QueuedWorkBatchingConfig::max_turn_input_claim),
    ///   so this row may run inside an earlier input's root. The call returns
    ///   the run of the root that drove it.
    /// * **Live per-turn context stays with this caller.** `protocol_extension`
    ///   and live `TurnContext` plugin inputs are process-local and cannot be
    ///   persisted, so a worker that recovers this accepted row drives its
    ///   durable projection.
    /// * **One turn id, one admission.** The accepted input's id is derived
    ///   from the turn's acceptance address (ADR 0069 §6), so re-running the
    ///   same turn id, whether a durable engine's redrive or a host retry,
    ///   names the same row: identical words adopt it, and different words are
    ///   refused as `durable_identity_conflict`. A turn id reused after its
    ///   turn completed therefore cedes
    ///   (`accepted_turn_input_ceded`) instead of admitting a second turn. A
    ///   retry under a fresh turn id is a new turn; a caller that needs
    ///   at-most-once submission across turn ids names its own `source_key`
    ///   through [`enqueue_turn_input`](Self::enqueue_turn_input).
    /// * **A turn id is a source key.** The accepted row's `source_key` is the
    ///   turn id, so direct turns share the session's
    ///   `UNIQUE (session_id, source_key)` space with host enqueue ids: a
    ///   direct turn whose id equals an earlier `.enqueue().id(..)` adopts that
    ///   row instead of accepting a new one.
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
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
            let mut session_execution_lease = self.claim_session_execution_lease().await?;
            let result = Box::pin(self.drive_logical_turn(
                LogicalTurnStart::Input(input),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
                LogicalTurnClaims::new(Vec::new(), Vec::new()),
                &mut session_execution_lease,
                stopwatch,
            ))
            .await;
            return self
                .settle_session_execution_lease(session_execution_lease.as_ref(), result)
                .await;
        };

        if let Some(trace_turn_id) = input.trace_turn_id.as_ref()
            && opts
                .scoped_effect_controller()
                .execution_scope()
                .validates_turn_trace_id()
            && trace_turn_id.as_str() != opts.execution_scope_id()
        {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                format!(
                    "input trace_turn_id `{trace_turn_id}` does not match execution scope id `{}`",
                    opts.execution_scope_id()
                ),
            ));
        }
        // FIG-3619: a session whose state generation this build cannot run
        // is refused before its input becomes admission evidence.
        store
            .read_session_state_version()
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        // The turn id names the root the accepted input starts: it is the
        // row's host id, so the drive runs the turn under this id (ADR 0069
        // §6, FIG-3600 ruling Q4).
        let trace_turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        input.trace_turn_id = Some(trace_turn_id.clone());
        // Acceptance is journaled, not written directly: it happens before the
        // turn runs, which puts it inside a durable engine's replay window, and
        // a replayed handler must re-derive this admission rather than mint a
        // second one (ADR 0069 §6).
        let scoped_effect_controller = opts.scoped_effect_controller();
        let acceptance_invocation = super::causal::turn_acceptance_effect_invocation(
            scoped_effect_controller.execution_scope(),
            &self.state.session_id,
            &trace_turn_id,
        );
        let accepted = scoped_effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    acceptance_invocation.clone(),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        // The id is provisioned from the acceptance address
                        // before the body runs, so a body re-run because its
                        // outcome was never recorded names the row the first
                        // run wrote and the store adopts it (ADR 0069 §6). The
                        // source key is the turn id: the drive runs the row
                        // under it.
                        draft: Box::new(
                            crate::PendingTurnInputDraft::new(
                                self.state.session_id.clone(),
                                crate::TurnInputIngress::next_turn(),
                                input.durable_projection(),
                            )
                            .with_input_id(super::turn_input_ingress::provisioned_turn_input_id(
                                acceptance_invocation.address(),
                            ))
                            .with_source_key(trace_turn_id.as_str()),
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

        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        // The accepted row is driven by the session drive, in arrival order:
        // any root admitted ahead of it runs first, and the drive stops once
        // the root that drove this row has run. The request is named by the
        // turn, so a redrive of the turn replays the same admissions.
        let request = crate::engine::DriveRequest {
            session: self.state.session_id.clone(),
            request: crate::engine::DriveRequestId::new(format!("turn:{trace_turn_id}")),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let sinks = crate::runtime::drive::DriveSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
        };
        let accepted_id = accepted.input_id.clone();
        // A follow-on the head owes is recovered by the session's drive, not
        // by a direct turn: this drive stops where admission names it, and
        // the accepted row waits behind it (ADR 0101 §3, FIG-3542).
        let crate::runtime::drive::DriveRun {
            outcome,
            runs,
            declined_follow_on,
        } = Box::pin(self.drive_until(
            &scoped_effect_controller,
            &request,
            &sinks,
            Some((&accepted_id, &input)),
            crate::runtime::drive::FollowOnRecovery::Decline,
            |run| run.driven_inputs.contains(&accepted_id),
        ))
        .await
        .map_err(|abort| aborted(abort.into_error()))?;
        let Some(mut run) = runs
            .into_iter()
            .find(|run| run.driven_inputs.contains(&accepted_id))
            .and_then(|run| run.run)
        else {
            if let crate::engine::DriveStop::Parked(park) = &outcome.stop {
                // A parked root holds the session: the input stays accepted
                // and is driven once the park is resolved.
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::QueuedRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits behind parked root `{}` \
                         (park {}); it is driven once that park is resolved",
                        park.root, park.park
                    ),
                )));
            }
            // A follow-on the head owes blocks every other claim (ADR 0101
            // §3, FIG-3542), so the accepted row stays pending behind it: no
            // turn runs, and the call reports that as an outcome. The drive
            // that recovers the follow-on answers the row after it.
            if let Some(ahead) = Box::pin(self.queued_behind_pending_follow_on(
                &store,
                &accepted_id,
                declined_follow_on,
            ))
            .await
            .map_err(aborted)?
            {
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
            return Err(aborted(RuntimeError::new(
                RuntimeErrorCode::AcceptedTurnInputCeded,
                format!(
                    "accepted turn input `{accepted_id}` was no longer open when the drive \
                     reached it: another driver settled it or the host cancelled it"
                ),
            )));
        };
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

    /// How many accepted rows wait ahead of `accepted_id` when a follow-on
    /// held it back, or `None` when nothing did: the row is no longer
    /// pending, or no follow-on was owed. A follow-on held it back when the
    /// drive `declined` the recovery its admission named, or when the
    /// refreshed head still owes one.
    async fn queued_behind_pending_follow_on(
        &self,
        store: &Arc<dyn crate::store::RuntimePersistence>,
        accepted_id: &crate::InputId,
        declined: bool,
    ) -> Result<Option<u64>, RuntimeError> {
        if !declined && self.state.pending_follow_on.is_none() {
            return Ok(None);
        }
        let open = store
            .list_pending_turn_inputs(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(own) = open.iter().find(|read| read.input.input_id == *accepted_id) else {
            return Ok(None);
        };
        if !matches!(own.status, crate::PendingTurnInputReadStatus::Pending) {
            return Ok(None);
        }
        // A row bound to an aborted turn waits for that turn's redrive, not
        // for the drain, so it is not ahead.
        let ahead = open
            .iter()
            .filter(|earlier| {
                earlier.input.state == crate::TurnInputState::DeferredNextTurn
                    && earlier.input.enqueue_seq < own.input.enqueue_seq
                    && !matches!(
                        earlier.status,
                        crate::PendingTurnInputReadStatus::TurnBound { .. }
                    )
            })
            .count();
        Ok(Some(u64::try_from(ahead).unwrap_or(u64::MAX)))
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
        let local_stop = LocalTurnStop::from_token(cancel, None);
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
            local_stop,
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
        .engine_execution_id()
        .map(str::to_owned)
    }

    fn correlated_input() -> TurnInput {
        let process_id = crate::ProcessId::fixture("process:subagent:call");
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
            &crate::ExecutionScope::process(crate::ProcessId::fixture("process:subagent:call")),
        );

        assert_eq!(
            invocation_id(&input).as_deref(),
            Some("invocation:subagent:call")
        );
    }
}
