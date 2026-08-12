//! Provider stream assembly and host forwarding are deliberately separate.
//!
//! Committed blocks and checkpoints are authoritative. Deltas describe
//! provider-wire volume, so hosts may observe fewer, larger delta events under
//! delivery lag without changing transcript content. Pending deltas therefore
//! coalesce losslessly by correlation; they are never committed state.

use std::sync::Arc;

use lash_trace::{
    TraceError, TraceEvent, TraceProviderRequestEvent, TraceProviderStreamEvent,
    TraceRuntimeStreamEvent,
};

use super::*;

mod host_forwarder;

use host_forwarder::{ProviderDeltaClass, ProviderHostForwarder};

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

async fn emit_plugin_runtime_events_runtime(
    forwarder: &mut ProviderHostForwarder<'_>,
    plugin_id: &str,
    events: Vec<crate::PluginRuntimeEvent>,
) {
    for event in crate::plugin::plugin_runtime_session_events(plugin_id, events) {
        forwarder.send_semantic_session_event(event).await;
    }
}

/// Report the clamp on every disposition the call produced.
///
/// The adapter knows only that it put the cap it was handed on the wire, so it
/// reports `Applied`; the runtime is the only layer that saw the larger number
/// the caller asked for. This narrows that report on every carrier of it — the
/// response, each attempt of the ledger, and the partial response an error
/// carries when the adapter salvaged one — so no two accounts of the same
/// request disagree. An adapter that reports nothing keeps reporting nothing:
/// `None` means unreported, not "nothing happened".
fn record_clamped_output_token_cap(
    result: &mut Result<LlmResponse, LlmCallError>,
    call_record: Option<&mut crate::LlmCallRecord>,
) {
    fn narrow(disposition: Option<&mut crate::GenerationDisposition>) {
        if let Some(disposition) = disposition
            && disposition.output_token_cap == crate::GenerationOptionDisposition::Applied
        {
            disposition.output_token_cap = crate::GenerationOptionDisposition::ClampedToCapacity;
        }
    }

    match result {
        Ok(response) => narrow(response.generation_disposition.as_mut()),
        Err(error) => narrow(
            error
                .partial_response
                .as_deref_mut()
                .and_then(|partial| partial.generation_disposition.as_mut()),
        ),
    }
    if let Some(call_record) = call_record {
        for attempt in &mut call_record.attempts {
            narrow(attempt.generation_disposition.as_mut());
        }
    }
}

/// Narrow the adapter's wire-level report when protocol projection replaced
/// caller-owned stop sequences before the request reached the adapter.
fn record_protocol_owned_stop_replacement(
    result: &mut Result<LlmResponse, LlmCallError>,
    call_record: Option<&mut crate::LlmCallRecord>,
) {
    fn replace(disposition: &mut Option<crate::GenerationDisposition>) {
        disposition.get_or_insert_default().stop_sequences =
            crate::GenerationOptionDisposition::ReplacedProtocolOwned;
    }

    match result {
        Ok(response) => replace(&mut response.generation_disposition),
        Err(error) => {
            if let Some(partial) = error.partial_response.as_deref_mut() {
                replace(&mut partial.generation_disposition);
            }
        }
    }
    if let Some(call_record) = call_record {
        for attempt in &mut call_record.attempts {
            replace(&mut attempt.generation_disposition);
        }
    }
}

