//! Provider stream assembly and host forwarding are deliberately separate.
//!
//! Committed blocks and checkpoints are authoritative. Deltas describe
//! provider-wire volume, so hosts may observe fewer, larger delta events under
//! delivery lag without changing transcript content. Pending deltas therefore
//! coalesce losslessly by correlation; they are never committed state.

use std::sync::Arc;

use crate::ModelGenerationClamp;

use lash_trace::{
    TraceError, TraceEvent, TraceProviderRequestEvent, TraceProviderStreamEvent,
    TraceRuntimeStreamEvent,
};

use super::*;

mod host_forwarder;
mod support;

use support::*;
mod terminal;

use host_forwarder::{ProviderDeltaClass, ProviderHostForwarder};
use lash_sansio::session_model::{FailureCode, TurnFailureCode};
use terminal::{observed_stream_protocol_position, synthesize_protocol_abort};

/// Largest exact provider request body retained as structured JSON in a trace.
/// Larger bodies keep their byte length and wire-byte digest without inflating
/// each JSONL record and optional OpenTelemetry payload attribute.
pub(crate) const MAX_PROVIDER_REQUEST_BODY_JSON_BYTES: usize = 2_048;

/// Result of running stream hooks over a visible chunk. Carries both
/// the (possibly rewritten) text and an `abort_requested` flag that the
/// LLM runner uses to break the stream early when a plugin has decided
/// the response is complete (for example, a protocol mask detecting a
/// closed code fence).
pub(super) struct StreamChunkOutcome {
    pub(super) chunk: String,
    pub(super) reasoning_deltas: Vec<String>,
    pub(super) abort_requested: bool,
}

