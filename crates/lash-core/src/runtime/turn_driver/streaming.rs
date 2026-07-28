use std::sync::Arc;

use lash_trace::{
    TraceError, TraceEvent, TraceProviderRequestEvent, TraceProviderStreamEvent,
    TraceRuntimeStreamEvent,
};

use super::*;

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
    event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    plugin_id: &str,
    events: Vec<crate::PluginRuntimeEvent>,
) {
    for event in crate::plugin::plugin_runtime_session_events(plugin_id, events) {
        send_session_event(event_tx, event).await;
    }
}

/// Reduce a requested output-token cap to what the model can produce, and say
/// whether it had to.
///
/// The cap is a bound, not a demand: a session asking for at most 32k against
/// a model that tops out at 8k is satisfied by 8k. Failing instead made the
/// one fail-closed option in an otherwise fail-silent struct — and because the
/// options are durable session policy, a `set_model` to a smaller model would
/// have failed every remaining turn of the session, non-retryably, forever.
/// The reduction is reported like every other divergence between what was
/// asked for and what was sent.
fn clamp_generation_options(
    model: &crate::ModelSpec,
    generation: &mut crate::GenerationOptions,
) -> bool {
    let (Some(requested), Some(capacity)) = (
        generation.output_token_cap,
        model.limits.output_token_capacity,
    ) else {
        return false;
    };
    if requested <= capacity {
        return false;
    }
    tracing::debug!(
        model = %model.id,
        requested = requested.get(),
        capacity = capacity.get(),
        "clamping requested output_token_cap to the model's output_token_capacity"
    );
    generation.output_token_cap = Some(capacity);
    true
}