impl RuntimeTurnDriver<'_> {
    pub(super) async fn invoke_turn_llm_effect(
        &mut self,
        machine: &mut TurnMachine,
        id: crate::sansio::EffectId,
        request: Arc<LlmRequest>,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> Result<RuntimeLlmCallOutcome, RuntimeEffectControllerError> {
        let invocation = self.turn_effect_invocation(machine, id, RuntimeEffectKind::LlmCall)?;
        self.execute_typed_turn_effect(
            machine,
            event_tx,
            cancel,
            RuntimeEffectEnvelope::new(
                invocation,
                RuntimeEffectCommand::LlmCall {
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
                code: Some("plugin_assistant_stream".to_string()),
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
            emit_plugin_runtime_events_runtime(forwarder, &emitted.plugin_id, emitted.value.events)
                .await;
        }
        let chunk = if first { original } else { current };
        Ok(StreamChunkOutcome {
            chunk,
            reasoning_deltas,
            abort_requested,
        })
    }

    async fn transform_assistant_response(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        response: LlmResponse,
    ) -> Result<LlmResponse, LlmCallError> {
        let original = response.clone();
        let transforms = self
            .session
            .plugins()
            .transform_assistant_response(&self.session_id, response)
            .await
            .map_err(|err| LlmCallError {
                message: err.to_string(),
                retryable: false,
                kind: crate::ProviderFailureKind::Unknown,
                raw: None,
                code: Some("plugin_assistant_response".to_string()),
                terminal_reason: crate::LlmTerminalReason::ProviderError,
                request_body: None,
                partial_response: None,
            })?;
        let mut current: Option<LlmResponse> = None;
        for emitted in transforms {
            emit_plugin_runtime_events_runtime(forwarder, &emitted.plugin_id, emitted.value.events)
                .await;
            current = Some(emitted.value.response);
        }
        Ok(current.unwrap_or(original))
    }

    pub(in crate::runtime) async fn run_llm_call(
        &mut self,
        request: Arc<LlmRequest>,
        protocol_iteration: usize,
        invocation: crate::RuntimeInvocation,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        cancel: &CancellationToken,
    ) -> RuntimeLlmCallOutcome {
        let mut request = (*request).clone();
        let protocol_replaced_stop_sequences =
            request.generation.stop_sequences_replaced_by_protocol();
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
                return (
                    Err(LlmCallError {
                        message: err.to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Unknown,
                        raw: None,
                        code: Some("attachment_resolution_failed".to_string()),
                        terminal_reason: crate::LlmTerminalReason::ProviderError,
                        request_body: None,
                        partial_response: None,
                    }),
                    false,
                    None,
                );
            }
        };
        let trace_enabled = self.host.core.tracing.trace_sink.is_some();
        let llm_call_id = trace_enabled.then(|| self.llm_call_id(protocol_iteration));
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
        let llm_request = LlmRequest {
            scope: crate::LlmRequestScope::new(
                self.session_id.clone(),
                self.turn_pipeline
                    .state()
                    .current_frame_node_id
                    .clone()
                    .unwrap_or_default(),
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

        let mut call_provider = self.policy.provider().clone();
        let mut llm_task = crate::task::spawn(async move {
            let result = call_provider.complete(llm_request).await;
            (result, call_provider)
        });
        let mut llm_task_abort = AbortOnDrop::new(llm_task.abort_handle());

        let mut text_streamed = false;
        let mut streamed_usage = LlmUsage::default();
        let mut stream_accumulator = LlmStreamAccumulator::default();
        let mut abort_requested = false;
        let mut assistant_prose_correlation = None;
        let mut reasoning_correlation = None;
        let mut assistant_prose_attempt_correlations = Vec::new();
        let mut reasoning_attempt_correlations = Vec::new();
        let mut stream_state = LlmStreamState {
            text_streamed: &mut text_streamed,
            streamed_usage: &mut streamed_usage,
            stream_accumulator: &mut stream_accumulator,
            debug: &mut debug,
            protocol_iteration,
            assistant_prose_correlation: &mut assistant_prose_correlation,
            reasoning_correlation: &mut reasoning_correlation,
            assistant_prose_attempt_correlations: &mut assistant_prose_attempt_correlations,
            reasoning_attempt_correlations: &mut reasoning_attempt_correlations,
            abort_requested: &mut abort_requested,
        };
        let mut host_forwarder = ProviderHostForwarder::new(event_tx);
        let mut call_record = None;
        let result = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    llm_task.abort();
                    break Err(LlmCallError {
                        message: "cancelled".to_string(),
                        retryable: false,
                        kind: crate::ProviderFailureKind::Unknown,
                        raw: None,
                        code: Some("cancelled".to_string()),
                        terminal_reason: crate::LlmTerminalReason::Cancelled,
                        request_body: None,
                        partial_response: None,
                    });
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
                        let final_streamed_usage = stream_state.streamed_usage.clone();
                        let mut resp = LlmResponse {
                            full_text: stream_state.stream_accumulator.full_text(),
                            parts: Vec::new(),
                            usage: final_streamed_usage,
                            terminal_reason: crate::LlmTerminalReason::Stop,
                            terminal_diagnostic: None,
                            provider_usage: None,
                            request_body: None,
                            http_summary: None,
                            execution_evidence: None,
                            generation_disposition: None,
                            response_metadata: Default::default(),
                        };
                        stream_state.stream_accumulator.apply_to_response(&mut resp);
                        let resp = match self
                            .transform_assistant_response(&mut host_forwarder, resp)
                            .await
                        {
                            Ok(resp) => resp,
                            Err(err) => break Err(err),
                        };

                        break Ok(resp);
                    }
                }
                join = &mut llm_task => {
                    let (result, provider_after) = match join {
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
                                    }),
                                    error: Some(crate::NormalizedError {
                                        class: crate::ProviderFailureKind::Unknown.code().to_string(),
                                        provider_code: Some("provider_panicked".to_string()),
                                        http_status: None,
                                        provider_request_id: None,
                                        retry_after: None,
                                        diagnostic: Some(message.clone()),
                                    }),
                                    evidence: None,
                                    generation_disposition: None,
                                    usage: None,
                                }],
                            });
                            let failure = LlmCallError {
                                message,
                                retryable: false,
                                kind: crate::ProviderFailureKind::Unknown,
                                raw: None,
                                code: Some("provider_panicked".to_string()),
                                terminal_reason: crate::LlmTerminalReason::ProviderError,
                                request_body: None,
                                partial_response: None,
                            };
                            drop(payload);
                            break Err(failure);
                        }
                        Err(e) => break Err(LlmCallError {
                            message: format!("internal task failed: {e}"),
                            retryable: false,
                            kind: crate::ProviderFailureKind::Unknown,
                            raw: None,
                            code: Some("task_join_failed".to_string()),
                            terminal_reason: crate::LlmTerminalReason::ProviderError,
                            request_body: None,
                            partial_response: None,
                        }),
                    };
                    self.policy.binding = match crate::ProviderBinding::new(
                        self.policy.binding.provider_id.clone(),
                        provider_after,
                    ) {
                        Ok(binding) => binding,
                        Err(err) => break Err(LlmCallError {
                            message: err.to_string(),
                            retryable: false,
                            kind: crate::ProviderFailureKind::Unknown,
                            raw: None,
                            code: Some("provider_binding_mismatch".to_string()),
                            terminal_reason: crate::LlmTerminalReason::ProviderError,
                            request_body: None,
                            partial_response: None,
                        }),
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
                            let resp = match self
                                .transform_assistant_response(&mut host_forwarder, resp)
                                .await
                            {
                                Ok(resp) => resp,
                                Err(err) => break Err(err),
                            };
                            break Ok(resp)
                        }
                        Err(e) => {
                            let crate::ProviderCompletionError {
                                error: e,
                                call_record: failed_call_record,
                            } = e;
                            call_record = Some(failed_call_record);
                            break Err(LlmCallError {
                                message: e.message,
                                retryable: e.retryable,
                                kind: e.kind,
                                raw: e.raw.map(|raw| *raw),
                                code: e.code,
                                terminal_reason: e.terminal_reason,
                                request_body: e.request_body,
                                partial_response: e.partial_response,
                            });
                        }
                    }
                }
            }
        };

        let mut result = result;
        let cancelled = matches!(
            &result,
            Err(err) if err.terminal_reason == crate::LlmTerminalReason::Cancelled
        );
        host_forwarder.finish(cancelled).await;
        if clamped_output_token_cap {
            record_clamped_output_token_cap(&mut result, call_record.as_mut());
        }
        if protocol_replaced_stop_sequences {
            record_protocol_owned_stop_replacement(&mut result, call_record.as_mut());
        }

        self.finish_assistant_stream_hooks(assistant_stream_finish_reason(
            &result,
            abort_requested,
        ))
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
        if trace_enabled {
            self.llm_stream_summaries
                .insert(protocol_iteration, debug.summary);
        }
        (result, text_streamed, call_record)
    }

    async fn finish_assistant_stream_hooks(
        &mut self,
        reason: crate::plugin::AssistantStreamFinishReason,
    ) {
        if !self.session.plugins().has_assistant_stream_finished_hooks() {
            return;
        }
        if let Err(err) = self
            .session
            .plugins()
            .finish_assistant_stream(&self.session_id, reason)
            .await
        {
            tracing::error!(
                session_id = %self.session_id,
                reason = ?reason,
                error = %err,
                "assistant stream cleanup hook failed"
            );
        }
    }

    pub(super) fn handle_log_event(&mut self, event: crate::sansio::LogEvent) {
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
                let stream_summary = self.llm_stream_summaries.remove(&protocol_iteration);
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
                            None,
                            response_parts,
                            None,
                        ),
                        usage: Some(crate::trace::trace_usage_from_session(&usage)),
                        provider_usage,
                        stream_summary: stream_summary.map(|summary| summary.to_json()),
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
                terminal_reason,
                ..
            } => {
                let stream_summary = self.llm_stream_summaries.remove(&protocol_iteration);
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
                            code,
                            raw,
                        },
                        stream_summary: stream_summary.map(|summary| summary.to_json()),
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

    /// Shared visible-assistant-text path for streamed text, used by both the
    /// `Delta` (item-less) and `Part::Text` (item-scoped) provider events.
    ///
    /// Sets the `text_streamed` flag, runs the chunk through plugin stream
    /// transforms (forwarding any reasoning deltas + abort request), logs the
    /// event, and emits the visible prose deltas.
    async fn emit_visible_assistant_text(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        text: String,
        item_id: Option<&str>,
        event_type: &'static str,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        if text.is_empty() {
            return Ok(());
        }
        *state.text_streamed = true;
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
        for reasoning_delta in outcome.reasoning_deltas {
            fold_llm_stream_event(
                state.stream_accumulator,
                state.streamed_usage,
                &LlmStreamEvent::ReasoningDelta(reasoning_delta.clone()),
            );
            let correlation_id = stream_correlation_id(state.reasoning_correlation, None);
            remember_attempt_correlation(state.reasoning_attempt_correlations, &correlation_id);
            forwarder.forward_delta(
                ProviderDeltaClass::Reasoning,
                correlation_id,
                reasoning_delta,
            );
        }
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
                item_id,
                usage: None,
                tool_call: None,
            },
        );
        if !text.is_empty() {
            fold_llm_stream_event(
                state.stream_accumulator,
                state.streamed_usage,
                &LlmStreamEvent::Delta(text.clone()),
            );
            let correlation_id = stream_correlation_id(state.assistant_prose_correlation, item_id);
            remember_attempt_correlation(
                state.assistant_prose_attempt_correlations,
                &correlation_id,
            );
            forwarder.forward_delta(ProviderDeltaClass::AssistantProse, correlation_id, text);
        }
        Ok(())
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
                forwarder
                    .send_semantic_turn_activity(
                        TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
                        TurnEvent::ModelAttemptReset {
                            assistant_prose_correlation_ids,
                            reasoning_correlation_ids,
                        },
                    )
                    .await;
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::AttemptReset,
                );
                *state.text_streamed = false;
                *state.assistant_prose_correlation = None;
                *state.reasoning_correlation = None;
            }
            LlmStreamEvent::Delta(delta) => {
                self.emit_visible_assistant_text(forwarder, delta, None, "delta", state)
                    .await?;
            }
            LlmStreamEvent::ReasoningDelta(delta) => {
                if !delta.is_empty() {
                    self.log_llm_stream_event(
                        state.debug,
                        LlmStreamEventLog {
                            protocol_iteration: state.protocol_iteration,
                            event_type: "reasoning_delta",
                            text: LlmDebugText {
                                raw: None,
                                visible: Some(&delta),
                            },
                            item_id: None,
                            usage: None,
                            tool_call: None,
                        },
                    );
                    // Delta-only streaming path (fix 1.3a display). No
                    // encrypted content yet — that arrives with the full
                    // item on `output_item.done` (fix 1.3b).
                    fold_llm_stream_event(
                        state.stream_accumulator,
                        state.streamed_usage,
                        &LlmStreamEvent::ReasoningDelta(delta.clone()),
                    );
                    let correlation_id = stream_correlation_id(state.reasoning_correlation, None);
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &correlation_id,
                    );
                    forwarder.forward_delta(ProviderDeltaClass::Reasoning, correlation_id, delta);
                }
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
                let item_id = replay.as_ref().and_then(|meta| meta.item_id.as_deref());
                if !text.is_empty() {
                    self.log_llm_stream_event(
                        state.debug,
                        LlmStreamEventLog {
                            protocol_iteration: state.protocol_iteration,
                            event_type: "reasoning_part",
                            text: LlmDebugText {
                                raw: None,
                                visible: Some(&text),
                            },
                            item_id,
                            usage: None,
                            tool_call: None,
                        },
                    );
                    let correlation_id =
                        stream_correlation_id(state.reasoning_correlation, item_id);
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &correlation_id,
                    );
                    forwarder.forward_delta(
                        ProviderDeltaClass::Reasoning,
                        correlation_id,
                        text.clone(),
                    );
                }
                fold_llm_stream_event(
                    state.stream_accumulator,
                    state.streamed_usage,
                    &LlmStreamEvent::Part(LlmOutputPart::Reasoning { text, replay }),
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
            LlmStreamEvent::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
            } => {
                forwarder
                    .send_semantic_session_event(SessionStreamEvent::RetryStatus {
                        wait_seconds,
                        attempt,
                        max_attempts,
                        reason,
                        envelope: None,
                    })
                    .await;
            }
        }
        Ok(())
    }

    /// Wait briefly for provider events emitted after a protocol-owned abort.
    /// `AttemptReset` is a hard boundary: the completed response belongs to
    /// the accepted attempt and must not be cleared by a provider retry that
    /// raced with cancellation. If the deadline wins, an uncooperative
    /// provider's late usage is unavailable for this attempt.
    async fn collect_trailing_stream_events_before_abort<T>(
        &mut self,
        forwarder: &mut ProviderHostForwarder<'_>,
        llm_task: &mut tokio::task::JoinHandle<T>,
        llm_stream_rx: &mut tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        let deadline = self.host.core.clock.now() + std::time::Duration::from_millis(2_000);
        loop {
            tokio::select! {
                _ = self.host.core.clock.sleep_until(deadline) => break,
                event = llm_stream_rx.recv() => match event {
                    None | Some(LlmStreamEvent::AttemptReset) => break,
                    Some(event) => {
                        let has_final_usage = matches!(event, LlmStreamEvent::Usage(_));
                        self.forward_provider_stream_event(forwarder, event, state).await?;
                        if has_final_usage {
                            break;
                        }
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

fn assistant_stream_finish_reason(
    result: &Result<LlmResponse, LlmCallError>,
    abort_requested: bool,
) -> crate::plugin::AssistantStreamFinishReason {
    use crate::plugin::AssistantStreamFinishReason;

    if abort_requested && result.is_ok() {
        return AssistantStreamFinishReason::Aborted;
    }
    match result {
        Ok(_) => AssistantStreamFinishReason::Complete,
        Err(err) if err.terminal_reason == crate::LlmTerminalReason::Cancelled => {
            AssistantStreamFinishReason::Cancelled
        }
        Err(_) => AssistantStreamFinishReason::ProviderError,
    }
}

struct AbortOnDrop {
    handle: tokio::task::AbortHandle,
    armed: bool,
}

impl AbortOnDrop {
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

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.handle.abort();
        }
    }
}

fn response_usage_is_empty(usage: &LlmUsage) -> bool {
    usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_input_tokens == 0
        && usage.cache_write_input_tokens == 0
        && usage.reasoning_output_tokens == 0
}

fn provider_item_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("item_id")
        .or_else(|| value.get("item").and_then(|item| item.get("id")))
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("id"))
        })
        .or_else(|| value.get("id"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn provider_output_index(value: &serde_json::Value) -> Option<i64> {
    value
        .get("output_index")
        .or_else(|| value.get("index"))
        .and_then(|value| value.as_i64())
}

fn stream_correlation_id(
    fallback_slot: &mut Option<TurnActivityId>,
    provider_item_id: Option<&str>,
) -> TurnActivityId {
    if let Some(provider_item_id) = provider_item_id {
        return TurnActivityId::new(provider_item_id.to_string());
    }
    fallback_slot
        .get_or_insert_with(|| TurnActivityId::new(uuid::Uuid::new_v4().to_string()))
        .clone()
}

fn remember_attempt_correlation(
    correlations: &mut Vec<TurnActivityId>,
    correlation_id: &TurnActivityId,
) {
    if !correlations.contains(correlation_id) {
        correlations.push(correlation_id.clone());
    }
}

#[cfg(test)]
#[path = "streaming/tests.rs"]
mod provider_host_forwarding_tests;

#[cfg(test)]
mod clamp_report_tests {
    use super::*;

    fn applied() -> Option<crate::GenerationDisposition> {
        Some(crate::GenerationDisposition {
            output_token_cap: crate::GenerationOptionDisposition::Applied,
            temperature: crate::GenerationOptionDisposition::Applied,
            seed: crate::GenerationOptionDisposition::NotRequested,
            stop_sequences: crate::GenerationOptionDisposition::NotRequested,
            cache: crate::GenerationOptionDisposition::NotRequested,
        })
    }

    fn cap_of(
        disposition: Option<crate::GenerationDisposition>,
    ) -> crate::GenerationOptionDisposition {
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
        let mut call_record = crate::LlmCallRecord {
            call_id: crate::LlmCallId("call".to_string()),
            label: None,
            attempts: vec![crate::AttemptRecord {
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
            }],
        };

        record_clamped_output_token_cap(&mut result, Some(&mut call_record));

        let partial = result
            .expect_err("the call failed")
            .partial_response
            .expect("the adapter salvaged a partial");
        assert_eq!(
            cap_of(partial.generation_disposition),
            crate::GenerationOptionDisposition::ClampedToCapacity
        );
        assert_eq!(
            cap_of(call_record.attempts[0].generation_disposition),
            crate::GenerationOptionDisposition::ClampedToCapacity
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
            generation_disposition: Some(crate::GenerationDisposition {
                output_token_cap: crate::GenerationOptionDisposition::OmittedUnsupported,
                ..Default::default()
            }),
            ..LlmResponse::default()
        });
        record_clamped_output_token_cap(&mut dropped, None);
        assert_eq!(
            cap_of(dropped.expect("ok").generation_disposition),
            crate::GenerationOptionDisposition::OmittedUnsupported
        );
    }

    #[test]
    fn protocol_stop_replacement_updates_response_and_attempt_ledger() {
        let mut result: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
            generation_disposition: applied(),
            ..LlmResponse::default()
        });
        let mut call_record = crate::LlmCallRecord {
            call_id: crate::LlmCallId("call".to_string()),
            label: None,
            attempts: vec![crate::AttemptRecord {
                ordinal: 1,
                started_at: 0,
                duration: std::time::Duration::ZERO,
                outcome: crate::AttemptOutcome::Completed,
                protocol_position: crate::ProtocolPosition::OutputStarted,
                retry_budget_consumed: false,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: applied(),
                usage: None,
            }],
        };

        record_protocol_owned_stop_replacement(&mut result, Some(&mut call_record));

        let response = result.expect("response");
        assert_eq!(
            response
                .generation_disposition
                .expect("response disposition")
                .stop_sequences,
            crate::GenerationOptionDisposition::ReplacedProtocolOwned
        );
        assert_eq!(
            call_record.attempts[0]
                .generation_disposition
                .expect("attempt disposition")
                .stop_sequences,
            crate::GenerationOptionDisposition::ReplacedProtocolOwned
        );
    }
}
