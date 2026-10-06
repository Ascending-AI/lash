//! Turn ingress for a child session's in-process turn: accepting it as durable
//! admission evidence and executing the run that admits the row it just wrote
//! (ADR 0069).
//!
//! Everything here runs before the prepare phase and hands it the admitted
//! rows, the shift fence, and the input the admission materialized.

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
    pub(in crate::runtime) async fn stream_turn_with_scoped_effect_controller_inner(
        &mut self,
        context: TurnPrepareContext<'_, '_>,
    ) -> Result<PhysicalTurnExecution, RuntimeError> {
        let TurnPrepareContext {
            run: std::marker::PhantomData,
            mut input,
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop,
            admissions,
            materialize_initial_admissions,
            shift_fence,
        } = context;
        if input.trace_turn_id.is_none() {
            input.trace_turn_id = Some(TurnId::parse(scoped_effect_controller.scope_id())?);
        }
        // The scope identifies the authority that admitted this run. Physical
        // turn ids remain separate routing and trace attribution, including for
        // queued drains, runtime operations, processes, and follow-on frames.
        // Re-scoping a borrowed or shared controller would change only the
        // outer address while leaving the host's inner fence on the admitted
        // scope, so preserve the controller unchanged for the complete run.
        // The stable execution-scope turn id is attached to every write-ahead
        // intent before ingress, tools, plugins, or envelope normalization can
        // put bytes. Replays bind the same id; no live pending-id state is used.
        let _attachment_execution_binding =
            match self.host.core.durability.attachment_store.holder() {
                lash_core_execution::attachments::AttachmentHolder::Runtime(
                    crate::RuntimeOwner::Session(_),
                ) => Some(
                    self.host
                        .core
                        .durability
                        .attachment_store
                        .bind_execution_scoped(
                            scoped_effect_controller
                                .execution_scope()
                                .journal_identity()?,
                        )
                        .map_err(|error| {
                            RuntimeError::new(
                                crate::RuntimeErrorCode::RuntimeEffectAttachmentStore,
                                error.to_string(),
                            )
                        })?,
                ),
                _ => None,
            };
        Box::pin(self.stream_turn_inner(TurnPrepareContext {
            run: std::marker::PhantomData,
            input: input.clone(),
            sinks: TurnSinks { observer },
            scoped_effect_controller,
            local_stop: local_stop.clone(),
            admissions,
            materialize_initial_admissions,
            shift_fence,
        }))
        .await
    }

    /// Run one child session's turn inside its parent's execution, following
    /// foreground AgentFrame switches until a terminal outcome is reached.
    ///
    /// A host never executes a run: it sends an input and the engine's session
    /// shift executes it (FIG-3600). The one turn the kernel executes in process is
    /// a child session's, which runs under its parent's process or turn
    /// controller (`session_init`). The turn is still *accepted* first:
    /// `input`'s durable projection is committed as a `NextTurn` Pending Turn
    /// Input row through a journaled acceptance step
    /// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)),
    /// whose id derives from the acceptance address and whose source key is
    /// the turn id, so a replay adopts the row the first run wrote. The
    /// session shift body then runs it in arrival order, and the call returns
    /// the execution of the run that drove it, with the acceptance identity on
    /// [`AgentFrameRun::acceptance`] and on the admitted turn's
    /// [`AssembledTurn::turn_input_acceptance`]. The live `TurnContext`
    /// (the parent's process correlation and lineage) cannot be persisted, so
    /// it is re-attached when the run's admission executes the row.
    ///
    /// A store-less runtime has no store to accept into and executes `input`
    /// directly.
    pub(crate) async fn stream_turn_with_agent_frames(
        &mut self,
        mut input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        clear_process_invocation_correlation_for_ordinary_turn(
            &mut input,
            opts.scoped_effect_controller().execution_scope(),
        );
        let Some(store) = self.services.store.clone() else {
            let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
            return Box::pin(self.execute_logical_turn(
                LogicalTurnStart::Input(input),
                opts.events_or_noop(),
                opts.turn_events_or_noop(),
                opts.scoped_effect_controller(),
                opts.local_stop().clone(),
                LogicalTurnAdmissions::new(Vec::new(), Vec::new()),
                None,
                stopwatch,
            ))
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
        // The turn id names the run the accepted input starts: it is the
        // row's host id, so the shift runs the turn under this id (ADR 0069
        // §6, FIG-3600 ruling Q4).
        let trace_turn_id = match input.trace_turn_id.clone() {
            Some(trace_turn_id) => trace_turn_id,
            None => TurnId::parse(opts.execution_scope_id())?,
        };
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
            .shift_effect(
                crate::RuntimeEffectEnvelope::new(
                    acceptance_invocation.clone(),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        // The id is provisioned from the acceptance address
                        // before the body runs, so a body re-run because its
                        // outcome was never recorded names the row the first
                        // run wrote and the store adopts it (ADR 0069 §6). The
                        // source key is the turn id: the shift runs the row
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
                // This call's own shift, below, is the ask the accepted
                // row's ingress obligation owes: the acceptance holds the
                // row's claim, so no relay pass asks the session for a second
                // shift of it before this one admits it (FIG-4728).
                crate::RuntimeEffectLocalExecutor::turn_acceptance(
                    Arc::clone(store.store()) as Arc<dyn crate::TurnInputStore>,
                    self.ingress_relay().claim_ttl_ms(),
                ),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        // From here the input is durably accepted, so every abort names it: the
        // host withdraws or redrives the input by this receipt (FIG-3575).
        let aborted = |err: RuntimeError| err.with_turn_input_acceptance(acceptance.clone());
        self.host
            .core
            .tracing
            .turn_execution(&scoped_effect_controller)
            .observe(|| {
                (
                    lash_trace::TraceContext::default()
                        .for_session(self.state.session_id.clone())
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        .for_turn_index(self.state.turn_index + 1)
                        .for_turn(trace_turn_id.clone()),
                    lash_trace::TraceEvent::Custom {
                        name: "turn_input.accepted".to_string(),
                        payload: serde_json::json!({ "input_id": &accepted.input_id }),
                    },
                )
            });

        // The accepted row is executed by the session shift, in arrival order:
        // any run admitted ahead of it runs first, and the shift stops once
        // the run that drove this row has run. The request is named by the
        // turn, so a redrive of the turn replays the same admissions.
        let request = crate::engine::ShiftRequest {
            session: self.state.session_id.clone(),
            request: crate::engine::ShiftRequestId::new(format!("turn:{trace_turn_id}")),
            intended_lane: None,
        };
        let sinks = crate::runtime::shift::ShiftSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
            settled: &crate::runtime::shift::NoopRunSettledSink,
        };
        let accepted_id = accepted.input_id.clone();
        // A follow-on the head owes is recovered by the session's shift, not
        // by a direct turn: this shift stops where admission names it, and
        // the accepted row waits behind it (ADR 0101 §3, FIG-3542).
        let crate::runtime::shift::ShiftLoopEnd {
            outcome,
            runs,
            declined_follow_on,
            ..
        } = Box::pin(self.work_until(
            &scoped_effect_controller,
            &request,
            &sinks,
            Some((&accepted_id, &input)),
            crate::runtime::shift::ShiftLimits {
                follow_on: crate::runtime::shift::FollowOnRecovery::Decline,
                max_runs: None,
                acceptor: true,
            },
            |run| run.executed_inputs.contains(&accepted_id),
        ))
        .await
        .map_err(|abort| aborted(abort.into_error()))?;
        let Some(mut run) = runs
            .into_iter()
            .find(|run| run.executed_inputs.contains(&accepted_id))
            .and_then(|run| run.run)
        else {
            if let crate::engine::ShiftStop::Parked(park) = &outcome.stop {
                // A parked run holds the session: the input stays accepted
                // and is executed once the park is resolved.
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::SessionRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits behind parked run `{}` \
                         (park {}); it is executed once that park is resolved",
                        park.run, park.park
                    ),
                )));
            }
            // A follow-on the head owes blocks every other admission (ADR 0101
            // §3, FIG-3542), so the accepted row stays pending behind it: no
            // turn runs. The shift that recovers the follow-on answers the row
            // after it; a send's handle waits for that (FIG-3600).
            if let Some(ahead) = Box::pin(self.queued_behind_pending_follow_on(
                &store,
                &accepted_id,
                declined_follow_on,
            ))
            .await
            .map_err(aborted)?
            {
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::SessionRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits behind the follow-on the \
                         session head owes, with {ahead} earlier inputs ahead of it; the shift \
                         that recovers the follow-on answers it"
                    ),
                )));
            }
            // Another execution executes the input, or drove it: the run
            // that took it answers it (FIG-4814).
            let superseded = matches!(
                outcome.ran.last(),
                Some(crate::engine::RunOutcome::Refused { .. })
            );
            let mut run = Box::pin(self.adopt_recorded_outcome(&store, &accepted_id, superseded))
                .await
                .map_err(aborted)?;
            if let Some(adopted) = run.turns.first_mut() {
                adopted.turn_input_acceptance = Some(acceptance.clone());
            }
            run.acceptance = Some(acceptance);
            return Ok(run);
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

    /// The run of accepted input `accepted_id` as its run's recorded
    /// executor left it, for an acceptor whose own shift did not run that
    /// run (FIG-4814).
    ///
    /// A run's recorded executor decides who runs it, so an acceptor that
    /// lost its ingress claim to a relay pass is refused the run and waits
    /// for it. Once the run has ended, its terminal evidence is the answer
    /// of every input it took: a committed run answers its committed
    /// outcome on the durable head, honest and thin, as the facade's durable
    /// report does, and a refused run its refusal. While the run has not
    /// ended, or the admission that `superseded` this shift's seal has yet
    /// to take the input, the acceptor fails retryably and its engine's
    /// retry asks again. An input no run took and no admission is about to
    /// take was cancelled, and the acceptor cedes it. Nothing here reads a
    /// pending row: the run's evidence is the answer of record.
    async fn adopt_recorded_outcome(
        &mut self,
        store: &crate::store::SessionStore,
        accepted_id: &crate::InputId,
        superseded: bool,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let run = store
            .run_of_input(accepted_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(run) = run else {
            // No run took the input. A shift whose seal another admission
            // superseded left it to that admission's run; any other shift
            // found it no longer open.
            return Err(if superseded {
                RuntimeError::new(
                    RuntimeErrorCode::SessionRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits for the admission that \
                         superseded this shift's; the run that takes it answers it"
                    ),
                )
            } else {
                RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{accepted_id}` was no longer open when the shift \
                         reached it: the host cancelled it"
                    ),
                )
            });
        };
        let terminal = store
            .run_terminal(&run)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let outcome = match terminal.map(|terminal| terminal.cause) {
            None => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::SessionRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` is executed by run `{run}` under \
                         its recorded executor; that run's end answers it"
                    ),
                ));
            }
            Some(crate::store::RunTerminalCause::Committed { outcome, .. }) => {
                crate::TurnOutcome::from(outcome)
            }
            Some(crate::store::RunTerminalCause::Refused {
                code,
                message,
                refusal_cause,
            }) => {
                let mut refusal = RuntimeError::new(code, message);
                refusal.cause = refusal_cause;
                return Err(refusal);
            }
            Some(cause) => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{accepted_id}` was taken by run `{run}`, which \
                         ended without an outcome: {cause:?}"
                    ),
                ));
            }
        };
        // The run committed on another runtime: answer on the durable head.
        self.refresh_resident_head().await?;
        let text = match &outcome {
            crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { text }) => {
                Some(text.clone())
            }
            _ => None,
        };
        Ok(AgentFrameRun {
            turns: vec![AssembledTurn {
                state: self.export_state(),
                outcome,
                assistant_output: crate::AssistantOutput {
                    state: if text.is_some() {
                        crate::OutputState::Usable
                    } else {
                        crate::OutputState::EmptyOutput
                    },
                    safe_text: text.clone().unwrap_or_default(),
                    raw_text: text.unwrap_or_default(),
                },
                execution: Default::default(),
                token_usage: Default::default(),
                llm_calls: Vec::new(),
                tool_calls: Vec::new(),
                omitted: None,
                retained_outputs: Vec::new(),
                failure_evidence: Vec::new(),
                errors: Vec::new(),
                turn_input_acceptance: None,
                turn_cancel_input_outcome: Default::default(),
            }],
            acceptance: None,
        })
    }

    /// How many accepted rows wait ahead of `accepted_id` when a follow-on
    /// held it back, or `None` when nothing did: the row is no longer
    /// pending, or no follow-on was owed. A follow-on held it back when the
    /// shift `declined` the recovery its admission named, or when the
    /// refreshed head still owes one.
    async fn queued_behind_pending_follow_on(
        &self,
        store: &crate::store::SessionStore,
        accepted_id: &crate::InputId,
        declined: bool,
    ) -> Result<Option<u64>, RuntimeError> {
        if !declined && self.state.pending_follow_on.is_none() {
            return Ok(None);
        }
        let open = store
            .list_pending_turn_inputs()
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(own) = open.iter().find(|read| read.input.input_id == *accepted_id) else {
            return Ok(None);
        };
        if !matches!(own.status, crate::PendingTurnInputReadStatus::Open) {
            return Ok(None);
        }
        // The owed follow-on is the turn that runs next; input addressed to
        // it waits for its checkpoints, not for a next turn.
        let running = self
            .state
            .pending_follow_on
            .as_ref()
            .map(|owed| &owed.follow_on_turn_id);
        let ahead = open
            .iter()
            .filter(|earlier| {
                earlier.input.state.is_next_turn_input(running)
                    && earlier.input.enqueue_seq < own.input.enqueue_seq
            })
            .count();
        Ok(Some(u64::try_from(ahead).unwrap_or(u64::MAX)))
    }
}

#[cfg(test)]
mod process_invocation_correlation_tests {
    use super::*;

    fn invocation_id(input: &TurnInput) -> Option<String> {
        crate::testing::TestExecutionContextBuilder::over_controller(
            crate::ActorContext::unavailable(),
        )
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