impl RuntimeTurnDriver<'_> {
    /// Runs the staged two-phase LLM-call effect boundary (FIG-1276).
    ///
    /// Phase 1 journals the **raw** provider completion: the paid external fact
    /// becomes durable before any fallible host code runs. Phase 2 is a second,
    /// distinct journal entry holding what host assistant-response hooks derived
    /// from that completion: the transformed response together with the plugin
    /// events those hooks emitted.
    ///
    /// Only a *complete* derivation is journaled. A hook failure seals nothing
    /// and surfaces as a retryable
    /// [`RuntimeErrorCode::RuntimeEffectAssistantResponseHook`](crate::RuntimeErrorCode::RuntimeEffectAssistantResponseHook),
    /// because a sealed error would replay as a permanent result over a
    /// completion that is merely underived.
    ///
    /// A crash or hook failure after phase 1 therefore redrives phase 2 only:
    /// replay serves the recorded completion and the provider is never
    /// re-invoked. Hooks are consequently **at-least-once**; see
    /// [`crate::plugin::AssistantResponseHook`].
    ///
    /// Honest scope: the window this does *not* cover is journal finalization
    /// itself — a process that dies after the provider returns but before phase
    /// 1 becomes durable still has no record to replay. Closing that needs
    /// provider-side idempotency or resume (FIG-1275), not more staging here.
    pub(super) async fn invoke_turn_llm_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        request: Arc<LlmRequest>,
        event_tx: &TurnObserver,
    ) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError> {
        let invocation = self.turn_effect_invocation(machine, id, RuntimeEffectKind::LlmCall)?;
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::LlmCall {
                    provider_id: self.policy.provider_id.clone(),
                    request: Box::new(
                        LlmRequestSpec::from_request(
                            &request,
                            self.host.core.durability.attachment_store.as_ref(),
                        )
                        .await?,
                    ),
                },
            ),
            RuntimeEffectOutcome::into_llm_call,
        )
        .await
    }

    async fn transform_assistant_stream_chunk(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        chunk: String,
    ) -> Result<StreamChunkOutcome, LlmCallError> {
        if !self.session.plugins().has_assistant_stream_hooks() {
            return Ok(StreamChunkOutcome {
                chunk,
                reasoning_deltas: Vec::new(),
                abort_requested: false,
            });
        }

        let original = chunk.clone();
        let transforms = self
            .session
            .plugins()
            .transform_assistant_stream(&self.session_id, chunk)
            .await
            .map_err(|err| LlmCallError {
                message: err.to_string(),
                retryable: false,
                kind: crate::ProviderFailureKind::Unknown,
                raw: None,
                code: Some(FailureCode::lash(TurnFailureCode::PluginAssistantStream)),
                terminal_reason: crate::LlmTerminalReason::ProviderError,
                request_body: None,
                partial_response: None,
            })?;
        let mut current = String::new();
        let mut first = true;
        let mut abort_requested = false;
        let mut reasoning_deltas = Vec::new();
        for emitted in transforms {
            if first {
                first = false;
            }
            current = emitted.value.chunk.clone();
            reasoning_deltas.extend(emitted.value.reasoning_deltas.clone());
            if emitted.value.abort_stream {
                abort_requested = true;
            }
            emit_plugin_runtime_events_runtime(forwarder, &emitted.plugin_id, emitted.value.events);
        }
        let chunk = if first { original } else { current };
        Ok(StreamChunkOutcome {
            chunk,
            reasoning_deltas,
            abort_requested,
        })
    }

    /// Phase 2 body of the staged LLM-call boundary: derive the host-visible
    /// response from the raw completion phase 1 already journaled.
    ///
    /// Hook-emitted plugin events are returned rather than forwarded here so
    /// they are journaled with this phase's outcome and served from it on
    /// replay. Because this phase redrives independently of phase 1, a hook may
    /// observe the same raw completion more than once — the at-least-once
    /// contract documented on [`crate::plugin::AssistantResponseHook`].
    pub(in crate::runtime) async fn run_assistant_response_hooks(
        &mut self,
        response: LlmResponse,
        stream_hook_states: &[crate::runtime::AssistantStreamHookState],
    ) -> Result<crate::runtime::RuntimeAssistantResponseHooksOutcome, RuntimeEffectControllerError>
    {
        let original = response.clone();
        let transforms = self
            .session
            .plugins()
            .transform_assistant_response(&self.session_id, response, stream_hook_states)
            .await
            .map_err(|err| {
                RuntimeEffectControllerError::retryable_response_derivation(format!(
                    "assistant response hook failed: {err}"
                ))
            })?;
        let mut current: Option<LlmResponse> = None;
        let mut events = Vec::new();
        for emitted in transforms {
            if !emitted.value.events.is_empty() {
                events.push(crate::AssistantResponseHookEvents {
                    plugin_id: emitted.plugin_id,
                    events: emitted.value.events,
                });
            }
            current = Some(emitted.value.response);
        }
        Ok((current.unwrap_or(original), events))
    }

    pub(in crate::runtime) async fn run_llm_call(
        &mut self,
        request: Arc<LlmRequest>,
        protocol_iteration: usize,
        invocation: crate::RuntimeInvocation,
        event_tx: &TurnObserver,
        cancel: &CancellationToken,
    ) -> RuntimeLlmCallOutcome {
        let mut request = (*request).clone();
        let protocol_suppressed_stop_sequences =
            request.generation.stop_sequences_suppressed_by_protocol();
        let clamped_output_token_cap = self
            .policy
            .model
            .clamp_generation_options(&mut request.generation);
        let request = match crate::attachments::resolve_llm_request_attachments(
            request,
            self.host.core.durability.attachment_store.as_ref(),
        )
        .await
        {
            Ok(request) => request,
            Err(err) => {
                return RuntimeLlmCallOutcome {
                    result: Err(LlmCallError {
                        message: err.to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Unknown,
                        raw: None,
                        code: Some(FailureCode::lash(
                            TurnFailureCode::AttachmentResolutionFailed,
                        )),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: None,
                        partial_response: None,
                    }),
                    text_streamed: false,
                    call_record: None,
                    stream: crate::runtime::LlmStreamRecord::default(),
                };
            }
        };
        let request_model = request.model.clone();
        let trace_enabled = self.host.core.tracing.trace_sink.is_some();
        let llm_call_id = trace_enabled.then(|| self.llm_call_id(protocol_iteration, &invocation));
        if let Some(llm_call_id) = llm_call_id.as_ref() {
            crate::runtime::effect::emit_llm_trace_started(
                &self.host.core.tracing.trace_sink,
                &self.host.core.tracing.trace_context,
                crate::trace::trace_context_from_invocation(&invocation)
                    .for_llm_call(llm_call_id.clone()),
                &request,
                self.host.core.clock.as_ref(),
            );
        }
        let (llm_stream_tx, mut llm_stream_rx) =
            tokio::sync::mpsc::unbounded_channel::<LlmStreamEvent>();
        let mut debug = LlmStreamDebugState::new(self.host.core.clock.now());
        let provider_trace =
            self.provider_trace_sender(protocol_iteration, llm_call_id.clone(), &debug);
        // The projector is built from the physical turn's committed frame.
        // A logical-turn follow-on may already have advanced beyond the
        // boundary's resident snapshot, so keep that frame identity while
        // replacing only the runtime-owned request correlation fields.
        let projected_agent_frame_id = request.scope.agent_frame_id.clone();
        let mut llm_request = LlmRequest {
            scope: crate::LlmRequestScope::new(
                self.session_id.clone(),
                projected_agent_frame_id,
                format!(
                    "{}:turn:{}:llm:{}",
                    self.session_id, self.turn_id, protocol_iteration
                ),
            ),
            stream_events: transport_stream_events(self.policy.provider(), Some(llm_stream_tx)),
            provider_trace,
            generation: request.generation.clone(),
            ..request
        };

        // Each call runs on its own copy of the provider the turn was admitted
        // with. Nothing the provider object learns during a call reaches the
        // next one: a replay, which never runs this body, must make the same
        // next call as the live pass.
        let mut call_provider = self.policy.provider().clone();
        let completion_sideband = call_provider.prepare_completion(&mut llm_request);
        let task_sideband = completion_sideband.clone();
        let charge_safety = self.policy.charge_safety.clone();
        let call_id = crate::provider::call_id_for_scope(&llm_request.scope);
        let mut llm_task = crate::task::spawn(async move {
            call_provider
                .complete_prepared(llm_request, task_sideband, charge_safety)
                .await
        });
        let mut llm_task_abort = AbortOnDrop::new(llm_task.abort_handle());

        let mut text_streamed = false;
        let mut streamed_usage = LlmUsage::default();
        let mut stream_accumulator = LlmStreamAccumulator::default();
        let mut stream_evidence = crate::LlmStreamEvidence::default();
        let mut abort_requested = false;
        let mut block_raw_text = std::collections::HashMap::new();
        let attempt_started_at = self.host.core.clock.timestamp_ms();
        let attempt_started = self.host.core.clock.now();
        let mut plugin_reasoning_blocks = 0u64;
        let mut completed_part_index = 0usize;
        let mut reasoning_publication = ReasoningPublicationState::default();
        let mut assistant_prose_attempt_correlations = Vec::new();
        let mut reasoning_attempt_correlations = Vec::new();
        let mut stream_state = LlmStreamState {
            text_streamed: &mut text_streamed,
            streamed_usage: &mut streamed_usage,
            stream_accumulator: &mut stream_accumulator,
            stream_evidence: &mut stream_evidence,
            debug: &mut debug,
            protocol_iteration,
            plugin_reasoning_blocks: &mut plugin_reasoning_blocks,
            completed_part_index: &mut completed_part_index,
            reasoning_publication: &mut reasoning_publication,
            assistant_prose_attempt_correlations: &mut assistant_prose_attempt_correlations,
            reasoning_attempt_correlations: &mut reasoning_attempt_correlations,
            abort_requested: &mut abort_requested,
            block_raw_text: &mut block_raw_text,
        };
        let mut host_forwarder = ProviderHostForwarder::new(event_tx);
        let mut call_record = None;
        let result = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    llm_task.abort();
                    let failure = crate::llm::transport::LlmTransportError::new("cancelled")
                    .with_kind(crate::ProviderFailureKind::Unknown)
                    .with_lash_code(TurnFailureCode::Cancelled)
                    .with_terminal_reason(crate::LlmTerminalReason::Cancelled)
                    .with_retry_verdict(
                        crate::llm::transport::TransportRetryVerdict::NotRetryable,
                    );
                    call_record = Some(crate::provider::synthetic_terminal_call_record(
                        call_id.clone(),
                        attempt_started_at,
                        self.host
                            .core
                            .clock
                            .now()
                            .saturating_duration_since(attempt_started),
                        crate::AttemptOutcome::Aborted,
                        &failure,
                        true,
                        observed_stream_protocol_position(
                            *stream_state.text_streamed,
                            stream_state.stream_accumulator,
                            stream_state.stream_evidence,
                        ),
                        completion_sideband.replay_drops(),
                    ));
                    break Err(crate::runtime::effect::llm_call_error_from_transport(failure));
                }
                Some(stream_event) = llm_stream_rx.recv() => {
                    if let Err(err) = self
                        .forward_provider_stream_event(
                            &mut host_forwarder,
                            stream_event,
                            &mut stream_state,
                        )
                        .await
                    {
                        break Err(err);
                    }
                    if *stream_state.abort_requested {
                        // A plugin stream hook asked us to end the LLM
                        // call now after seeing a complete response block.
                        // The response is committed once the plugin asks to
                        // abort. Continue forwarding late text, signed
                        // reasoning, and usage into that response, but stop at
                        // an attempt-reset boundary rather than erasing the
                        // already-complete cell.
                        if let Err(err) = self
                            .collect_trailing_stream_events_before_abort(
                                &mut host_forwarder,
                                &mut llm_task,
                                &mut llm_stream_rx,
                                &mut stream_state,
                            )
                            .await
                        {
                            break Err(err);
                        }
                        if llm_task.is_finished()
                            && let Ok(provider_result) = (&mut llm_task).await
                        {
                            llm_task_abort.disarm();
                            match provider_result {
                                Ok(completion) => {
                                    let crate::ProviderCompletion {
                                        response: mut resp,
                                        call_record: completed_call_record,
                                    } = completion;
                                    call_record = Some(completed_call_record);
                                    if response_usage_is_empty(&resp.usage) {
                                        resp.usage = stream_state.streamed_usage.clone();
                                    }
                                    stream_state.stream_accumulator.apply_to_response(&mut resp);
                                    break Ok(resp);
                                }
                                Err(error) => {
                                    let crate::ProviderCompletionError {
                                        error,
                                        call_record: failed_call_record,
                                    } = error;
                                    call_record = Some(*failed_call_record);
                                    if completion_sideband.origin_conflict().is_some() {
                                        break Err(
                                            crate::runtime::effect::llm_call_error_from_transport(
                                                error,
                                            ),
                                        );
                                    }
                                    let (resp, _) = synthesize_protocol_abort(
                                        stream_state.stream_accumulator,
                                        stream_state.streamed_usage.clone(),
                                        stream_state.stream_evidence,
                                        attempt_started_at,
                                        self.host
                                            .core
                                            .clock
                                            .now()
                                            .saturating_duration_since(attempt_started),
                                        completion_sideband.replay_drops(),
                                    );
                                    break Ok(resp);
                                }
                            }
                        }
                        let (resp, aborted_call_record) = synthesize_protocol_abort(
                            stream_state.stream_accumulator,
                            stream_state.streamed_usage.clone(),
                            stream_state.stream_evidence,
                            attempt_started_at,
                            self.host
                                .core
                                .clock
                                .now()
                                .saturating_duration_since(attempt_started),
                            completion_sideband.replay_drops(),
                        );
                        let mut resp = resp;
                        if let Err(error) = completion_sideband.fence_response(&mut resp) {
                            call_record = Some(aborted_call_record);
                            let retryable = error.is_retryable();
                            break Err(LlmCallError {
                                message: error.message,
                                retryable,
                                kind: error.kind,
                                raw: error.raw.map(|raw| *raw),
                                code: error.code,
                                terminal_reason: error.terminal_reason,
                                request_body: error.request_body.map(|body| *body),
                                partial_response: error.partial_response,
                            });
                        }
                        call_record = Some(aborted_call_record);

                        break Ok(resp);
                    }
                }
                join = &mut llm_task => {
                    let result = match join {
                        Ok(v) => {
                            llm_task_abort.disarm();
                            v
                        }
                        Err(e) if e.is_panic() => {
                            let payload = e.into_panic();
                            let message = crate::panic_containment::payload_message(payload.as_ref());
                            call_record = Some(crate::LlmCallRecord {
                                call_id: crate::LlmCallId(uuid::Uuid::new_v4().to_string()),
                                label: None,
                                replay_drops: completion_sideband.replay_drops(),
                                attempts: vec![crate::AttemptRecord {
                                    ordinal: 1,
                                    started_at: self.host.core.clock.timestamp_ms(),
                                    duration: std::time::Duration::ZERO,
                                    outcome: crate::AttemptOutcome::Failed,
                                    protocol_position: crate::ProtocolPosition::NoResponse,
                                    retry_budget_consumed: true,
                                    retry_decision: Some(crate::RetryDecision {
                                        scheduled: false,
                                        delay: None,
                                        reason: Some("not_retryable".to_string()),
                                        charge_safety: None,
                                    }),
                                    error: Some(crate::NormalizedError {
                                        class: crate::ProviderFailureKind::Unknown.code().to_string(),
                                        code: Some(FailureCode::lash(TurnFailureCode::ProviderPanicked)),
                                        http_status: None,
                                        provider_request_id: None,
                                        retry_after: None,
                                        diagnostic: Some(message.clone()),
                                    }),
                                    evidence: None,
                                    generation_disposition: None,
                                    usage: None,
                                    usage_disposition:
                                        crate::AttemptUsageDisposition::UnreportedAfterFailure,
                                }],
                            });
                            let failure = LlmCallError {
                                message,
                                retryable: false,
                                kind: crate::ProviderFailureKind::Unknown,
                                raw: None,
                                code: Some(FailureCode::lash(TurnFailureCode::ProviderPanicked)),
                                terminal_reason: crate::LlmTerminalReason::ProviderError,
                                request_body: None,
                                partial_response: None,
                            };
                            drop(payload);
                            break Err(failure);
                        }
                        Err(e) => {
                            let failure = crate::llm::transport::LlmTransportError::new(format!(
                                "internal task failed: {e}"
                            ))
                            .with_kind(crate::ProviderFailureKind::Unknown)
                            .with_lash_code(TurnFailureCode::TaskJoinFailed)
                            .with_retry_verdict(
                                crate::llm::transport::TransportRetryVerdict::NotRetryable,
                            );
                            call_record = Some(crate::provider::synthetic_terminal_call_record(
                                call_id.clone(),
                                attempt_started_at,
                                self.host
                                    .core
                                    .clock
                                    .now()
                                    .saturating_duration_since(attempt_started),
                                crate::AttemptOutcome::Interrupted,
                                &failure,
                                true,
                                observed_stream_protocol_position(
                                    *stream_state.text_streamed,
                                    stream_state.stream_accumulator,
                                    stream_state.stream_evidence,
                                ),
                                completion_sideband.replay_drops(),
                            ));
                            break Err(crate::runtime::effect::llm_call_error_from_transport(
                                failure,
                            ));
                        }
                    };
                    if let Err(err) = self
                        .drain_provider_stream_queue(
                            &mut host_forwarder,
                            &mut llm_stream_rx,
                            &mut stream_state,
                        )
                        .await
                    {
                        break Err(err);
                    }
                    match result {
                        Ok(completion) => {
                            let crate::ProviderCompletion {
                                response: mut resp,
                                call_record: completed_call_record,
                            } = completion;
                            call_record = Some(completed_call_record);
                            if response_usage_is_empty(&resp.usage) {
                                resp.usage = streamed_usage.clone();
                            }
                            stream_accumulator.apply_to_response(&mut resp);
                            break Ok(resp)
                        }
                        Err(e) => {
                            let crate::ProviderCompletionError {
                                error: e,
                                call_record: failed_call_record,
                            } = e;
                            call_record = Some(*failed_call_record);
                            let retryable = e.is_retryable();
                            break Err(LlmCallError {
                                message: e.message,
                                retryable,
                                kind: e.kind,
                                raw: e.raw.map(|raw| *raw),
                                code: e.code,
                                terminal_reason: e.terminal_reason,
                                request_body: e.request_body.map(|body| *body),
                                partial_response: e.partial_response,
                            });
                        }
                    }
                }
            }
        };

        let mut result = result;
        if let Some(conflict) = completion_sideband.origin_conflict() {
            match &mut result {
                Ok(_) => {
                    result = Err(LlmCallError {
                        message: conflict.to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Validation,
                        raw: None,
                        code: Some(FailureCode::lash(
                            TurnFailureCode::ProviderReplayOriginConflict,
                        )),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: None,
                        partial_response: None,
                    });
                }
                Err(error)
                    if error.code
                        != Some(FailureCode::lash(
                            TurnFailureCode::ProviderReplayOriginConflict,
                        )) =>
                {
                    error.message =
                        format!("{conflict}; original runtime failure: {}", error.message);
                    error.retryable = false;
                    error.kind = crate::ProviderFailureKind::Validation;
                    error.code = Some(FailureCode::lash(
                        TurnFailureCode::ProviderReplayOriginConflict,
                    ));
                }
                Err(_) => {}
            }
        }
        if matches!(
            &result,
            Err(err) if err.terminal_reason == crate::LlmTerminalReason::Cancelled
        ) {
            // Deltas are non-authoritative provider-wire volume: a cancelled
            // call's backlog behind a lagging host is discarded rather than
            // delaying the cancelled result.
            event_tx.discard_lagging_deltas();
        }
        if clamped_output_token_cap {
            record_clamped_output_token_cap(&mut result, call_record.as_mut());
        }
        if protocol_suppressed_stop_sequences {
            record_protocol_owned_stop_suppression(&mut result, call_record.as_mut());
        }

        let stream_hook_states = self
            .finish_assistant_stream_hooks(assistant_stream_finish_reason(&result, abort_requested))
            .await;

        if let Err(err) = &result {
            tracing::error!(
                session_id = %self.session_id,
                turn = protocol_iteration,
                retryable = err.retryable,
                code = ?err.code,
                raw_present = err.raw.is_some(),
                request_body_present = err.request_body.is_some(),
                message = %err.message,
                "llm call failed"
            );
        }
        if let Some(llm_call_id) = llm_call_id {
            let stream_summary = debug.summary.to_json();
            match &result {
                Ok(response) => {
                    crate::runtime::effect::emit_llm_trace_completed(
                        &self.host.core.tracing.trace_sink,
                        &self.host.core.tracing.trace_context,
                        crate::trace::trace_context_from_invocation(&invocation)
                            .for_llm_call(llm_call_id),
                        response,
                        &request_model,
                        debug.elapsed_ms(self.host.core.clock.as_ref()),
                        Some(stream_summary.clone()),
                        call_record.as_ref(),
                        self.host.core.clock.as_ref(),
                    );
                }
                Err(error) => {
                    crate::runtime::effect::emit_llm_trace_failed(
                        &self.host.core.tracing.trace_sink,
                        &self.host.core.tracing.trace_context,
                        crate::trace::trace_context_from_invocation(&invocation)
                            .for_llm_call(llm_call_id),
                        crate::runtime::effect::LlmTraceFailure::from(error),
                        Some(stream_summary.clone()),
                        call_record.as_ref(),
                        self.host.core.clock.as_ref(),
                    );
                }
            }
        }
        RuntimeLlmCallOutcome {
            result,
            text_streamed,
            call_record,
            stream: crate::runtime::LlmStreamRecord {
                reasoning_published: reasoning_publication.into_published_blocks(),
                stream_hook_states,
            },
        }
    }

    /// Ends the stream for every stream-finished hook and returns the end
    /// states they hand to phase 2.
    async fn finish_assistant_stream_hooks(
        &mut self,
        reason: crate::plugin::AssistantStreamFinishReason,
    ) -> Vec<crate::runtime::AssistantStreamHookState> {
        if !self.session.plugins().has_assistant_stream_finished_hooks() {
            return Vec::new();
        }
        match self
            .session
            .plugins()
            .finish_assistant_stream(&self.session_id, reason)
            .await
        {
            Ok(states) => states,
            Err(err) => {
                tracing::error!(
                    session_id = %self.session_id,
                    reason = ?reason,
                    error = %err,
                    "assistant stream cleanup hook failed"
                );
                Vec::new()
            }
        }
    }

    pub(super) fn handle_log_event(&self, event: crate::sansio::LogEvent) {
        if self.host.core.tracing.trace_sink.is_none() {
            return;
        }

        match event {
            crate::sansio::LogEvent::LlmDebug {
                session_id,
                protocol_iteration,
                usage,
                provider_usage,
                response_text,
                response_parts,
                ..
            } => {
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    self.trace_context(protocol_iteration)
                        .for_session(session_id)
                        .for_llm_call(format!(
                            "{}:{}:{}:log",
                            self.session_id, self.turn_index, protocol_iteration
                        )),
                    TraceEvent::LlmCallCompleted {
                        response: crate::trace::trace_llm_response(
                            response_text,
                            0,
                            self.policy.model.id.clone(),
                            None,
                            response_parts,
                            None,
                        ),
                        usage: Some(crate::trace::trace_usage_from_session(&usage)),
                        provider_usage,
                        // The call's own trace carries its stream summary.
                        stream_summary: None,
                        attempts: None,
                    },
                    self.host.core.clock.as_ref(),
                );
            }
            crate::sansio::LogEvent::LlmError {
                session_id,
                protocol_iteration,
                message,
                retryable,
                raw,
                code,
                kind,
                terminal_reason,
                ..
            } => {
                crate::trace::emit_trace(
                    &self.host.core.tracing.trace_sink,
                    &self.host.core.tracing.trace_context,
                    self.trace_context(protocol_iteration)
                        .for_session(session_id)
                        .for_llm_call(format!(
                            "{}:{}:{}:log",
                            self.session_id, self.turn_index, protocol_iteration
                        )),
                    TraceEvent::LlmCallFailed {
                        error: TraceError {
                            message,
                            retryable,
                            terminal_reason: Some(terminal_reason.code().to_string()),
                            failure_kind: (kind != crate::ProviderFailureKind::Unknown)
                                .then(|| kind.code().to_string()),
                            code: code.as_ref().map(|code| code.spelling().to_string()),
                            code_namespace: code
                                .as_ref()
                                .map(|code| code.namespace().as_str().to_string()),
                            raw,
                        },
                        // The call's own trace carries its stream summary.
                        stream_summary: None,
                        attempts: None,
                    },
                    self.host.core.clock.as_ref(),
                );
            }
        }
    }

    fn log_llm_stream_event(&self, debug: &mut LlmStreamDebugState, log: LlmStreamEventLog<'_>) {
        if self.host.core.tracing.trace_sink.is_none() {
            return;
        }

        let elapsed_ms = debug.elapsed_ms(self.host.core.clock.as_ref());
        if matches!(log.event_type, "delta") {
            debug
                .summary
                .record_text_chunk(log.text.visible, elapsed_ms);
        }

        if !self.host.core.tracing.trace_level.is_extended() {
            return;
        }

        let mut event = TraceRuntimeStreamEvent {
            sequence: debug.next_sequence(),
            elapsed_ms,
            event_name: log.event_type.to_string(),
            raw_text: log.text.raw.map(str::to_string),
            visible_text: log.text.visible.map(str::to_string),
            item_id: log.item_id.map(str::to_string),
            block_id: log.block_id.map(str::to_string),
            output_index: None,
            call_id: None,
            tool_name: None,
            input_json: None,
            usage: log.usage.map(crate::trace::trace_usage_from_llm),
        };

        if let Some(tool_call) = log.tool_call {
            event.call_id = Some(tool_call.call_id.to_string());
            event.tool_name = Some(tool_call.tool_name.to_string());
            event.input_json = Some(
                serde_json::from_str(tool_call.input_json).unwrap_or_else(|_| {
                    serde_json::Value::String(tool_call.input_json.to_string())
                }),
            );
        }

        crate::trace::emit_trace(
            &self.host.core.tracing.trace_sink,
            &self.host.core.tracing.trace_context,
            self.trace_context(log.protocol_iteration),
            TraceEvent::RuntimeStreamEvent { event },
            self.host.core.clock.as_ref(),
        );
    }

    fn provider_trace_sender(
        &self,
        protocol_iteration: usize,
        llm_call_id: Option<String>,
        debug: &LlmStreamDebugState,
    ) -> Option<LlmProviderTraceSender> {
        if !self.host.core.tracing.trace_level.is_extended()
            || self.host.core.tracing.trace_sink.is_none()
        {
            return None;
        }

        let llm_call_id = llm_call_id?;
        let sink = self.host.core.tracing.trace_sink.clone();
        let base_context = self.host.core.tracing.trace_context.clone();
        let context = self.trace_context(protocol_iteration);
        let clock = Arc::clone(&self.host.core.clock);
        let created_at = debug.created_at;
        let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));

        Some(LlmProviderTraceSender::new(
            move |provider_event: LlmProviderTraceEvent| {
                let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let elapsed_ms = clock
                    .now()
                    .saturating_duration_since(created_at)
                    .as_millis() as u64;
                if let Some(endpoint) = provider_event.request_endpoint() {
                    let body_len = provider_event.raw.len();
                    let (body_json, body_json_omitted_reason) =
                        if body_len > MAX_PROVIDER_REQUEST_BODY_JSON_BYTES {
                            (None, Some("size_limit".to_string()))
                        } else {
                            match serde_json::from_str(&provider_event.raw) {
                                Ok(body_json) => (Some(body_json), None),
                                Err(_) => (None, Some("invalid_json".to_string())),
                            }
                        };
                    let event = TraceProviderRequestEvent {
                        provider: provider_event.provider.to_string(),
                        sequence,
                        elapsed_ms,
                        endpoint: endpoint.to_string(),
                        body_len,
                        body_sha256: lash_trace::sha256_hex(provider_event.raw.as_bytes()),
                        body_json,
                        body_json_omitted_reason,
                    };
                    crate::trace::emit_trace(
                        &sink,
                        &base_context,
                        context.clone().for_llm_call(llm_call_id.clone()),
                        TraceEvent::ProviderRequest { event },
                        clock.as_ref(),
                    );
                    return;
                }
                let raw_json = serde_json::from_str::<serde_json::Value>(&provider_event.raw).ok();
                let item_id = raw_json.as_ref().and_then(provider_item_id);
                let output_index = raw_json.as_ref().and_then(provider_output_index);
                let event = TraceProviderStreamEvent {
                    provider: provider_event.provider.to_string(),
                    sequence,
                    elapsed_ms,
                    event_name: provider_event.event_name,
                    item_id,
                    output_index,
                    raw_len: provider_event.raw.len(),
                    raw_sha256: lash_trace::sha256_hex(provider_event.raw.as_bytes()),
                    raw_json,
                };
                crate::trace::emit_trace(
                    &sink,
                    &base_context,
                    context.clone().for_llm_call(llm_call_id.clone()),
                    TraceEvent::ProviderStreamEvent { event },
                    clock.as_ref(),
                );
            },
        ))
    }

    /// Shared visible-assistant-text path for streamed text.
    async fn emit_visible_assistant_text(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        text: String,
        block: &StreamBlockIdentity,
        event_type: &'static str,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        if text.is_empty() {
            return Ok(());
        }
        *state.text_streamed = true;
        state
            .block_raw_text
            .entry(block.id.clone())
            .or_default()
            .push_str(&text);
        let raw_text = self
            .host
            .core
            .tracing
            .trace_sink
            .as_ref()
            .map(|_| text.clone());
        let outcome = self
            .transform_assistant_stream_chunk(forwarder, text)
            .await?;
        if outcome.abort_requested {
            *state.abort_requested = true;
        }
        self.forward_plugin_reasoning(forwarder, outcome.reasoning_deltas, state)
            .await;
        let text = outcome.chunk;
        self.log_llm_stream_event(
            state.debug,
            LlmStreamEventLog {
                protocol_iteration: state.protocol_iteration,
                event_type,
                text: LlmDebugText {
                    raw: raw_text.as_deref(),
                    visible: Some(&text),
                },
                item_id: block.item_id.as_deref(),
                block_id: Some(block.id.as_str()),
                usage: None,
                tool_call: None,
            },
        );
        if !text.is_empty() {
            fold_llm_stream_event(
                state.stream_accumulator,
                state.streamed_usage,
                &LlmStreamEvent::Delta {
                    block: block.clone(),
                    text: text.clone(),
                },
            );
            remember_attempt_correlation(
                state.assistant_prose_attempt_correlations,
                &TurnActivityId::new(block.id.clone()),
            );
            forwarder.forward_delta(ProviderDeltaClass::AssistantProse, block.clone(), text);
        }
        Ok(())
    }

    /// These blocks have no provider identity — they are host observations
    /// minted inside the runtime, so they get deterministic
    /// `plugin-reasoning:{iteration}:{n}` ids and ordinals above every
    /// provider mint's band rather than borrowing the provider's space.
    async fn forward_plugin_reasoning(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        reasoning_deltas: Vec<String>,
        state: &mut LlmStreamState<'_>,
    ) {
        if !reasoning_deltas.iter().any(|delta| !delta.is_empty()) {
            return;
        }
        let index = *state.plugin_reasoning_blocks;
        *state.plugin_reasoning_blocks += 1;
        let block = StreamBlockIdentity::new(
            format!("plugin-reasoning:{}:{}", state.protocol_iteration, index),
            PLUGIN_BLOCK_ORDINAL_BASE + index,
        );
        state.reasoning_publication.record_streamed_block(&block);
        remember_attempt_correlation(
            state.reasoning_attempt_correlations,
            &TurnActivityId::new(block.id.clone()),
        );
        forwarder.forward_block_start(ProviderDeltaClass::Reasoning, block.clone());
        let mut block_text = String::new();
        for delta in reasoning_deltas {
            if delta.is_empty() {
                continue;
            }
            block_text.push_str(&delta);
            fold_llm_stream_event(
                state.stream_accumulator,
                state.streamed_usage,
                &LlmStreamEvent::ReasoningDelta {
                    block: block.clone(),
                    text: delta.clone(),
                },
            );
            forwarder.forward_delta(ProviderDeltaClass::Reasoning, block.clone(), delta);
        }
        forwarder.forward_block_end(ProviderDeltaClass::Reasoning, block, block_text);
    }

    async fn forward_provider_stream_event(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        stream_event: LlmStreamEvent,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        match stream_event {
            LlmStreamEvent::AttemptReset => {
                self.finish_assistant_stream_hooks(
                    crate::plugin::AssistantStreamFinishReason::AttemptReset,
                )
                .await;
                let assistant_prose_correlation_ids =
                    std::mem::take(state.assistant_prose_attempt_correlations);
                let reasoning_correlation_ids =
                    std::mem::take(state.reasoning_attempt_correlations);
                // The reset observes the provider generation boundary itself,
                // even when the discarded attempt produced no output. Empty
                // correlation lists are therefore meaningful host evidence.
                forwarder.send_semantic_turn_activity(
                    TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
                    TurnEvent::ModelAttemptReset {
                        assistant_prose_correlation_ids,
                        reasoning_correlation_ids,
                    },
                );
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::AttemptReset,
                );
                *state.stream_evidence = crate::LlmStreamEvidence::default();
                *state.text_streamed = false;
                *state.reasoning_publication = ReasoningPublicationState::default();
                *state.plugin_reasoning_blocks = 0;
                *state.completed_part_index = 0;
            }
            LlmStreamEvent::TextBlockStart { block } => {
                *state.text_streamed = true;
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::TextBlockStart {
                        block: block.clone(),
                    },
                );
                remember_attempt_correlation(
                    state.assistant_prose_attempt_correlations,
                    &TurnActivityId::new(block.id.clone()),
                );
                forwarder.forward_block_start(ProviderDeltaClass::AssistantProse, block);
            }
            LlmStreamEvent::Delta { block, text } => {
                self.emit_visible_assistant_text(forwarder, text, &block, "delta", state)
                    .await?;
            }
            LlmStreamEvent::TextBlockEnd { block, text } => {
                // The end event's text is authoritative for the block. Only
                // content beyond what streamed as deltas may go through the
                // plugin stream transform — a stateful chunk hook must never
                // see the same text twice.
                let raw_text = self
                    .host
                    .core
                    .tracing
                    .trace_sink
                    .as_ref()
                    .map(|_| text.clone());
                let raw_accumulated = state
                    .block_raw_text
                    .get(&block.id)
                    .cloned()
                    .unwrap_or_default();
                let prefix_extension = text.starts_with(raw_accumulated.as_str());
                if prefix_extension {
                    // A completion that extends the streamed prefix forwards
                    // only the unseen tail — covers zero-delta blocks and
                    // non-streamed final-message reconciliation alike.
                    let tail = text[raw_accumulated.len()..].to_string();
                    self.emit_visible_assistant_text(forwarder, tail, &block, "delta", state)
                        .await?;
                }
                // Prefix extensions seal with the post-transform total hosts
                // accumulated from deltas; a non-prefix completion is a
                // correction and seals with the provider's authoritative text.
                let sealed = if prefix_extension {
                    state
                        .stream_accumulator
                        .block_text(&block)
                        .unwrap_or_else(|| text.clone())
                } else {
                    text.clone()
                };
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "text_block_end",
                        text: LlmDebugText {
                            raw: raw_text.as_deref(),
                            visible: Some(&sealed),
                        },
                        item_id: block.item_id.as_deref(),
                        block_id: Some(block.id.as_str()),
                        usage: None,
                        tool_call: None,
                    },
                );
                *state.text_streamed = true;
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::TextBlockEnd {
                        block: block.clone(),
                        text,
                    },
                );
                remember_attempt_correlation(
                    state.assistant_prose_attempt_correlations,
                    &TurnActivityId::new(block.id.clone()),
                );
                forwarder.forward_block_end(ProviderDeltaClass::AssistantProse, block, sealed);
            }
            LlmStreamEvent::ReasoningBlockStart { block } => {
                state.reasoning_publication.record_streamed_block(&block);
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::ReasoningBlockStart {
                        block: block.clone(),
                    },
                );
                remember_attempt_correlation(
                    state.reasoning_attempt_correlations,
                    &TurnActivityId::new(block.id.clone()),
                );
                forwarder.forward_block_start(ProviderDeltaClass::Reasoning, block);
            }
            LlmStreamEvent::ReasoningDelta { block, text } => {
                state.reasoning_publication.record_streamed_block(&block);
                if !text.is_empty() {
                    self.log_llm_stream_event(
                        state.debug,
                        LlmStreamEventLog {
                            protocol_iteration: state.protocol_iteration,
                            event_type: "reasoning_delta",
                            text: LlmDebugText {
                                raw: None,
                                visible: Some(&text),
                            },
                            item_id: block.item_id.as_deref(),
                            block_id: Some(block.id.as_str()),
                            usage: None,
                            tool_call: None,
                        },
                    );
                    fold_llm_stream_event(
                        state.stream_accumulator,
                        state.streamed_usage,
                        &LlmStreamEvent::ReasoningDelta {
                            block: block.clone(),
                            text: text.clone(),
                        },
                    );
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &TurnActivityId::new(block.id.clone()),
                    );
                    forwarder.forward_delta(ProviderDeltaClass::Reasoning, block, text);
                }
            }
            LlmStreamEvent::ReasoningBlockEnd { block, text } => {
                state.reasoning_publication.record_streamed_block(&block);
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "reasoning_block_end",
                        text: LlmDebugText {
                            raw: None,
                            visible: Some(&text),
                        },
                        item_id: block.item_id.as_deref(),
                        block_id: Some(block.id.as_str()),
                        usage: None,
                        tool_call: None,
                    },
                );
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::ReasoningBlockEnd {
                        block: block.clone(),
                        text: text.clone(),
                    },
                );
                remember_attempt_correlation(
                    state.reasoning_attempt_correlations,
                    &TurnActivityId::new(block.id.clone()),
                );
                forwarder.forward_block_end(ProviderDeltaClass::Reasoning, block, text);
            }
            LlmStreamEvent::Part(LlmOutputPart::Text {
                text,
                response_meta,
            }) => {
                let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "text_part",
                        text: LlmDebugText {
                            raw: Some(&text),
                            visible: None,
                        },
                        item_id: item_id.as_deref(),
                        block_id: None,
                        usage: None,
                        tool_call: None,
                    },
                );
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::Part(LlmOutputPart::Text {
                        text,
                        response_meta,
                    }),
                );
            }
            LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                call_id,
                tool_name,
                input_json,
                replay,
            }) => {
                let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_deref());
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "tool_call_part",
                        text: LlmDebugText {
                            raw: None,
                            visible: None,
                        },
                        item_id,
                        block_id: None,
                        usage: None,
                        tool_call: Some(LlmDebugToolCall {
                            call_id: &call_id,
                            tool_name: &tool_name,
                            input_json: &input_json,
                        }),
                    },
                );
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::Part(LlmOutputPart::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                    }),
                );
            }
            LlmStreamEvent::Part(LlmOutputPart::Reasoning { text, replay }) => {
                let part = LlmOutputPart::Reasoning {
                    text: text.clone(),
                    replay: replay.clone(),
                };
                let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_deref());
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "reasoning_part",
                        text: LlmDebugText {
                            raw: Some(&text),
                            visible: None,
                        },
                        item_id,
                        block_id: None,
                        usage: None,
                        tool_call: None,
                    },
                );
                // Item-level completion: replay material (encrypted content,
                // signatures, summary) rides here, while any of the item's
                // blocks that never streamed publish now as complete blocks —
                // boundaries the live path already emitted are not repeated.
                let part_index = *state.completed_part_index;
                *state.completed_part_index += 1;
                let mut next_ordinal = state.reasoning_publication.next_block_ordinal();
                let unpublished = state.reasoning_publication.unpublished_blocks(
                    part_index,
                    &part,
                    &mut next_ordinal,
                );
                for (block, block_text) in unpublished {
                    state.reasoning_publication.record_streamed_block(&block);
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &TurnActivityId::new(block.id.clone()),
                    );
                    forwarder.forward_block_start(ProviderDeltaClass::Reasoning, block.clone());
                    forwarder.forward_delta(
                        ProviderDeltaClass::Reasoning,
                        block.clone(),
                        block_text.clone(),
                    );
                    forwarder.forward_block_end(ProviderDeltaClass::Reasoning, block, block_text);
                }
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::Part(part),
                );
            }
            LlmStreamEvent::Usage(usage) => {
                self.log_llm_stream_event(
                    state.debug,
                    LlmStreamEventLog {
                        protocol_iteration: state.protocol_iteration,
                        event_type: "usage",
                        text: LlmDebugText {
                            raw: None,
                            visible: None,
                        },
                        item_id: None,
                        block_id: None,
                        usage: Some(&usage),
                        tool_call: None,
                    },
                );
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::Usage(usage),
                );
            }
            LlmStreamEvent::Evidence(evidence) => {
                state.stream_evidence.merge(evidence).map_err(|error| {
                    let code = FailureCode::lash(TurnFailureCode::from_wire(error.code()));
                    LlmCallError {
                        message: error.to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Stream,
                        raw: None,
                        code: Some(code),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: state.stream_evidence.request_body.clone(),
                        partial_response: None,
                    }
                })?;
            }
            LlmStreamEvent::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
            } => {
                forwarder.send_semantic_session_event(SessionStreamEvent::RetryStatus {
                    wait_seconds,
                    attempt,
                    max_attempts,
                    reason,
                    envelope: None,
                });
            }
        }
        Ok(())
    }

    /// `AttemptReset` is a hard boundary: the completed response belongs to the accepted
    /// attempt and must not be cleared by a provider retry that raced with cancellation.
    /// If the deadline wins, an uncooperative provider's late usage is unavailable for this
    /// attempt and the sealed record says so
    /// (`AttemptUsageDisposition::UnreportedAfterAbort`).
    /// The deadline is the host's `abort_drain_grace` lever, not a literal.
    async fn collect_trailing_stream_events_before_abort<T>(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        llm_task: &mut tokio::task::JoinHandle<T>,
        llm_stream_rx: &mut tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        let deadline = self.host.core.clock.now() + self.host.core.control.abort_drain_grace;
        loop {
            tokio::select! {
                _ = self.host.core.clock.sleep_until(deadline) => break,
                event = llm_stream_rx.recv() => match event {
                    None | Some(LlmStreamEvent::AttemptReset) => break,
                    Some(event) => {
                        self.forward_provider_stream_event(forwarder, event, state).await?;
                    }
                },
            }
        }
        llm_task.abort();
        Ok(())
    }

    async fn drain_provider_stream_queue(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        llm_stream_rx: &mut tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        while let Ok(stream_event) = llm_stream_rx.try_recv() {
            self.forward_provider_stream_event(forwarder, stream_event, state)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "streaming/tests.rs"]
mod provider_host_forwarding_tests;

#[cfg(test)]
mod clamp_report_tests {
    use super::*;

    fn applied() -> Option<crate::GenerationReceipt> {
        Some(crate::GenerationReceipt {
            output_token_cap: crate::GenerationOptionOutcome::Applied,
            temperature: crate::GenerationOptionOutcome::Applied,
            seed: crate::GenerationOptionOutcome::NotRequested,
            stop_sequences: crate::GenerationOptionOutcome::NotRequested,
            cache: crate::GenerationOptionOutcome::NotRequested,
        })
    }

    fn attempt(generation_disposition: Option<crate::GenerationReceipt>) -> crate::AttemptRecord {
        crate::AttemptRecord {
            ordinal: 1,
            started_at: 0,
            duration: std::time::Duration::ZERO,
            outcome: crate::AttemptOutcome::Completed,
            protocol_position: crate::ProtocolPosition::OutputStarted,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition,
            usage: None,
            usage_disposition: Default::default(),
        }
    }
    fn call_record(attempts: Vec<crate::AttemptRecord>) -> crate::LlmCallRecord {
        crate::LlmCallRecord {
            call_id: crate::LlmCallId("call".to_string()),
            label: None,
            replay_drops: Vec::new(),
            attempts,
        }
    }
    fn cap_of(disposition: Option<crate::GenerationReceipt>) -> crate::GenerationOptionOutcome {
        disposition
            .expect("a reported disposition")
            .output_token_cap
    }

    /// A failed call still leaves accounts of itself behind: the ledger
    /// attempt, and the partial response an adapter salvaged onto the error.
    /// Narrowing one and not the other is how the same attempt comes to say
    /// two different things.
    #[test]
    fn a_failed_calls_partial_response_agrees_with_its_ledger_attempt() {
        let mut result: Result<LlmResponse, LlmCallError> = Err(LlmCallError {
            message: "stream ended early".to_string(),
            retryable: false,
            kind: crate::ProviderFailureKind::Unknown,
            raw: None,
            code: None,
            terminal_reason: crate::LlmTerminalReason::ProviderError,
            request_body: None,
            partial_response: Some(Box::new(LlmResponse {
                generation_disposition: applied(),
                ..LlmResponse::default()
            })),
        });
        let mut call_record = call_record(vec![crate::AttemptRecord {
            ordinal: 1,
            started_at: 0,
            duration: std::time::Duration::ZERO,
            outcome: crate::AttemptOutcome::Failed,
            protocol_position: crate::ProtocolPosition::OutputStarted,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: applied(),
            usage: None,
            usage_disposition: Default::default(),
        }]);

        record_clamped_output_token_cap(&mut result, Some(&mut call_record));

        let partial = result
            .expect_err("the call failed")
            .partial_response
            .expect("the adapter salvaged a partial");
        assert_eq!(
            cap_of(partial.generation_disposition),
            crate::GenerationOptionOutcome::ClampedToCapacity
        );
        assert_eq!(
            cap_of(call_record.attempts[0].generation_disposition),
            crate::GenerationOptionOutcome::ClampedToCapacity
        );
    }

    /// An adapter that reports nothing keeps reporting nothing, and an option
    /// the adapter dropped is not overwritten with a clamp it never applied.
    #[test]
    fn narrowing_only_touches_a_cap_the_adapter_reported_as_applied() {
        let mut unreported: Result<LlmResponse, LlmCallError> = Ok(LlmResponse::default());
        record_clamped_output_token_cap(&mut unreported, None);
        assert!(
            unreported.expect("ok").generation_disposition.is_none(),
            "None means unreported, not an invitation to invent a report"
        );

        let mut dropped: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
            generation_disposition: Some(crate::GenerationReceipt {
                output_token_cap: crate::GenerationOptionOutcome::OmittedUnsupported,
                ..Default::default()
            }),
            ..LlmResponse::default()
        });
        record_clamped_output_token_cap(&mut dropped, None);
        assert_eq!(
            cap_of(dropped.expect("ok").generation_disposition),
            crate::GenerationOptionOutcome::OmittedUnsupported
        );
    }

    #[test]
    fn protocol_stop_suppression_updates_response_and_attempt_ledger() {
        let mut result: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
            generation_disposition: applied(),
            ..LlmResponse::default()
        });
        let mut call_record = call_record(vec![attempt(applied())]);

        record_protocol_owned_stop_suppression(&mut result, Some(&mut call_record));

        let response = result.expect("response");
        assert_eq!(
            response
                .generation_disposition
                .expect("response disposition")
                .stop_sequences,
            crate::GenerationOptionOutcome::SuppressedProtocolOwned
        );
        assert_eq!(
            call_record.attempts[0]
                .generation_disposition
                .expect("attempt disposition")
                .stop_sequences,
            crate::GenerationOptionOutcome::SuppressedProtocolOwned
        );
    }

    #[test]
    fn protocol_stop_suppression_leaves_unreported_attempts_absent() {
        let mut result: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
            generation_disposition: applied(),
            ..LlmResponse::default()
        });
        let mut call_record = call_record(vec![attempt(None), attempt(applied())]);

        record_protocol_owned_stop_suppression(&mut result, Some(&mut call_record));

        assert!(call_record.attempts[0].generation_disposition.is_none());
        assert_eq!(
            call_record.attempts[1]
                .generation_disposition
                .expect("reported attempt disposition")
                .stop_sequences,
            crate::GenerationOptionOutcome::SuppressedProtocolOwned
        );
    }
}

#[cfg(test)]
#[path = "streaming_protocol_abort_tests.rs"]
mod protocol_abort_evidence_tests;
