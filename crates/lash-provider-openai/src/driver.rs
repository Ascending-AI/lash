use crate::support::*;

const CACHE_SESSION_ID_MAX_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionEndpoint {
    Responses,
    ChatCompletions,
}

struct ResponseContext {
    stream_events: Option<LlmEventSender>,
    provider_trace: Option<LlmProviderTraceSender>,
    url: String,
    stream_termination: StreamTermination,
}

impl CompletionEndpoint {
    pub(crate) fn request_trace_name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat/completions",
        }
    }

    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ChatCompletions => "chat/completions",
        }
    }

    pub(crate) fn serialize_error(self) -> &'static str {
        match self {
            Self::Responses => "Failed to serialize Responses body",
            Self::ChatCompletions => "Failed to serialize Chat Completions body",
        }
    }

    pub(crate) fn response_start_timeout_error(self) -> &'static str {
        match self {
            Self::Responses => "OpenAI-compatible response start timed out",
            Self::ChatCompletions => "OpenAI-compatible chat response start timed out",
        }
    }

    pub(crate) fn response_body_timeout_error(self) -> &'static str {
        match self {
            Self::Responses => "OpenAI-compatible response body timed out",
            Self::ChatCompletions => "OpenAI-compatible chat response body timed out",
        }
    }

    pub(crate) fn stream_chunk_timeout_error(self) -> &'static str {
        match self {
            Self::Responses => "OpenAI-compatible stream chunk timed out",
            Self::ChatCompletions => "OpenAI-compatible chat stream chunk timed out",
        }
    }

    pub(crate) fn request_failed_prefix(self) -> &'static str {
        match self {
            Self::Responses => "OpenAI-compatible request failed",
            Self::ChatCompletions => "OpenAI-compatible chat request failed",
        }
    }

    pub(crate) fn http_summary(self, url: &str, stream: bool) -> String {
        if stream {
            format!("HTTP POST {url} (stream)")
        } else {
            format!("HTTP POST {url}")
        }
    }
}

