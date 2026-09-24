use super::*;
use lash_sansio::session_model::{FailureCode, TurnFailureCode};

impl RuntimeTurnDriver<'_> {
    /// Refuse a selected drain whose model-context cost Lash cannot bound.
    ///
    /// FIG-1313: this seam once also refused any selected drain whose complete
    /// conservative projection (prompt, tools, retained history, and the queued
    /// rows together) exceeded the window. That hardwired guard was a law no
    /// ordinary turn had to obey, and it wedged every host under roughly 28k
    /// tokens: the queue could never drain even one row. Drain size is now a
    /// host policy ([`QueuedDrainPolicy`](crate::QueuedDrainPolicy), defaulting
    /// to one row per drain) and the irreducible residue — a single row larger
    /// than the whole window — is refused as a typed outcome at claim time.
    /// Everything in between is the provider's judgement, exactly as for an
    /// ordinary turn.
    ///
    /// What remains here is the one cost Lash genuinely cannot project: an
    /// external or provider-file attachment, whose model-context weight is not
    /// bounded by any bytes Lash can measure.
    fn ensure_queued_work_cost_is_bounded(&self, request: &LlmRequest) -> Result<(), RuntimeError> {
        if self.pending_queue_claims.is_empty()
            || !self.turn_context.enforces_selected_queued_work_cost_bound()
        {
            return Ok(());
        }
        // Attachment *resolution* is deliberately not performed here: it only
        // materializes stored bytes, leaving `attachments` — the sources this
        // guard reads — untouched. Since FIG-1313 removed the projected-request
        // token comparison, resolving would buy nothing but store reads and a
        // new failure mode on the selected-drain path.
        if request.attachments().iter().any(|source| {
            matches!(
                source,
                crate::AttachmentSource::ExternalUrl { .. }
                    | crate::AttachmentSource::ProviderFile { .. }
            )
        }) {
            return Err(RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                "cannot safely admit queued work with an external or provider-file attachment: \
                 its model-context cost is not bounded by the projected request",
            ));
        }

        Ok(())
    }

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
            self.emit_trace(
                machine.protocol_iteration(),
                lash_trace::TraceEvent::AttachmentDegraded {
                    attachment_id: notice.attachment_id,
                    label: notice.label,
                    media_type: notice.media_type,
                    source: notice.source,
                    reason: notice.reason,
                },
            );
        }
        self.ensure_queued_work_cost_is_bounded(&request)?;
        self.reasoning_publication = ReasoningPublicationState::default();
        let (result, text_streamed, call_record) = match self
            .invoke_turn_llm_effect(machine, id, request, event_tx, cancel)
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
            send_turn_activity(
                event_tx,
                TurnActivityId::new(call_record.call_id.0.clone()),
                TurnEvent::ModelCallRecorded {
                    record: call_record.clone(),
                },
            )
            .await;
            self.llm_calls.push(call_record);
        }
        // Phase 2 of the staged boundary runs only once the paid attempt is
        // journaled and on the ledger, so a failing derivation can never take
        // the record of what we bought down with it.
        let result = match result {
            Ok(raw) if self.session.plugins().has_assistant_response_hooks() => {
                match self
                    .invoke_assistant_response_hooks_effect(machine, id, raw, event_tx, cancel)
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
        // FIG-793: the LLM run is the deployed first journal command for this
        // protocol iteration, so it must be emitted and awaited before any
        // cancellation observation is registered. Restate SDK 0.10 emits a
        // `ctx.run` command only when its future is polled and requires that
        // future to be awaited immediately; it cannot honestly be selected
        // away from mid-flight. The durable contract is therefore cancellation
        // between iterations. A local provider may still cooperatively observe
        // `cancel` while the run is executing, and that result is journaled.
        // Runtime-owned (native) execution keeps its existing cooperative-token
        // behavior. Only a controller-owned journal needs this additional
        // durable, replayed boundary.
        if self.observes_durable_cancel_after_llm {
            let pending_cancel = self
                .turn_control
                .observe_pending_cancel(
                    &self.scoped_effect_controller,
                    crate::runtime::turn_control::TurnCancelPeekIdentity::AfterLlm {
                        protocol_iteration: machine.protocol_iteration(),
                    },
                )
                .await?;
            if let Some(evidence) = pending_cancel {
                cancel.cancel();
                send_session_event(event_tx, SessionStreamEvent::Done).await;
                machine.finish_with_outcome(crate::TurnOutcome::Stopped(TurnStop::Cancelled {
                    evidence,
                }));
                return Ok(());
            }
        }
        if let Ok(response) = &result {
            let usage = crate::runtime::effect::token_usage_from_llm(&response.usage);
            self.latest_prompt_usage = nonzero_usage(usage);
            if !text_streamed {
                let prose_projector = self.session.plugins().assistant_prose_projector();
                emit_semantic_response_parts(
                    event_tx,
                    response,
                    prose_projector.as_deref(),
                    &self.reasoning_publication,
                )
                .await;
            }
        }
        // Name the request that stopped the call before the machine decides a
        // cancelled terminal reason, so its outcome carries real evidence.
        if let Some(evidence) = self.turn_control.evidence() {
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        if matches!(checkpoint, CheckpointKind::BeforeCompletion) {
            // The opener's end precedes the turn's terminal checkpoint, so
            // the facts its losers' settlements carry are delivered and
            // committed with the turn (ADR 0099 §7 step 2).
            Box::pin(self.finish_opener_groups_before_completion(event_tx)).await?;
        }
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
                // FIG-635: the step boundary. The checkpoint commit above is
                // the last act of the protocol iteration it closes (response
                // streamed, tools completed, work committed), and the machine
                // has already advanced its counter, so the closed iteration
                // is one behind. A machine that finished on this checkpoint
                // has its request honoured at commit instead.
                if !machine.is_done()
                    && let Some(closed_iteration) = machine.protocol_iteration().checked_sub(1)
                {
                    self.observe_step_boundary_cancel(machine, closed_iteration, event_tx, cancel)
                        .await?;
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
                // FIG-3157: a terminal checkpoint that failed delivers
                // nothing and starts no follow-on turn, so work withheld
                // for that follow-on was never delivered and must go back
                // to the queue claimable.
                if let Some(withheld) = self.withheld_terminal_work.take_if_any()
                    && let Some(store) = self.session.history_store()
                {
                    if !withheld.queued.is_empty() {
                        store
                            .abandon_queued_work_claims(&withheld.queued)
                            .await
                            .map_err(crate::runtime::runtime_error_from_store_commit)?;
                    }
                    let turn_input_claims = &withheld.turn_inputs;
                    if !turn_input_claims.is_empty() {
                        store
                            .abandon_turn_input_claims(turn_input_claims)
                            .await
                            .map_err(crate::runtime::runtime_error_from_store_commit)?;
                    }
                }
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
            }
        }
        Ok(())
    }

    /// Observe the cancellation gate at the step boundary that closed
    /// `closed_iteration` (`turn_cancel.after_step.{n}`), identically on the
    /// native and the controller-owned binding.
    ///
    /// An after-step request found here is honoured without the cooperative
    /// token: nothing is in flight, the checkpoint is committed, so the turn
    /// simply finishes cancelled. An immediate request found here on the
    /// native binding fires the token and lets the run observe it exactly as
    /// the live watcher would have; on a controller-owned journal it finishes
    /// here, between journal commands, like the after-LLM gate.
    async fn observe_step_boundary_cancel(
        &mut self,
        machine: &mut TurnMachine,
        closed_iteration: usize,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let effect_host = Arc::clone(&self.host.core.control.effect_host);
        let binding = effect_host
            .turn_control_binding(&self.scoped_effect_controller)
            .await?;
        let (resolver, peek_controller): (&dyn AwaitEventResolver, &ScopedEffectController<'_>) =
            match &binding {
                crate::TurnControlBinding::HostOwned { resolver, peek, .. } => (*resolver, peek),
                crate::TurnControlBinding::RunScoped { resolver, .. } => {
                    (*resolver, &self.scoped_effect_controller)
                }
            };
        // A process-local after-step stop lands on the durable gate before
        // the journaled peek, so replay sees the gate and never the flag.
        self.turn_control.resolve_local_after_step(resolver).await?;
        let pending_cancel = self
            .turn_control
            .observe_pending_cancel(
                peek_controller,
                crate::runtime::turn_control::TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: closed_iteration,
                },
            )
            .await?;
        let Some(evidence) = pending_cancel else {
            return Ok(());
        };
        if evidence.mode.is_immediate() && !self.observes_durable_cancel_after_llm {
            cancel.cancel();
            return Ok(());
        }
        send_session_event(event_tx, SessionStreamEvent::Done).await;
        machine.finish_with_outcome(crate::TurnOutcome::Stopped(TurnStop::Cancelled {
            evidence,
        }));
        Ok(())
    }

    pub(super) async fn handle_execution_environment_sync_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        let (result, cell_replay_grammar) = match self
            .invoke_turn_execution_environment_sync_effect(machine, id, event_tx, cancel)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        self.handle_machine_response(
            machine,
            Response::ExecutionEnvironmentSynced {
                id,
                result,
                cell_replay_grammar,
            },
        )?;
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
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
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
                    Self::fail_or_abort_runtime_effect_controller(
                        machine,
                        crate::RuntimeEffectControllerError::new(
                            crate::RuntimeErrorCode::AttachmentSourcePolicyDenied,
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
            self.emit_trace(
                iteration,
                lash_trace::TraceEvent::ExecCodeStarted {
                    code: code.clone(),
                    code_chars: code.chars().count(),
                },
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
                        error: Some(crate::CellFailure::new(
                            crate::CellFailureKind::Host,
                            message,
                        )),
                        success: false,
                        duration_ms: 0,
                        tool_call_ids: Vec::new(),
                        graph_key: None,
                    },
                )
                .await;
                Self::fail_or_abort_runtime_effect_controller(machine, err)?;
                return Ok(());
            }
        };
        let graph_key = Some(foreground_effect_graph_key(&invocation));
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
                        error: Some(crate::CellFailure::new(
                            crate::CellFailureKind::Host,
                            message,
                        )),
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
                let cancellation_evidence = self.turn_control.evidence();
                if let Some(code_executor) = self.session.plugins().code_executor() {
                    code_executor
                        .settle_code_execution(if cancellation_evidence.is_some() {
                            crate::plugin::CodeExecutionDisposition::Cancelled
                        } else {
                            crate::plugin::CodeExecutionDisposition::Discarded
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
        match &result {
            Ok(output) => {
                send_turn_activity(
                    event_tx,
                    code_correlation_id.clone(),
                    TurnEvent::CodeBlockCompleted {
                        language: language.clone(),
                        output: join_observations(&output.observations),
                        error: output.error.clone(),
                        success: output.error.is_none(),
                        duration_ms: output.duration_ms,
                        tool_call_ids: output
                            .calls
                            .iter()
                            .filter_map(|call| call.host_record.as_ref())
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
                        error: Some(crate::CellFailure::new(
                            crate::CellFailureKind::Host,
                            error.message.clone(),
                        )),
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
                        duration_ms: record.duration_ms,
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
                self.emit_trace(
                    iteration,
                    lash_trace::TraceEvent::ExecCodeCompleted {
                        duration_ms: output.duration_ms,
                        output: observations_text.clone(),
                        output_chars: observations_text.chars().count(),
                        observation_count: output.observations.len(),
                        observation_projections: observation_projections.clone(),
                        error: output.error.clone(),
                        terminal_finish: output.terminal_finish.clone(),
                        tool_calls,
                    },
                );
                if !observation_projections.is_empty() {
                    self.emit_trace(
                        iteration,
                        lash_trace::TraceEvent::ObservationProjection {
                            projections: observation_projections,
                        },
                    );
                }
            }
        } else if let Err(error) = &result
            && self.host.core.tracing.trace_sink.is_some()
        {
            self.emit_trace(
                iteration,
                lash_trace::TraceEvent::ExecCodeFailed {
                    reason: error.reason,
                    error: error.message.clone(),
                },
            );
        }
        // Name the request that stopped code execution before the protocol
        // classifies its typed Stop response. This is the same evidence seam
        // used for provider cancellation above.
        let cancellation_evidence = self.turn_control.evidence();
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
                    crate::plugin::CodeExecutionDisposition::Cancelled
                } else {
                    crate::plugin::CodeExecutionDisposition::Accepted
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
        self.handle_machine_response(
            machine,
            match result {
                Ok(output) => Response::ExecResult {
                    id,
                    result: Ok(output),
                },
                Err(error) => Response::ExecResult {
                    id,
                    result: Err(error.message),
                },
            },
        )?;
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
        let invocation = RuntimeEffectInvocation::new(
            EffectAddress::new(ExecutionScope::turn("session-1", "turn-1"), "replay-key")
                .expect("valid foreground exec address"),
            RuntimeAttribution::for_turn("session-1", "turn-1", 2, 3),
            "effect-7",
        );

        assert_eq!(
            foreground_effect_graph_key(&invocation),
            "effect:{\"version\":2,\"kind\":\"turn\",\"session_id\":\"session-1\",\"execution_id\":\"turn-1\"}:\"replay-key\""
        );
    }
}
