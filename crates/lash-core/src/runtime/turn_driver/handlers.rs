use super::*;
use lash_sansio::session_model::{FailureCode, TurnFailureCode};

impl RuntimeTurnDriver<'_> {
    fn handle_machine_response(
        &self,
        machine: &mut TurnMachine,
        response: Response,
    ) -> Result<(), RuntimeError> {
        machine.try_handle_response(response).map_err(|overflow| {
            crate::runtime::runtime_error_from_store_commit(
                crate::StoreError::TokenUsageAccountingOverflow {
                    usage_source: "turn".to_string(),
                    model: self
                        .policy
                        .llm_profile_config()
                        .model
                        .wire_model()
                        .to_string(),
                    counter: overflow.counter(),
                },
            )
        })
    }

    pub(super) async fn handle_llm_call_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        request: Arc<LlmRequest>,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        self.trace_before_llm_call(machine, &request);
        let invocation = self
            .turn_effect_invocation(machine, id, RuntimeEffectKind::BeforeLlmCall)
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let decision = self
            .execute_typed_turn_effect(
                machine,
                event_tx,
                RuntimeEffectEnvelope::new(
                    invocation,
                    RuntimeEffectCommand::BeforeLlmCall {
                        request: Box::new((*request).clone()),
                    },
                ),
                RuntimeEffectOutcome::into_before_llm_call,
            )
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        match decision {
            Ok(Some(crate::ProtocolLlmCallAction::SwitchAgentFrame { frame_key, task })) => {
                machine.finish_with_outcome(crate::TurnOutcome::AgentFrameSwitch {
                    frame_key,
                    task,
                    initial_nodes: Vec::new(),
                });
                return Ok(());
            }
            Ok(None) => {}
            // A protocol refusal before the model call is an outcome over the
            // turn's journaled inputs, recorded as a failed turn on every host
            // (FIG-3575). Only a live fault the hook ran into, or a session
            // retirement it met (FIG-3630), aborts.
            Err(err) => {
                let failure = err.into_turn_failure(RuntimeErrorCode::ProtocolBeforeLlmCall);
                if failure.turn_failure_cause().aborts_invocation()
                    || failure.is_session_retirement()
                {
                    return Err(failure);
                }
                machine.fail_turn(make_error_event(
                    crate::TurnFailureKind::ProtocolBeforeLlmCall,
                    Some(crate::TurnFailureCode::BeforeLlmCallFailed.into()),
                    failure.message.clone(),
                    Some(failure.message),
                ));
                return Ok(());
            }
        }
        let mut request = request;
        let degraded =
            crate::attachments::degrade_unmaterializable_request_attachments(&mut request);
        for notice in degraded {
            self.emit_trace(machine.protocol_iteration(), || {
                lash_trace::TraceEvent::AttachmentDegraded {
                    attachment_id: notice.attachment_id,
                    label: notice.label,
                    media_type: notice.media_type,
                    source: notice.source,
                    reason: notice.reason,
                }
            });
        }
        let crate::runtime::RuntimeLlmCallOutcome {
            result,
            text_streamed,
            call_record,
            stream:
                crate::runtime::LlmStreamRecord {
                    reasoning_published,
                    stream_hook_states,
                    response_plan,
                },
        } = match self
            .invoke_turn_llm_effect(machine, id, request, event_tx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        if let (Err(error), Some(record)) = (&result, call_record.as_ref()) {
            let sealed_attempt_count = self
                .llm_calls
                .iter()
                .fold(record.attempts.len(), |count, call| {
                    count.saturating_add(call.attempts.len())
                });
            if self.failure_evidence.len() < sealed_attempt_count
                && let Some(evidence) = crate::TurnFailureEvidence::from_llm_failure(error, record)
            {
                self.failure_evidence.push(evidence);
            }
        }
        if let Some(call_record) = call_record {
            self.turn_observations.observe(
                event_tx,
                crate::engine::ObservedEvent::Activity {
                    correlation_id: Some(TurnActivityId::new(call_record.call_id.0.clone())),
                    event: TurnEvent::ModelCallRecorded {
                        record: call_record.clone(),
                    },
                },
            );
            self.llm_calls.push(call_record);
        }
        // Phase 2 of the staged boundary runs only once the paid attempt is
        // journaled and on the ledger, so a failing derivation can never take
        // the record of what we bought down with it. Whether it runs at all is
        // the plan phase 1 recorded with the completion (ADR 0105 §1), never
        // the response hooks installed now: a replay after a hook was added or
        // removed issues the phases its first execution did.
        let result = match result {
            Ok(raw) if !response_plan.callbacks.is_empty() => {
                match self
                    .invoke_assistant_response_hooks_effect(
                        machine,
                        id,
                        raw,
                        response_plan,
                        stream_hook_states,
                        event_tx,
                    )
                    .await
                {
                    Ok(response) => Ok(response),
                    Err(err) => {
                        Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                        return Ok(());
                    }
                }
            }
            result => result,
        };
        let loud_provider_panic = result.as_ref().err().and_then(|error| {
            (error.code == Some(FailureCode::lash(TurnFailureCode::ProviderPanicked)))
                .then(|| error.message.clone())
        });
        if let Ok(response) = &result {
            let usage = crate::runtime::effect::token_usage_from_llm(&response.usage);
            self.latest_prompt_usage = nonzero_usage(usage);
            if !text_streamed {
                let prose_projector = self.session.plugins().assistant_prose_projector();
                emit_semantic_response_parts(
                    event_tx,
                    &mut self.turn_observations,
                    response,
                    prose_projector.as_deref(),
                    &ReasoningPublicationState::from_published_blocks(reasoning_published),
                );
            }
        }
        // Name the request that stopped the call before the machine decides a
        // cancelled terminal reason, so its outcome carries real evidence.
        if let Some(evidence) = self.turn_cancel.clone() {
            machine.record_cancellation_evidence(evidence);
        }
        self.handle_machine_response(
            machine,
            Response::LlmComplete {
                id,
                result,
                text_streamed,
            },
        )?;
        if let Some(message) = loud_provider_panic {
            crate::panic_containment::enforce_message("provider_panicked", &message);
        }
        Ok(())
    }

    pub(super) async fn handle_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let protocol_iteration = machine.protocol_iteration();
        if matches!(checkpoint, CheckpointKind::BeforeCompletion) {
            // The opener's end precedes the turn's terminal checkpoint, so
            // the facts its losers' settlements carry are delivered and
            // committed with the turn (ADR 0099 §7 step 2).
            Box::pin(self.finish_tool_run_before_completion()).await?;
        }
        let result = self
            .invoke_turn_checkpoint_effect(machine, id, checkpoint, event_tx)
            .await;
        match result {
            Ok(delivery) => {
                let committed_user_messages = delivery.committed_user_messages.clone();
                self.handle_machine_response(machine, Response::Checkpoint { id, delivery })?;
                self.turn_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: None,
                        event: TurnEvent::CheckpointRecorded { protocol_iteration },
                    },
                );
                if let Some(mut admitted) = self.pending_checkpoint_turn_inputs.take() {
                    admitted.record_checkpoint_applications(
                        &self.turn_id,
                        checkpoint,
                        &committed_user_messages,
                    );
                    let applications = admitted.applications.clone();
                    let accepted_turn_inputs = admitted.accepted_turn_inputs();
                    self.pending_turn_inputs.push(admitted);
                    send_turn_input_applications(
                        event_tx,
                        &mut self.turn_observations,
                        applications,
                    );
                    if !accepted_turn_inputs.is_empty() {
                        self.turn_observations.observe(
                            event_tx,
                            crate::engine::ObservedEvent::Session(
                                SessionStreamEvent::InjectedTurnInputAccepted {
                                    inputs: accepted_turn_inputs,
                                    checkpoint,
                                },
                            ),
                        );
                    }
                }
            }
            Err(err) => {
                // A failed checkpoint delivers nothing and starts no follow-on
                // turn. What it admitted stays bound to the run, which never
                // settles it as delivered: the run's terminal write hands it
                // back open at its own position (FIG-3927 §2.4).
                self.pending_checkpoint_turn_inputs = None;
                drop(self.withheld_terminal_work.take_if_any());
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let crate::runtime::effect::ServedExecutionEnvironmentSync {
            prelude,
            result,
            tool_surface,
        } = match self
            .invoke_turn_execution_environment_sync_effect(machine, id, event_tx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        // The journal recorded the prelude by digest; live or replayed, it
        // is read back from the store and never re-derived (FIG-5133). A
        // prelude that cannot be read ends the attempt rather than the turn:
        // the journal already holds what followed the sync, so settling the
        // turn here would diverge from it. A live fault is retried; a prelude
        // that is gone or not the recorded one refuses the run, typed.
        let prelude = prelude
            .read(self.host.core.durability.turn_prelude_store.as_ref())
            .await
            .map_err(RuntimeEffectControllerError::into_runtime_error)?;
        let providers = self
            .session
            .plugins()
            .resolve_context_tool_bindings(&prelude.context.tool_providers)
            .map_err(|error| error.into_turn_failure(RuntimeErrorCode::ContextPrepareTurn))?;
        self.session
            .set_context_overlay(providers)
            .map_err(|error| error.into_turn_failure(RuntimeErrorCode::SessionToolRegistry))?;
        machine.adopt_committed_messages(prelude.history.clone());
        machine.adopt_prepared_messages(prelude.context.messages.clone(), id.0 == 1);
        self.prelude = prelude;
        // The surface the sync recorded is the one the iteration's tool calls
        // resolve against, whether the sync ran here or was served from the
        // journal (FIG-3672 P7b). Only a sync that built an environment
        // recorded one.
        if result.is_ok() {
            let authority = &self.turn_pipeline.state().authority;
            self.session
                .install_recorded_tool_surface(
                    &authority.tool_access,
                    authority.subagent.as_ref(),
                    &tool_surface,
                )
                // The install pins the live registry and runs the catalog
                // contributors again, as the sync's step body did, so their
                // failures are classified the same way (FIG-4651): a live
                // cause aborts the attempt under its own code, and only a
                // deterministic catalog defect ends the turn as one.
                .map_err(|error| {
                    let message =
                        format!("the recorded tool surface could not be installed: {error}");
                    match super::tool_catalog::SyncFailure::of_plugin_error(
                        crate::sansio::ExecutionEnvironmentSyncFailureKind::ToolSurface,
                        error,
                    ) {
                        super::tool_catalog::SyncFailure::Live(fault) => fault,
                        super::tool_catalog::SyncFailure::Recorded(_) => RuntimeError::new(
                            RuntimeErrorCode::ToolCatalogResolutionFailed,
                            message,
                        ),
                    }
                })?;
        }
        self.handle_machine_response(machine, Response::ExecutionEnvironmentSynced { id, result })?;
        Ok(())
    }

    pub(super) async fn handle_exec_code_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        language: String,
        code: String,
        event_tx: &TurnObserver,
    ) -> Result<(), RuntimeError> {
        let code_correlation_id = TurnActivityId::new(format!("code:{id:?}"));
        let iteration = machine.protocol_iteration();
        if self.trace.is_observed() {
            self.emit_trace(iteration, || lash_trace::TraceEvent::ExecCodeStarted {
                code: code.clone(),
                code_chars: code.chars().count(),
            });
        }
        let invocation = match self.turn_effect_invocation(machine, id, RuntimeEffectKind::ExecCode)
        {
            Ok(invocation) => invocation,
            Err(err) => {
                let message = err.to_string();
                self.turn_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockStarted {
                            language: language.clone(),
                            code: code.clone(),
                            graph_key: None,
                        },
                    },
                );
                self.turn_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            output: String::new(),
                            error: Some(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                message,
                            )),
                            duration_ms: 0,
                            tool_call_ids: Vec::new(),
                            graph_key: None,
                        },
                    },
                );
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        let graph_key = Some(foreground_effect_graph_key(&invocation));
        let cell_key = invocation.effect_replay_key().to_string();
        // The cell's own observation lane: every CodeBlock* event this driver
        // publishes for the cell sequences under the cell's replay key.
        let mut code_observations = crate::engine::ObservationCursor::new(
            crate::engine::ReplayKey::new(format!("{cell_key}:code")),
        );
        code_observations.observe(
            event_tx,
            crate::engine::ObservedEvent::Activity {
                correlation_id: Some(code_correlation_id.clone()),
                event: TurnEvent::CodeBlockStarted {
                    language: language.clone(),
                    code: code.clone(),
                    graph_key: graph_key.clone(),
                },
            },
        );
        // The observed duration is measured around the journaled invocation:
        // the recorded `ExecResponse` carries no wall-clock fields, so the
        // Completed activity and its trace mirror take the live window here.
        let cell_started = self.host.core.clock.now();
        let result = match self
            .invoke_turn_exec_effect(machine, invocation, code.clone(), event_tx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                let message = err.to_string();
                // The observation-only duration is not read from the clock:
                // the shift decides nothing from it, so it reports 0 rather
                // than take a live timestamp (FIG-3672 P6b).
                code_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            output: String::new(),
                            error: Some(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                message,
                            )),
                            duration_ms: 0,
                            tool_call_ids: Vec::new(),
                            graph_key: graph_key.clone(),
                        },
                    },
                );
                // A cell that aborted stopped for the turn's cancellation only
                // when the turn recorded one.
                let cancellation_evidence = self.turn_cancel.clone();
                if let Some(code_executor) = self.session.plugins().code_executor() {
                    code_executor
                        .settle_code_execution(if cancellation_evidence.is_some() {
                            crate::plugin::CodeExecutionOutcome::Cancelled
                        } else {
                            crate::plugin::CodeExecutionOutcome::Discarded
                        })
                        .await
                        .map_err(|error| {
                            RuntimeError::new(
                                RuntimeErrorCode::ExecutionStateCaptureFailed,
                                error.to_string(),
                            )
                        })?;
                }
                // A live fault or a session retirement aborts whether or not a
                // cancel is pending; the effect loop settles a pending cancel as
                // `Stopped { Cancelled }` after the abort. Any other failure
                // under a cancel is the cancel's own consequence (FIG-3575).
                if Self::aborts_turn(&err) {
                    return Err(err.into_runtime_error());
                }
                if let Some(evidence) = cancellation_evidence {
                    machine.finish_with_outcome(crate::TurnOutcome::Stopped(TurnStop::Cancelled {
                        evidence,
                    }));
                    return Ok(());
                }
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        // A turn on the session actor takes no segment boundary: a cell that
        // stopped at one inside itself has no successor to hand its wait to.
        if result.as_ref().is_ok_and(|output| output.suspended) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "a code cell stopped at a segment boundary in a turn that takes none",
            ));
        }
        let cell_duration_ms = self
            .host
            .core
            .clock
            .now()
            .saturating_duration_since(cell_started)
            .as_millis() as u64;
        if let Ok(output) = &result {
            self.recorded_assembly.note_code_outputs(output);
        }
        match &result {
            Ok(output) => {
                code_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            output: join_observations(&output.observations),
                            error: output.error.clone(),
                            duration_ms: cell_duration_ms,
                            tool_call_ids: output
                                .calls
                                .iter()
                                .filter_map(|call| call.host_record.as_ref())
                                .map(|record| record.call_id.clone())
                                .collect(),
                            graph_key: graph_key.clone(),
                        },
                    },
                );
            }
            Err(error) => {
                code_observations.observe(
                    event_tx,
                    crate::engine::ObservedEvent::Activity {
                        correlation_id: Some(code_correlation_id.clone()),
                        event: TurnEvent::CodeBlockCompleted {
                            language: language.clone(),
                            output: String::new(),
                            error: Some(crate::CellFailure::new(
                                crate::CellFailureKind::Host,
                                error.message.clone(),
                            )),
                            duration_ms: 0,
                            tool_call_ids: Vec::new(),
                            graph_key: graph_key.clone(),
                        },
                    },
                );
            }
        }
        if let Ok(output) = &result {
            if self.trace.is_observed() {
                let observations_text = join_observations(&output.observations);
                let observation_projections = output
                    .observations
                    .iter()
                    .map(|observation| observation.projection.clone())
                    .collect::<Vec<_>>();
                let tool_calls = output
                    .calls
                    .iter()
                    .filter_map(|call| call.host_record.as_ref())
                    .map(|record| lash_trace::TraceExecToolCall {
                        call_id: record.call_id.clone(),
                        name: record.tool.clone(),
                        status: match record.output.status() {
                            lash_sansio::ToolCallStatus::Success => {
                                lash_trace::TraceToolCallStatus::Success
                            }
                            lash_sansio::ToolCallStatus::Failure => {
                                lash_trace::TraceToolCallStatus::Failure
                            }
                            lash_sansio::ToolCallStatus::Cancelled => {
                                lash_trace::TraceToolCallStatus::Cancelled
                            }
                        },
                    })
                    .collect::<Vec<_>>();
                self.emit_trace(iteration, || lash_trace::TraceEvent::ExecCodeCompleted {
                    duration_ms: cell_duration_ms,
                    output: observations_text.clone(),
                    output_chars: observations_text.chars().count(),
                    observation_count: output.observations.len(),
                    observation_projections: observation_projections.clone(),
                    error: output.error.clone(),
                    terminal_finish: output.terminal_finish.clone(),
                    tool_calls,
                });
                if !observation_projections.is_empty() {
                    self.emit_trace(iteration, || {
                        lash_trace::TraceEvent::ObservationProjection {
                            projections: observation_projections,
                        }
                    });
                }
            }
        } else if let Err(error) = &result
            && self.trace.is_observed()
        {
            self.emit_trace(iteration, || lash_trace::TraceEvent::ExecCodeFailed {
                reason: error.reason,
                error: error.message.clone(),
            });
        }
        // Name the cancellation the turn recorded, if any, before the
        // protocol classifies its typed Stop response.
        let cancellation_evidence = self.turn_cancel.clone();
        // A cancelled tool call ended the cell as an uncatchable host terminal,
        // so the execution settles as cancelled even without a host cancel
        // request. The protocol then reads the cancelled record off the call
        // ledger and finishes the turn cancelled itself.
        let tool_call_cancelled = result.as_ref().ok().is_some_and(|output| {
            output.calls.iter().any(|call| {
                call.host_record.as_ref().is_some_and(|record| {
                    record.output.status() == lash_sansio::ToolCallStatus::Cancelled
                })
            })
        });
        if let Some(code_executor) = self.session.plugins().code_executor() {
            code_executor
                .settle_code_execution(if cancellation_evidence.is_some() || tool_call_cancelled {
                    crate::plugin::CodeExecutionOutcome::Cancelled
                } else {
                    crate::plugin::CodeExecutionOutcome::Accepted
                })
                .await
                .map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::ExecutionStateCaptureFailed,
                        error.to_string(),
                    )
                })?;
        }
        if let Some(evidence) = cancellation_evidence {
            machine.record_cancellation_evidence(evidence);
        }
        self.handle_machine_response(machine, Response::ExecResult { id, result })?;
        Ok(())
    }
}