pub(crate) async fn complete(
    provider: &mut OpenAiCompatibleProvider,
    req: LlmRequest,
    endpoint: CompletionEndpoint,
) -> Result<LlmResponse, LlmTransportError> {
    let stream_events = req.stream_events.clone();
    let provider_trace = req.provider_trace.clone();
    let timeouts = provider.options.llm_timeouts();
    let stream = stream_events.is_some();
    let compat = provider.resolved_compat(endpoint);
    let stream_termination = req
        .model_capability
        .stream_termination
        .unwrap_or(compat.stream_termination);
    let mut body = match endpoint {
        CompletionEndpoint::Responses => provider.build_responses_request_body(&req, stream)?,
        CompletionEndpoint::ChatCompletions => provider.build_chat_request_body(&req, stream)?,
    };
    if compat.cache_session_affinity {
        body["session_id"] = Value::String(
            req.scope
                .session_id
                .chars()
                .take(CACHE_SESSION_ID_MAX_CHARS)
                .collect(),
        );
    }
    let generation_disposition = Some(generation_disposition(&req, &body));
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| LlmTransportError::new(format!("{}: {e}", endpoint.serialize_error())))?;
    emit_provider_request_trace(
        provider_trace.as_ref(),
        "openai_compatible",
        endpoint.request_trace_name(),
        &body_bytes,
    );
    let request_body = bytes::Bytes::from(body_bytes);
    let request_body_for_error = String::from_utf8_lossy(&request_body).into_owned();
    let url = format!(
        "{}/{}",
        provider.base_url.trim_end_matches('/'),
        endpoint.path()
    );
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {}", provider.api_key),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
    ];
    if compat.cache_session_affinity {
        headers.push((
            "x-client-request-id".to_string(),
            req.scope.request_id.clone(),
        ));
    }
    let http_request = LlmHttpRequest {
        method: LlmHttpMethod::Post,
        url: url.clone(),
        headers,
        body: request_body.clone(),
        body_for_error: Some(request_body_for_error.clone()),
        response_start_timeout_message: Some(endpoint.response_start_timeout_error().to_string()),
    };
    let stream_bounds = SseStreamBounds::new(timeouts.request_timeout, &provider.options);
    let resp = provider
        .transport
        .send(
            http_request,
            response_start_timeout(timeouts.request_timeout, timeouts.chunk_timeout, stream),
        )
        .await?;

    let status = resp.status;
    if !resp.is_success() {
        let headers = resp.headers;
        let text = read_http_body_text(
            resp.body,
            timeouts.request_timeout,
            endpoint.response_body_timeout_error(),
        )
        .await
        .unwrap_or_default();
        let message = format!("{} with {}", endpoint.request_failed_prefix(), status);
        // Retryability is decided centrally by the provider failure
        // classifier from the attached HTTP status; no inline override here.
        return Err(http_error_envelope(
            message,
            status,
            headers,
            text,
            Some(request_body_for_error),
        ));
    }

    let provider_request_id = first_header_value(&resp.headers, "x-request-id").map(str::to_string);
    let mut capture = ResponseMetadataCapture::from_response(&provider.options, &resp.headers);
    let is_sse = header_contains(&resp.headers, "content-type", "text/event-stream");

    let response_context = ResponseContext {
        stream_events,
        provider_trace,
        url,
        stream_termination,
    };
    let response = if is_sse {
        drive_streaming_response(
            provider,
            endpoint,
            resp.body,
            timeouts.chunk_timeout,
            stream_bounds,
            response_context,
            &mut capture,
        )
        .await
    } else {
        complete_buffered_response(
            provider,
            endpoint,
            resp.body,
            timeouts.request_timeout,
            response_context,
            &mut capture,
        )
        .await
    };
    let mut response = match response {
        Ok(response) => response,
        Err(mut failure) => {
            let response_metadata = capture.into_metadata();
            if failure.request_body.is_none() {
                failure.request_body = Some(request_body_for_error.clone());
            }
            if let Some(partial) = failure.partial_response.as_deref_mut()
                && partial.request_body.is_none()
            {
                partial.request_body = Some(request_body_for_error.clone());
            }
            if let Some(partial) = failure.partial_response.as_deref_mut() {
                partial.response_metadata = response_metadata;
                partial.generation_disposition = generation_disposition;
            }
            if let (Some(partial), Some(provider_request_id)) = (
                failure.partial_response.as_deref_mut(),
                provider_request_id.as_ref(),
            ) {
                partial
                    .execution_evidence
                    .get_or_insert_with(ExecutionEvidence::default)
                    .provider_request_id = Some(provider_request_id.clone());
            }
            if let Some(provider_request_id) = provider_request_id
                && !failure
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("x-request-id"))
            {
                failure
                    .headers
                    .push(("x-request-id".to_string(), provider_request_id));
            }
            return Err(failure);
        }
    };
    if let Some(provider_request_id) = provider_request_id {
        response
            .execution_evidence
            .get_or_insert_with(ExecutionEvidence::default)
            .provider_request_id = Some(provider_request_id);
    }
    // Keep successful responses aligned with Anthropic and Google: hosts may
    // consume this public diagnostic through `LlmDebug`. This also means the
    // exact body is serialized in durable effect outcomes; that journal-size
    // cost is accepted deliberately for the existing cross-provider contract.
    response.request_body = Some(request_body_for_error);
    response.response_metadata = capture.into_metadata();
    response.generation_disposition = generation_disposition;
    Ok(response)
}

async fn complete_buffered_response(
    provider: &OpenAiCompatibleProvider,
    endpoint: CompletionEndpoint,
    body: LlmHttpBody,
    timeout: Option<std::time::Duration>,
    context: ResponseContext,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseContext {
        stream_events,
        provider_trace,
        url,
        stream_termination,
    } = context;
    let stream_termination = stream_events.is_some().then_some(stream_termination);
    let text = read_http_body_text(body, timeout, endpoint.response_body_timeout_error()).await?;
    capture.capture_body_text(&text);
    emit_provider_trace(provider_trace.as_ref(), "openai_compatible", &text);
    match endpoint {
        CompletionEndpoint::Responses => {
            complete_buffered_responses(provider, text, stream_events, url, stream_termination)
        }
        CompletionEndpoint::ChatCompletions => {
            complete_buffered_chat(provider, text, stream_events, url, stream_termination)
        }
    }
}