/// Report the clamp on every disposition the call produced.
///
/// The adapter knows only that it put the cap it was handed on the wire, so it
/// reports `Applied`; the runtime is the only layer that saw the larger number
/// the caller asked for. This narrows that report on the response and on each
/// attempt of the ledger alike, so no carrier of the fate of this request
/// disagrees with another. An adapter that reports nothing keeps reporting
/// nothing: `None` means unreported, not "nothing happened".
fn record_clamped_output_token_cap(
    response: Option<&mut LlmResponse>,
    call_record: Option<&mut crate::LlmCallRecord>,
) {
    fn narrow(disposition: Option<&mut crate::GenerationDisposition>) {
        if let Some(disposition) = disposition
            && disposition.output_token_cap == crate::GenerationOptionDisposition::Applied
        {
            disposition.output_token_cap = crate::GenerationOptionDisposition::ClampedToCapacity;
        }
    }

    narrow(response.and_then(|response| response.generation_disposition.as_mut()));
    if let Some(call_record) = call_record {
        for attempt in &mut call_record.attempts {
            narrow(attempt.generation_disposition.as_mut());
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
            emit_plugin_runtime_events_runtime(event_tx, &emitted.plugin_id, emitted.value.events)
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
            emit_plugin_runtime_events_runtime(event_tx, &emitted.plugin_id, emitted.value.events)
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
        let clamped_output_token_cap =
            clamp_generation_options(&self.policy.model, &mut request.generation);
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
                        .forward_provider_stream_event(event_tx, stream_event, &mut stream_state)
                        .await
                    {
                        break Err(err);
                    }
                    if *stream_state.abort_requested {
                        // A plugin stream hook asked us to end the LLM
                        // call now after seeing a complete response block.
                        // Drain events already sitting in the buffer, then
                        // wait briefly for the provider's final completion
                        // event. Some providers attach usage there, and the
                        // next prompt's budget contribution depends on the
                        // driver seeing that accounting before the next
                        // protocol_iteration starts.
                        if let Err(err) = self
                            .drain_provider_stream_queue(event_tx, &mut llm_stream_rx, &mut stream_state)
                            .await
                        {
                            break Err(err);
                        }
                        streamed_usage = collect_trailing_usage_before_abort(
                            &mut llm_task,
                            &mut llm_stream_rx,
                            streamed_usage.clone(),
                            self.host.core.clock.as_ref(),
                        )
                        .await;

                        let mut resp = LlmResponse {
                            full_text: stream_accumulator.full_text(),
                            parts: Vec::new(),
                            usage: streamed_usage.clone(),
                            terminal_reason: crate::LlmTerminalReason::Stop,
                            terminal_diagnostic: None,
                            provider_usage: None,
                            request_body: None,
                            http_summary: None,
                            execution_evidence: None,
                            generation_disposition: None,
                            response_metadata: Default::default(),
                        };
                        stream_accumulator.apply_to_response(&mut resp);
                        let resp = match self.transform_assistant_response(event_tx, resp).await {
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
                        .drain_provider_stream_queue(event_tx, &mut llm_stream_rx, &mut stream_state)
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
                            let resp = match self.transform_assistant_response(event_tx, resp).await {
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
        if clamped_output_token_cap {
            record_clamped_output_token_cap(result.as_mut().ok(), call_record.as_mut());
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
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
            .transform_assistant_stream_chunk(event_tx, text)
            .await?;
        if outcome.abort_requested {
            *state.abort_requested = true;
        }
        for reasoning_delta in outcome.reasoning_deltas {
            let reasoning_delta: Arc<str> = reasoning_delta.into();
            state.stream_accumulator.push_reasoning(
                reasoning_delta.to_string(),
                None,
                Vec::new(),
                None,
            );
            send_session_event(
                event_tx,
                SessionStreamEvent::ReasoningDelta {
                    content: reasoning_delta.to_string(),
                },
            )
            .await;
            let correlation_id = stream_correlation_id(state.reasoning_correlation, None);
            remember_attempt_correlation(state.reasoning_attempt_correlations, &correlation_id);
            send_turn_activity(
                event_tx,
                correlation_id,
                TurnEvent::ReasoningDelta {
                    text: reasoning_delta,
                },
            )
            .await;
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
            state.stream_accumulator.push_text(&text);
            let text: Arc<str> = text.into();
            send_session_event(
                event_tx,
                SessionStreamEvent::TextDelta {
                    content: text.to_string(),
                },
            )
            .await;
            let correlation_id = stream_correlation_id(state.assistant_prose_correlation, item_id);
            remember_attempt_correlation(
                state.assistant_prose_attempt_correlations,
                &correlation_id,
            );
            send_turn_activity(
                event_tx,
                correlation_id,
                TurnEvent::AssistantProseDelta { text },
            )
            .await;
        }
        Ok(())
    }

    async fn forward_provider_stream_event(
        &mut self,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
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
                if !assistant_prose_correlation_ids.is_empty()
                    || !reasoning_correlation_ids.is_empty()
                {
                    send_turn_activity(
                        event_tx,
                        TurnActivityId::fresh(),
                        TurnEvent::ModelAttemptReset {
                            assistant_prose_correlation_ids,
                            reasoning_correlation_ids,
                        },
                    )
                    .await;
                }
                *state.stream_accumulator = LlmStreamAccumulator::default();
                *state.streamed_usage = LlmUsage::default();
                *state.text_streamed = false;
                *state.assistant_prose_correlation = None;
                *state.reasoning_correlation = None;
            }
            LlmStreamEvent::Delta(delta) => {
                self.emit_visible_assistant_text(event_tx, delta, None, "delta", state)
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
                    state
                        .stream_accumulator
                        .push_reasoning(delta.clone(), None, Vec::new(), None);
                    send_session_event(
                        event_tx,
                        SessionStreamEvent::ReasoningDelta {
                            content: delta.clone(),
                        },
                    )
                    .await;
                    let correlation_id = stream_correlation_id(state.reasoning_correlation, None);
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &correlation_id,
                    );
                    send_turn_activity(
                        event_tx,
                        correlation_id,
                        TurnEvent::ReasoningDelta { text: delta.into() },
                    )
                    .await;
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
                state.stream_accumulator.push_text_part(text, response_meta);
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
                state
                    .stream_accumulator
                    .push_tool_call(call_id, tool_name, input_json, replay);
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
                    send_session_event(
                        event_tx,
                        SessionStreamEvent::ReasoningDelta {
                            content: text.clone(),
                        },
                    )
                    .await;
                    let correlation_id =
                        stream_correlation_id(state.reasoning_correlation, item_id);
                    remember_attempt_correlation(
                        state.reasoning_attempt_correlations,
                        &correlation_id,
                    );
                    send_turn_activity(
                        event_tx,
                        correlation_id,
                        TurnEvent::ReasoningDelta {
                            text: text.clone().into(),
                        },
                    )
                    .await;
                }
                state
                    .stream_accumulator
                    .push_reasoning_with_replay(text, replay);
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
                *state.streamed_usage = usage;
            }
            LlmStreamEvent::RetryStatus {
                wait_seconds,
                attempt,
                max_attempts,
                reason,
            } => {
                send_session_event(
                    event_tx,
                    SessionStreamEvent::RetryStatus {
                        wait_seconds,
                        attempt,
                        max_attempts,
                        reason,
                        envelope: None,
                    },
                )
                .await;
            }
        }
        Ok(())
    }

    async fn drain_provider_stream_queue(
        &mut self,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
        llm_stream_rx: &mut tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>,
        state: &mut LlmStreamState<'_>,
    ) -> Result<(), LlmCallError> {
        while let Ok(stream_event) = llm_stream_rx.try_recv() {
            self.forward_provider_stream_event(event_tx, stream_event, state)
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
        .get_or_insert_with(TurnActivityId::fresh)
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

/// Wait up to 2s for a late `Usage` event from the provider after an
/// a plugin stream-mask abort. The usage is returned on the response itself so
/// sansio records it synchronously, which makes prompt-budget guidance
/// available to the next protocol iteration.
async fn collect_trailing_usage_before_abort<T>(
    llm_task: &mut tokio::task::JoinHandle<T>,
    llm_stream_rx: &mut tokio::sync::mpsc::UnboundedReceiver<LlmStreamEvent>,
    initial_usage: LlmUsage,
    clock: &dyn crate::Clock,
) -> LlmUsage {
    let deadline = clock.now() + std::time::Duration::from_millis(2_000);
    let mut latest = initial_usage;
    loop {
        tokio::select! {
            _ = clock.sleep_until(deadline) => break,
            event = llm_stream_rx.recv() => match event {
                None => break,
                Some(LlmStreamEvent::Usage(usage)) => {
                    latest = usage;
                    break;
                }
                Some(_) => continue,
            },
        }
    }
    llm_task.abort();
    latest
}
