use super::*;

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
                    model: self.policy.model.id.clone(),
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        match self.before_llm_call(machine, &request).await {
            Ok(Some(crate::ProtocolLlmCallAction::SwitchAgentFrame { frame_id, task })) => {
                machine.finish_with_outcome(crate::TurnOutcome::AgentFrameSwitch {
                    frame_id,
                    task,
                    initial_nodes: Vec::new(),
                });
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => {
                let err_string = err.to_string();
                if self.should_abort_for_runtime_effect_error() {
                    return Err(RuntimeError::new(
                        RuntimeErrorCode::ProtocolBeforeLlmCall,
                        err_string,
                    ));
                }
                machine.fail_turn(make_error_event(
                    "protocol_before_llm_call",
                    Some("before_llm_call_failed"),
                    err_string.clone(),
                    Some(err_string),
                ));
                return Ok(());
            }
        }
        let (result, text_streamed, call_record) = match self
            .invoke_turn_llm_effect(machine, id, request, event_tx, cancel)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        if let Some(call_record) = call_record {
            self.llm_calls.push(call_record);
        }
        // FIG-793: the LLM run is the deployed first journal command for this
        // protocol iteration, so it must be emitted and awaited before any
        // cancellation observation is registered. Restate SDK 0.10 emits a
        // `ctx.run` command only when its future is polled and requires that
        // future to be awaited immediately; it cannot honestly be selected
        // away from mid-flight. The durable contract is therefore cancellation
        // between iterations. A local provider may still cooperatively observe
        // `cancel` while the run is executing, and that result is journaled.
        // Runtime-owned (inline) execution keeps its existing cooperative-token
        // behavior. Only a controller-owned journal needs this additional
        // durable, replayed boundary.
        if self.observes_durable_cancel_after_llm {
            let pending_cancel = self
                .turn_control
                .observe_pending_cancel(
                    self.scoped_effect_controller.controller(),
                    crate::runtime::turn_control::TurnCancelPeekIdentity::AfterLlm {
                        protocol_iteration: machine.protocol_iteration(),
                    },
                )
                .await?;
            if pending_cancel.is_some() {
                cancel.cancel();
                send_session_event(event_tx, SessionStreamEvent::Done).await;
                machine.finish_with_outcome(crate::TurnOutcome::Stopped(TurnStop::Cancelled));
                return Ok(());
            }
        }
        if let Ok(response) = &result {
            let usage = crate::runtime::effect::token_usage_from_llm(&response.usage);
            self.turn_pipeline.state_mut().last_prompt_usage = normalize_prompt_usage(&usage);
            if !text_streamed {
                let prose_projector = self.session.plugins().assistant_prose_projector();
                emit_semantic_response_parts(event_tx, response, prose_projector.as_deref()).await;
            }
        }
        self.handle_machine_response(
            machine,
            Response::LlmComplete {
                id,
                result,
                text_streamed,
            },
        )?;
        Ok(())
    }

    pub(super) async fn handle_checkpoint_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        checkpoint: CheckpointKind,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let result = self
            .invoke_turn_checkpoint_effect(machine, id, checkpoint, event_tx, cancel)
            .await;
        match result {
            Ok(delivery) => {
                let committed_user_messages = delivery.committed_user_messages.clone();
                self.handle_machine_response(machine, Response::Checkpoint { id, delivery })?;
                if let Some(mut claim) = self.pending_checkpoint_turn_input_claim.take() {
                    claim.record_checkpoint_applications(
                        &self.turn_id,
                        checkpoint,
                        &committed_user_messages,
                    );
                    let applications = claim.applications.clone();
                    let accepted_turn_inputs = claim.accepted_turn_inputs();
                    self.pending_turn_input_claims.push(claim);
                    send_turn_input_applications(event_tx, applications).await;
                    if !accepted_turn_inputs.is_empty() {
                        send_session_event(
                            event_tx,
                            SessionStreamEvent::InjectedTurnInputAccepted {
                                inputs: accepted_turn_inputs,
                                checkpoint,
                            },
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                if let Some(claim) = self.pending_checkpoint_turn_input_claim.take()
                    && let Some(store) = self.session.history_store()
                {
                    store
                        .abandon_turn_input_claim(&claim)
                        .await
                        .map_err(crate::runtime::runtime_error_from_store_commit)?;
                }
                self.fail_or_abort_runtime_effect_controller(machine, err.into())?;
            }
        }
        Ok(())
    }

    pub(super) async fn handle_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        update_machine_config: bool,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let result = match self
            .invoke_turn_execution_environment_sync_effect(
                machine,
                id,
                update_machine_config,
                event_tx,
                cancel,
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        self.handle_machine_response(machine, Response::ExecutionEnvironmentSynced { id, result })?;
        Ok(())
    }

    pub(super) async fn handle_tool_calls_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        calls: Vec<crate::sansio::PendingToolCall>,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        // Per-tool trace events (ToolCallStarted / ToolCallCompleted) are
        // emitted from the shared tool-execution seam so every tool call
        // produces exactly one Started + one Completed pair. See
        // `RuntimeExecutionContext::emit_tool_call_started_trace` /
        // `emit_tool_call_completed_trace`.
        let results = match self
            .invoke_turn_tool_calls_effect(machine, id, calls, event_tx, cancel)
            .await
        {
            Ok(results) => results,
            Err(err) => {
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        for result in &results {
            let producer = crate::AttachmentProducer::Tool {
                tool_name: result.tool_name.clone(),
            };
            for source in result.output.attachments() {
                if let Err(err) = self
                    .host
                    .core
                    .attachment_source_policy
                    .authorize(&producer, &source)
                {
                    self.fail_or_abort_runtime_effect_controller(
                        machine,
                        crate::RuntimeEffectControllerError::new(
                            "attachment_source_policy_denied",
                            err.to_string(),
                        ),
                    )?;
                    return Ok(());
                }
            }
        }
        self.handle_machine_response(machine, Response::ToolResults { id, results })?;
        Ok(())
    }

    pub(super) async fn handle_exec_code_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        language: String,
        code: String,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let code_correlation_id = TurnActivityId::new(format!("code:{id:?}"));
        let iteration = machine.protocol_iteration();
        if self.host.core.tracing.trace_sink.is_some() {
            self.emit_protocol_diagnostic_trace(
                iteration,
                "exec_code_started",
                serde_json::json!({
                    "code": code,
                    "code_chars": code.chars().count(),
                }),
            );
        }
        let invocation = match self.turn_effect_invocation(machine, id, RuntimeEffectKind::ExecCode)
        {
            Ok(invocation) => invocation,
            Err(err) => {
                let message = err.to_string();
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockStarted {
                        language: language.clone(),
                        code: code.clone(),
                        graph_key: None,
                    },
                )
                .await;
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockCompleted {
                        language: language.clone(),
                        output: String::new(),
                        error: Some(message),
                        success: false,
                        duration_ms: 0,
                        tool_call_ids: Vec::new(),
                        graph_key: None,
                    },
                )
                .await;
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        let graph_key = foreground_exec_graph_key(&invocation);
        send_turn_activity(
            event_tx,
            code_correlation_id.clone(),
            TurnEvent::CodeBlockStarted {
                language: language.clone(),
                code: code.clone(),
                graph_key: graph_key.clone(),
            },
        )
        .await;
        let exec_created_at = self.host.core.clock.now();
        let result = match self
            .invoke_turn_exec_effect(
                machine,
                invocation,
                language.clone(),
                code.clone(),
                event_tx,
                cancel,
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                let message = err.to_string();
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockCompleted {
                        language: language.clone(),
                        output: String::new(),
                        error: Some(message),
                        success: false,
                        duration_ms: self
                            .host
                            .core
                            .clock
                            .now()
                            .saturating_duration_since(exec_created_at)
                            .as_millis() as u64,
                        tool_call_ids: Vec::new(),
                        graph_key: graph_key.clone(),
                    },
                )
                .await;
                self.fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        match &result {
            Ok(output) => {
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockCompleted {
                        language: language.clone(),
                        output: output.observations.join("\n"),
                        error: output.error.clone(),
                        success: output.error.is_none(),
                        duration_ms: output.duration_ms,
                        tool_call_ids: output
                            .tool_calls
                            .iter()
                            .filter_map(|record| record.call_id.clone())
                            .collect(),
                        graph_key: graph_key.clone(),
                    },
                )
                .await;
            }
            Err(error) => {
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockCompleted {
                        language: language.clone(),
                        output: String::new(),
                        error: Some(error.clone()),
                        success: false,
                        duration_ms: self
                            .host
                            .core
                            .clock
                            .now()
                            .saturating_duration_since(exec_created_at)
                            .as_millis() as u64,
                        tool_call_ids: Vec::new(),
                        graph_key: graph_key.clone(),
                    },
                )
                .await;
            }
        }
        if let Ok(output) = &result {
            if self.host.core.tracing.trace_sink.is_some() {
                let observations_text = output.observations.join("\n");
                let tool_calls = output
                    .tool_calls
                    .iter()
                    .map(|record| {
                        serde_json::json!({
                            "call_id": record.call_id,
                            "name": record.tool,
                            "duration_ms": record.duration_ms,
                            "status": format!("{:?}", record.output.status())
                                .to_ascii_lowercase(),
                        })
                    })
                    .collect::<Vec<_>>();
                self.emit_protocol_diagnostic_trace(
                    iteration,
                    "exec_code_completed",
                    serde_json::json!({
                        "duration_ms": output.duration_ms,
                        "output": observations_text,
                        "output_chars": observations_text.chars().count(),
                        "observation_count": output.observations.len(),
                        "observation_truncation": output.observation_truncation,
                        "error": output.error,
                        "terminal_finish": output.terminal_finish,
                        "terminal_finish_present": output.terminal_finish.is_some(),
                        "tool_call_count": output.tool_calls.len(),
                        "tool_calls": tool_calls,
                    }),
                );
                if !output.observation_truncation.is_empty() {
                    self.emit_protocol_diagnostic_trace(
                        iteration,
                        "observation_projection",
                        serde_json::json!({
                            "projections": output.observation_truncation,
                        }),
                    );
                }
            }
        } else if let Err(error) = &result
            && self.host.core.tracing.trace_sink.is_some()
        {
            self.emit_protocol_diagnostic_trace(
                iteration,
                "exec_code_failed",
                serde_json::json!({ "error": error }),
            );
        }
        self.handle_machine_response(
            machine,
            match result {
                Ok(output) => Response::ExecResult {
                    id,
                    result: Ok(output),
                },
                Err(error) => Response::ExecResult {
                    id,
                    result: Err(error),
                },
            },
        )?;
        Ok(())
    }
}

pub(super) fn foreground_exec_graph_key(invocation: &RuntimeInvocation) -> Option<String> {
    let RuntimeSubject::Effect { effect_id, kind } = &invocation.subject else {
        return None;
    };
    if *kind != RuntimeEffectKind::ExecCode {
        return None;
    }
    Some(match invocation.scope.turn_id.as_deref() {
        Some(turn_id) if !turn_id.is_empty() => {
            format!(
                "effect:{}:{turn_id}:{effect_id}",
                invocation.scope.session_id
            )
        }
        _ => format!("effect:{}:{effect_id}", invocation.scope.session_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_exec_graph_key_uses_runtime_invocation_identity() {
        let invocation = RuntimeInvocation::effect(
            RuntimeScope::for_turn("session-1", "turn-1", 2, 3),
            "effect-7",
            RuntimeEffectKind::ExecCode,
            "replay-key",
        );

        assert_eq!(
            foreground_exec_graph_key(&invocation).as_deref(),
            Some("effect:session-1:turn-1:effect-7")
        );
    }
}