fn complete_buffered_responses(
    provider: &OpenAiCompatibleProvider,
    text: String,
    stream_events: Option<LlmEventSender>,
    url: String,
    stream_termination: Option<StreamTermination>,
) -> Result<LlmResponse, LlmTransportError> {
    let mut state = ResponsesStreamState::default();
    if text.trim_start().starts_with("data:") || text.contains("\ndata:") {
        OpenAiCompatibleProvider::parse_sse_payload(&text, &mut state)?;
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Responses JSON: {e}")).with_raw(text.clone())
        })?;
        state.provider_usage = value.get("usage").cloned();
        state.usage = usage_from_response_value(&value);
        state.parts = OpenAiCompatibleProvider::response_parts_from_value(&value);
        state.recompute_full_text();
        state.final_response = Some(value);
    }
    let terminal_event_seen = state.terminal_event_seen
        || state
            .final_response
            .as_ref()
            .and_then(|response| response.get("status").and_then(Value::as_str))
            .is_some_and(|status| matches!(status, "completed" | "incomplete" | "failed"));
    if stream_termination == Some(StreamTermination::RequireTerminalEvidence)
        && !terminal_event_seen
    {
        let mut partial = shared_response_from_state(
            state,
            CompletionEndpoint::Responses.http_summary(&url, false),
        );
        partial.terminal_reason = LlmTerminalReason::Unknown;
        return Err(LlmTransportError::new(
            "OpenAI Responses stream ended before a terminal response event",
        )
        .with_kind(ProviderFailureKind::Stream)
        .with_code("stream_ended_before_terminal_response")
        .retryable(true)
        .with_partial_response(partial));
    }
    let parts = state.response_parts();
    let terminal_reason = state
        .final_response
        .as_ref()
        .map(|value| terminal_reason_from_responses_value(value, &parts))
        .unwrap_or_else(|| terminal_reason_from_parts(&parts));
    if !has_response_content(&parts)
        && !matches!(
            terminal_reason,
            LlmTerminalReason::OutputLimit
                | LlmTerminalReason::ContentFilter
                | LlmTerminalReason::Cancelled
        )
    {
        return Err(empty_response_error(text));
    }
    if let Some(tx) = &stream_events {
        if state.usage != LlmUsage::default() {
            tx.send(LlmStreamEvent::Usage(state.usage.clone()));
        }
        if provider.options.expose_thinking {
            for part in &parts {
                if let LlmOutputPart::Reasoning { text, .. } = part
                    && !text.is_empty()
                {
                    tx.send(LlmStreamEvent::ReasoningDelta(text.clone()));
                }
            }
        }
        if !state.full_text.is_empty() {
            tx.send(LlmStreamEvent::Delta(state.full_text.clone()));
        }
    }
    Ok(LlmResponse {
        full_text: state.full_text,
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(CompletionEndpoint::Responses.http_summary(&url, false)),
        execution_evidence: None,
        generation_disposition: None,
        response_metadata: Default::default(),
    })
}

fn complete_buffered_chat(
    provider: &OpenAiCompatibleProvider,
    text: String,
    stream_events: Option<LlmEventSender>,
    url: String,
    stream_termination: Option<StreamTermination>,
) -> Result<LlmResponse, LlmTransportError> {
    let mut state = ChatStreamState::default();
    let mut parsed_parts = None;
    if text.trim_start().starts_with("data:") || text.contains("\ndata:") {
        OpenAiCompatibleProvider::parse_chat_sse_payload(&text, &mut state)?;
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Chat Completions JSON: {e}"))
                .with_raw(text.clone())
        })?;
        state.capture_response_value(&value);
        state.provider_usage = value.get("usage").cloned();
        state.usage = usage_from_response_value(&value);
        let parts = OpenAiCompatibleProvider::chat_response_parts_from_value(&value);
        let terminal_reason = terminal_reason_from_chat_value(&value, &parts);
        state.full_text = parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        parsed_parts = Some(parts);
        state.final_response_raw = Some(text.clone());
        state.terminal_reason = terminal_reason;
    }
    let parts = parsed_parts.unwrap_or_else(|| state.parts());
    if stream_termination == Some(StreamTermination::RequireTerminalEvidence)
        && state.provider_finish_reason.is_none()
    {
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_finish_reason")
            .retryable(true)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    if !has_response_content(&parts) {
        return Err(empty_response_error(text));
    }
    if let Some(tx) = &stream_events {
        if state.usage != LlmUsage::default() {
            tx.send(LlmStreamEvent::Usage(state.usage.clone()));
        }
        if !state.full_text.is_empty() {
            tx.send(LlmStreamEvent::Delta(state.full_text.clone()));
        }
        if provider.options.expose_thinking {
            for part in parts
                .iter()
                .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
            {
                tx.send(LlmStreamEvent::Part(part.clone()));
            }
        }
        for part in parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
        {
            tx.send(LlmStreamEvent::Part(part.clone()));
        }
    }
    let terminal_reason = if state.terminal_reason == LlmTerminalReason::Unknown {
        terminal_reason_from_parts(&parts)
    } else {
        state.terminal_reason
    };
    let execution_evidence = state.execution_evidence();
    Ok(LlmResponse {
        full_text: state.full_text,
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(CompletionEndpoint::ChatCompletions.http_summary(&url, false)),
        execution_evidence,
        generation_disposition: None,
        response_metadata: Default::default(),
    })
}

