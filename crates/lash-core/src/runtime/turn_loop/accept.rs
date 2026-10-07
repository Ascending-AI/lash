//! Turn ingress for a child session's turn: accepting it as durable admission
//! evidence (ADR 0069), whose transaction wakes the session actor, and
//! answering from the run that took the row it just wrote.

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
        }))
        .await
    }

    /// Run one child session's turn inside its parent's execution, following
    /// foreground AgentFrame switches until a terminal outcome is reached.
    ///
    /// A host never executes a run: it sends an input and the session actor
    /// executes it (FIG-3600). The one turn the kernel executes in process is
    /// a child session's, which runs under its parent's process or turn
    /// controller (`session_init`). The turn is still *accepted* first:
    /// `input`'s durable projection is committed as a `NextTurn` Pending Turn
    /// Input row through a journaled acceptance step
    /// ([ADR 0069](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)),
    /// whose id derives from the acceptance address and whose source key is
    /// the turn id, so a replay adopts the row the first run wrote. The
    /// session actor then runs it in arrival order, and the call returns
    /// the execution of the run that took it, with the acceptance identity on
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
        // row's host id, so the session runs the turn under this id (ADR 0069
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
            .ingress_effect(
                crate::RuntimeEffectEnvelope::new(
                    acceptance_invocation.clone(),
                    crate::RuntimeEffectCommand::AcceptTurnInput {
                        // The id is provisioned from the acceptance address
                        // before the body runs, so a body re-run because its
                        // outcome was never recorded names the row the first
                        // run wrote and the store adopts it (ADR 0069 §6). The
                        // source key is the turn id: the session runs the row
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
                // The acceptance's transaction wakes the session actor,
                // which admits and runs the row (ADR 0132 §12).
                crate::RuntimeEffectLocalExecutor::turn_acceptance(
                    Arc::clone(store.store()) as Arc<dyn crate::TurnInputStore>
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

        // The session actor runs the accepted row, in arrival order; the run
        // that took it answers it (FIG-4814). Until that run has ended the
        // acceptor fails retryably, and the input stays accepted.
        let mut run = Box::pin(self.adopt_recorded_outcome(&store, &accepted.input_id))
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

    /// The run of accepted input `accepted_id` as the session actor's run of
    /// it left it (FIG-4814).
    ///
    /// Once the run has ended, its terminal evidence is the answer of every
    /// input it took: a committed run answers its committed outcome on the
    /// durable head, honest and thin, as the facade's durable report does,
    /// and a refused run its refusal. While no run has taken the open input,
    /// or its run has not ended, the acceptor fails retryably and asks
    /// again. An input no run took that is no longer open was cancelled, and
    /// the acceptor cedes it.
    async fn adopt_recorded_outcome(
        &mut self,
        store: &crate::store::SessionStore,
        accepted_id: &crate::InputId,
    ) -> Result<AgentFrameRun, RuntimeError> {
        let run = store
            .run_of_input(accepted_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        let Some(run) = run else {
            let open = store
                .list_pending_turn_inputs()
                .await
                .map_err(super::runtime_error_from_store_commit)?
                .iter()
                .any(|read| read.input.input_id == *accepted_id);
            return Err(if open {
                RuntimeError::new(
                    RuntimeErrorCode::SessionRunPending,
                    format!(
                        "accepted turn input `{accepted_id}` waits for the session to admit it; \
                         the run that takes it answers it"
                    ),
                )
            } else {
                RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{accepted_id}` is no longer open and no run took \
                         it: the host cancelled it"
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
                        "accepted turn input `{accepted_id}` is executed by run `{run}`; that \
                         run's end answers it"
                    ),
                ));
            }
            Some(crate::store::RunTerminalCause::Committed { outcome, .. }) => {
                crate::TurnOutcome::from(outcome)
            }
            Some(crate::store::RunTerminalCause::Cancelled { evidence }) => {
                crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence })
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