fn join_observations(observations: &[crate::Observation]) -> String {
    observations
        .iter()
        .map(|observation| observation.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn foreground_effect_graph_key(invocation: &RuntimeEffectInvocation) -> String {
    invocation.address().graph_key()
}

pub(super) fn foreground_exec_graph_key(invocation: &RuntimeInvocation) -> Option<String> {
    invocation
        .effect_address()
        .map(crate::EffectAddress::graph_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_exec_graph_key_uses_runtime_invocation_identity() {
        let effect = RuntimeInvocation::effect(
            EffectAddress::new(ExecutionScope::turn("session-1", "turn-1"), "replay-key")
                .expect("valid foreground exec address"),
            RuntimeAttribution::for_turn("session-1", "turn-1", 2, 3),
            "effect-7",
        );
        assert_eq!(
            foreground_exec_graph_key(&effect).as_deref(),
            Some(
                "effect:{\"version\":2,\"kind\":\"turn\",\"session_id\":\"session-1\",\"execution_id\":\"turn-1\"}:\"replay-key\""
            )
        );

        let process = RuntimeInvocation {
            attribution: RuntimeAttribution::for_turn("session-1", "turn-1", 2, 3),
            subject: crate::RuntimeSubject::Process {
                process_id: crate::ProcessId::fixture("process-1"),
            },
            caused_by: None,
            replay: None,
        };
        assert_eq!(foreground_exec_graph_key(&process), None);
    }
}