async fn drive_streaming_response(
    provider: &OpenAiCompatibleProvider,
    endpoint: CompletionEndpoint,
    body: LlmHttpBody,
    chunk_timeout: std::time::Duration,
    stream_bounds: SseStreamBounds,
    context: ResponseContext,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    match endpoint {
        CompletionEndpoint::Responses => {
            drive_streaming_responses(
                provider,
                body,
                chunk_timeout,
                stream_bounds,
                context,
                capture,
            )
            .await
        }
        CompletionEndpoint::ChatCompletions => {
            drive_streaming_chat(
                provider,
                body,
                chunk_timeout,
                stream_bounds,
                context,
                capture,
            )
            .await
        }
    }
}

async fn drive_streaming_responses(
    provider: &OpenAiCompatibleProvider,
    body: LlmHttpBody,
    chunk_timeout: std::time::Duration,
    stream_bounds: SseStreamBounds,
    context: ResponseContext,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseContext {
        stream_events,
        provider_trace,
        url,
        stream_termination,
    } = context;
    let mut state = ResponsesStreamState::default();
    let mut emitted_parts = Vec::new();
    let expose_thinking = provider.options.expose_thinking;
    let stream_result = drive_sse_response(
        body,
        chunk_timeout,
        stream_bounds,
        CompletionEndpoint::Responses.stream_chunk_timeout_error(),
        "OpenAI-compatible request timed out",
        capture,
        |raw| {
            emit_provider_trace(provider_trace.as_ref(), "openai_compatible", raw);
            let prev_usage = state.usage.clone();
            OpenAiCompatibleProvider::process_sse_event(raw, &mut state, Some(&mut emitted_parts))?;
            emit_stream_progress(
                stream_events.as_ref(),
                state.take_text_deltas(),
                &state.usage,
                &prev_usage,
            );
            if let Some(tx) = &stream_events {
                for delta in state.take_reasoning_deltas() {
                    if expose_thinking {
                        tx.send(LlmStreamEvent::ReasoningDelta(delta));
                    }
                }
                for part in emitted_parts.drain(..) {
                    if matches!(part, LlmOutputPart::Reasoning { .. }) && !expose_thinking {
                        continue;
                    }
                    tx.send(LlmStreamEvent::Part(part));
                }
            } else {
                emitted_parts.clear();
                state.take_reasoning_deltas();
            }
            Ok(())
        },
    )
    .await;

    if let Err(error) = stream_result {
        let mut partial = shared_response_from_state(
            state.clone(),
            CompletionEndpoint::Responses.http_summary(&url, true),
        );
        partial.terminal_reason = LlmTerminalReason::Unknown;
        return Err(error.with_partial_response(partial));
    }

    if stream_termination == StreamTermination::RequireTerminalEvidence
        && !state.terminal_event_seen
    {
        let mut partial = shared_response_from_state(
            state.clone(),
            CompletionEndpoint::Responses.http_summary(&url, true),
        );
        partial.terminal_reason = LlmTerminalReason::Unknown;
        return Err(LlmTransportError::new(
            "OpenAI Responses stream ended before a terminal response event",
        )
        .with_kind(ProviderFailureKind::Stream)
        .with_code("stream_ended_before_terminal_response")
        .retryable(true)
        .with_partial_response(partial));
    }

    let parts = state.response_parts();
    let terminal_reason = state
        .final_response
        .as_ref()
        .map(|value| terminal_reason_from_responses_value(value, &parts))
        .unwrap_or_else(|| terminal_reason_from_parts(&parts));
    if !has_response_content(&parts)
        && !matches!(
            terminal_reason,
            LlmTerminalReason::OutputLimit
                | LlmTerminalReason::ContentFilter
                | LlmTerminalReason::Cancelled
        )
    {
        return Err(empty_response_error(
            state
                .final_response
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default(),
        ));
    }
    Ok(LlmResponse {
        full_text: state.full_text,
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(CompletionEndpoint::Responses.http_summary(&url, true)),
        execution_evidence: None,
        generation_disposition: None,
        response_metadata: Default::default(),
    })
}

async fn drive_streaming_chat(
    provider: &OpenAiCompatibleProvider,
    body: LlmHttpBody,
    chunk_timeout: std::time::Duration,
    stream_bounds: SseStreamBounds,
    context: ResponseContext,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseContext {
        stream_events,
        provider_trace,
        url,
        stream_termination,
    } = context;
    let mut state = ChatStreamState::default();
    let expose_thinking = provider.options.expose_thinking;
    let stream_result = drive_sse_response(
        body,
        chunk_timeout,
        stream_bounds,
        CompletionEndpoint::ChatCompletions.stream_chunk_timeout_error(),
        "OpenAI-compatible request timed out",
        capture,
        |raw| {
            emit_provider_trace(provider_trace.as_ref(), "openai_compatible", raw);
            let prev_usage = state.usage.clone();
            OpenAiCompatibleProvider::process_chat_sse_event(raw, &mut state)?;
            emit_stream_progress(
                stream_events.as_ref(),
                state.take_text_deltas(),
                &state.usage,
                &prev_usage,
            );
            if let Some(tx) = &stream_events {
                for delta in state.take_reasoning_deltas() {
                    if expose_thinking {
                        tx.send(LlmStreamEvent::ReasoningDelta(delta));
                    }
                }
            } else {
                state.take_reasoning_deltas();
            }
            if let Some(tx) = &stream_events {
                for part in state.take_completed_tool_call_parts() {
                    tx.send(LlmStreamEvent::Part(part));
                }
            }
            Ok(())
        },
    )
    .await;

    if let Err(error) = stream_result {
        return Err(error.with_partial_response(chat_response_from_state(state.clone(), &url)));
    }

    let parts = state.parts();
    if !has_response_content(&parts) {
        return Err(empty_response_error(
            state.final_response_raw.clone().unwrap_or_default(),
        ));
    }
    if stream_termination == StreamTermination::RequireTerminalEvidence
        && state.provider_finish_reason.is_none()
    {
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_finish_reason")
            .retryable(true)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    if let Some(tx) = &stream_events {
        for part in state.take_remaining_tool_call_parts() {
            tx.send(LlmStreamEvent::Part(part));
        }
    }
    let parts = state.parts();
    let terminal_reason = if state.terminal_reason == LlmTerminalReason::Unknown {
        terminal_reason_from_parts(&parts)
    } else {
        state.terminal_reason
    };
    let execution_evidence = state.execution_evidence();
    Ok(LlmResponse {
        full_text: state.full_text,
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(CompletionEndpoint::ChatCompletions.http_summary(&url, true)),
        execution_evidence,
        generation_disposition: None,
        response_metadata: Default::default(),
    })
}

fn shared_response_from_state(state: ResponsesStreamState, http_summary: String) -> LlmResponse {
    crate::responses_shared::response_from_stream_state(state, None, http_summary)
}

fn chat_response_from_state(state: ChatStreamState, url: &str) -> LlmResponse {
    let parts = state.parts();
    let execution_evidence = state.execution_evidence();
    LlmResponse {
        full_text: state.full_text,
        parts,
        usage: state.usage,
        terminal_reason: LlmTerminalReason::Unknown,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(CompletionEndpoint::ChatCompletions.http_summary(url, true)),
        execution_evidence,
        generation_disposition: None,
        response_metadata: Default::default(),
    }
}
