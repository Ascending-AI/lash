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
    http_summary: String,
    stream_termination: StreamTermination,
    responses_resume: Option<ResponsesResumeCheckpoint>,
    request_key: ResponsesRequestKey,
}

pub(crate) type ResponsesRequestFingerprint = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResponsesRequestKey {
    pub(crate) request_id: String,
    pub(crate) fingerprint: ResponsesRequestFingerprint,
}

#[derive(Clone, Debug)]
pub(crate) struct ResponsesResumeCheckpoint {
    pub(crate) request_key: ResponsesRequestKey,
    response_id: String,
    starting_after: u64,
    state: ResponsesStreamState,
}

fn responses_resume_url(
    base_url: &str,
    response_id: &str,
    starting_after: u64,
    query_params: &[(String, String)],
) -> Result<String, LlmTransportError> {
    let mut url = reqwest::Url::parse(base_url.trim_end_matches('/')).map_err(|error| {
        LlmTransportError::new(format!("Invalid OpenAI Responses resume URL: {error}"))
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_provider_endpoint")
    })?;
    url.path_segments_mut()
        .map_err(|_| {
            LlmTransportError::new("OpenAI Responses resume URL cannot carry path segments")
                .with_kind(ProviderFailureKind::Validation)
                .with_code("invalid_provider_endpoint")
        })?
        .push("responses")
        .push(response_id);
    url.query_pairs_mut()
        .extend_pairs(query_params.iter())
        .append_pair("starting_after", &starting_after.to_string())
        .append_pair("stream", "true");
    Ok(url.into())
}

fn responses_event_sequence_number(raw: &str) -> Option<u64> {
    serde_json::from_str::<Value>(raw.trim())
        .ok()?
        .get("sequence_number")?
        .as_u64()
}

fn responses_stream_failure(
    provider: &mut OpenAiCompatibleProvider,
    request_key: ResponsesRequestKey,
    state: ResponsesStreamState,
    last_sequence_number: Option<u64>,
    sequence_cursor_valid: bool,
    http_summary: String,
    error: LlmTransportError,
) -> LlmTransportError {
    let output_started = state.output_started();
    let mut partial = shared_response_from_state(state.clone(), http_summary);
    partial.terminal_reason = LlmTerminalReason::Unknown;

    let response_id = state
        .execution_evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_response_id.clone());
    provider.responses_resume =
        if error.is_retryable() && !state.terminal_event_seen && sequence_cursor_valid {
            response_id
                .zip(last_sequence_number)
                .map(|(response_id, starting_after)| ResponsesResumeCheckpoint {
                    request_key,
                    response_id,
                    starting_after,
                    state,
                })
        } else {
            None
        };

    error
        .with_output_started(output_started)
        .with_partial_response(partial)
}

fn build_request_body(
    provider: &OpenAiCompatibleProvider,
    req: &LlmRequest,
    endpoint: CompletionEndpoint,
    stream: bool,
    origin_route: &ProviderRouteIdentity,
) -> Result<Value, LlmTransportError> {
    let mut body = match endpoint {
        CompletionEndpoint::Responses => {
            provider.build_responses_request_body_for_route(req, stream, origin_route)?
        }
        CompletionEndpoint::ChatCompletions => provider.build_chat_request_body(req, stream)?,
    };
    if provider.resolved_compat(endpoint).cache_session_affinity {
        body["session_id"] = Value::String(
            req.scope
                .session_id
                .chars()
                .take(CACHE_SESSION_ID_MAX_CHARS)
                .collect(),
        );
    }
    Ok(body)
}

fn request_fingerprint(body: &[u8]) -> ResponsesRequestFingerprint {
    lash_sansio::core_support::blake3_domain_hash("lash-openai-responses-request/v2", body)
}

pub(crate) fn responses_request_fingerprint(
    provider: &OpenAiCompatibleProvider,
    req: &LlmRequest,
) -> Option<ResponsesRequestFingerprint> {
    let endpoint = CompletionEndpoint::Responses;
    let origin_route = ProviderRouteIdentity::for_endpoint(
        endpoint.provider_kind(),
        &provider.base_url,
        req.model.clone(),
    );
    let body = build_request_body(
        provider,
        req,
        endpoint,
        req.stream_events.is_some(),
        &origin_route,
    )
    .ok()?;
    let body_bytes = serde_json::to_vec(&body).ok()?;
    Some(request_fingerprint(&body_bytes))
}

impl CompletionEndpoint {
    fn provider_kind(self) -> &'static str {
        match self {
            Self::Responses => "openai",
            Self::ChatCompletions => "openai-compatible",
        }
    }

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
    let origin_model = req.model.clone();
    let origin_route = ProviderRouteIdentity::for_endpoint(
        endpoint.provider_kind(),
        &provider.base_url,
        origin_model.clone(),
    );
    origin_route.validate_endpoint().map_err(|error| {
        LlmTransportError::new(error.to_string())
            .with_kind(ProviderFailureKind::Validation)
            .with_code("invalid_provider_endpoint")
    })?;
    let stream_events = req.stream_events.clone().map(|downstream| {
        let origin_route = origin_route.clone();
        LlmEventSender::new(move |mut event| {
            if let LlmStreamEvent::Part(part) = &mut event {
                let _ = part.stamp_replay_origin(&origin_route);
            }
            downstream.send(event);
        })
    });
    let provider_trace = req.provider_trace.clone();
    let timeouts = provider.options.llm_timeouts();
    let stream = stream_events.is_some();
    let compat = provider.resolved_compat(endpoint);
    let stream_termination = req
        .model_capability
        .stream_termination
        .unwrap_or(compat.stream_termination);
    let body = build_request_body(provider, &req, endpoint, stream, &origin_route)?;
    let generation_disposition = Some(generation_disposition(&req, &body));
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| LlmTransportError::new(format!("{}: {e}", endpoint.serialize_error())))?;
    let request_fingerprint = request_fingerprint(&body_bytes);
    let request_key = ResponsesRequestKey {
        request_id: req.scope.request_id.clone(),
        fingerprint: request_fingerprint,
    };
    let responses_resume = if endpoint == CompletionEndpoint::Responses && stream {
        let matches_request = provider
            .responses_resume
            .as_ref()
            .is_some_and(|resume| resume.request_key == request_key);
        if !matches_request {
            provider.responses_resume = None;
        }
        provider.responses_resume.clone()
    } else {
        provider.responses_resume = None;
        None
    };
    emit_provider_request_trace(
        provider_trace.as_ref(),
        "openai_compatible",
        endpoint.request_trace_name(),
        &body_bytes,
    );
    let request_body = bytes::Bytes::from(body_bytes);
    let request_body_for_error = String::from_utf8_lossy(&request_body).into_owned();
    let base_url = provider.base_url.trim_end_matches('/');
    let mut creation_url = match base_url.split_once('?') {
        Some((base_path, query)) => format!("{}/{}?{}", base_path, endpoint.path(), query),
        None => format!("{}/{}", base_url, endpoint.path()),
    };
    if !provider.wire.query_params.is_empty() {
        let mut parsed = reqwest::Url::parse(&creation_url).map_err(|error| {
            LlmTransportError::new(format!("Invalid OpenAI-compatible request URL: {error}"))
                .with_kind(ProviderFailureKind::Validation)
                .with_code("invalid_provider_endpoint")
        })?;
        parsed
            .query_pairs_mut()
            .extend_pairs(provider.wire.query_params.iter());
        creation_url = parsed.into();
    }
    let (http_method, url, wire_body) = if let Some(resume) = responses_resume.as_ref() {
        (
            LlmHttpMethod::Get,
            responses_resume_url(
                &provider.base_url,
                &resume.response_id,
                resume.starting_after,
                &provider.wire.query_params,
            )?,
            bytes::Bytes::new(),
        )
    } else {
        (LlmHttpMethod::Post, creation_url, request_body.clone())
    };
    let http_summary = if http_method == LlmHttpMethod::Get {
        format!("HTTP GET {url} (stream)")
    } else {
        endpoint.http_summary(&url, stream)
    };
    let mut headers = vec![
        (
            provider.wire.auth_header_name.clone(),
            format!("{}{}", provider.wire.auth_value_prefix, provider.api_key),
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
        method: http_method,
        url: url.clone(),
        headers,
        body: wire_body,
        body_for_error: responses_resume
            .is_none()
            .then_some(request_body_for_error.clone()),
        response_start_timeout_message: Some(endpoint.response_start_timeout_error().to_string()),
    };
    let stream_bounds = SseStreamBounds::new(timeouts.request_timeout, &provider.options);
    let resp = match provider
        .transport
        .send(
            http_request,
            response_start_timeout(timeouts.request_timeout, timeouts.chunk_timeout, stream),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let Some(resume) = responses_resume else {
                return Err(error);
            };
            return Err(responses_stream_failure(
                provider,
                resume.request_key,
                resume.state,
                Some(resume.starting_after),
                true,
                http_summary,
                error,
            ));
        }
    };

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
        // Preserve the provider's typed error code before the shared status
        // classifier runs; exact overflow codes are authoritative.
        let mut failure = http_error_envelope(
            message,
            status,
            headers,
            text.clone(),
            Some(request_body_for_error),
        );
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            failure = classify_openai_error(&value, failure);
        }
        if let Some(resume) = responses_resume {
            failure = responses_stream_failure(
                provider,
                resume.request_key,
                resume.state,
                Some(resume.starting_after),
                true,
                http_summary,
                failure,
            );
        }
        return Err(failure);
    }

    let provider_request_id = first_header_value(&resp.headers, "x-request-id").map(str::to_string);
    let mut capture = ResponseMetadataCapture::from_response(&provider.options, &resp.headers);
    // Reattachment is another HTTP request for the same logical generation.
    // Its transport request id belongs in the per-attempt record, but sending
    // a second response-start Evidence event would conflict with the live
    // stream's already-established request identity and request summary.
    if responses_resume.is_none()
        && let Some(tx) = &stream_events
    {
        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
            request_body: Some(request_body_for_error.clone()),
            http_summary: Some(http_summary.clone()),
            execution_evidence: provider_request_id.clone().map(|provider_request_id| {
                ExecutionEvidence {
                    provider_request_id: Some(provider_request_id),
                    ..Default::default()
                }
            }),
            generation_disposition,
            response_metadata: capture.metadata(),
            ..Default::default()
        }));
    }
    let is_sse = header_contains(&resp.headers, "content-type", "text/event-stream");
    if !is_sse && let Some(resume) = responses_resume.clone() {
        return Err(responses_stream_failure(
            provider,
            resume.request_key,
            resume.state,
            Some(resume.starting_after),
            true,
            http_summary,
            LlmTransportError::new("OpenAI Responses reattachment did not return an event stream")
                .with_kind(ProviderFailureKind::Stream)
                .with_code("responses_resume_not_streaming")
                .with_retry_verdict(TransportRetryVerdict::NotRetryable),
        ));
    }

    let response_context = ResponseContext {
        stream_events,
        provider_trace,
        url,
        http_summary,
        stream_termination,
        responses_resume,
        request_key,
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
        Ok(response) => {
            if endpoint == CompletionEndpoint::Responses {
                provider.responses_resume = None;
            }
            response
        }
        Err(mut failure) => {
            let response_metadata = capture.into_metadata();
            if failure.request_body.is_none() {
                failure.request_body = Some(Box::new(request_body_for_error.clone()));
            }
            if let Some(partial) = failure.partial_response.as_deref_mut()
                && partial.request_body.is_none()
            {
                partial.request_body = Some(request_body_for_error.clone());
            }
            if let Some(partial) = failure.partial_response.as_deref_mut() {
                partial.response_metadata = response_metadata;
                partial.generation_disposition = generation_disposition;
                partial
                    .stamp_replay_origin(&origin_route)
                    .map_err(|conflict| {
                        LlmTransportError::new(conflict.to_string())
                            .with_kind(ProviderFailureKind::Validation)
                            .with_code("provider_replay_origin_conflict")
                    })?;
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
    response
        .stamp_replay_origin(&origin_route)
        .map_err(|conflict| {
            LlmTransportError::new(conflict.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_code("provider_replay_origin_conflict")
        })?;
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
        http_summary,
        stream_termination,
        ..
    } = context;
    let stream_termination = stream_events.is_some().then_some(stream_termination);
    let text = read_http_body_text(body, timeout, endpoint.response_body_timeout_error()).await?;
    capture.capture_body_text(&text);
    emit_provider_trace(provider_trace.as_ref(), "openai_compatible", &text);
    match endpoint {
        CompletionEndpoint::Responses => complete_buffered_responses(
            provider,
            text,
            stream_events,
            http_summary,
            stream_termination,
        ),
        CompletionEndpoint::ChatCompletions => {
            complete_buffered_chat(provider, text, stream_events, url, stream_termination)
        }
    }
}

fn complete_buffered_responses(
    provider: &OpenAiCompatibleProvider,
    text: String,
    stream_events: Option<LlmEventSender>,
    http_summary: String,
    stream_termination: Option<StreamTermination>,
) -> Result<LlmResponse, LlmTransportError> {
    let mut state = ResponsesStreamState::default();
    if text.trim_start().starts_with("data:") || text.contains("\ndata:") {
        OpenAiCompatibleProvider::parse_sse_payload(&text, &mut state)?;
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Responses JSON: {e}")).with_raw(text.clone())
        })?;
        state.capture_execution_evidence(&value, true)?;
        state.provider_usage = value.get("usage").cloned();
        state.usage = usage_from_response_value(&value);
        state.parts = OpenAiCompatibleProvider::response_parts_from_value(&value);
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
        let output_started = state.output_started();
        let mut partial = shared_response_from_state(state, http_summary);
        partial.terminal_reason = LlmTerminalReason::Unknown;
        return Err(LlmTransportError::new(
            "OpenAI Responses stream ended before a terminal response event",
        )
        .with_kind(ProviderFailureKind::Stream)
        .with_code("stream_ended_before_terminal_response")
        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
        .with_output_started(output_started)
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
        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
            provider_usage: state.provider_usage.clone(),
            execution_evidence: state.execution_evidence.clone(),
            ..Default::default()
        }));
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
        let full_text = state.full_text();
        if !full_text.is_empty() {
            tx.send(LlmStreamEvent::Delta(full_text));
        }
    }
    Ok(LlmResponse {
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(http_summary),
        execution_evidence: state.execution_evidence,
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
        state.capture_response_value(&value)?;
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
        && state
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_finish_reason.as_ref())
            .is_none()
    {
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_finish_reason")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    if !has_response_content(&parts) {
        return Err(empty_response_error(text));
    }
    if let Some(tx) = &stream_events {
        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
            provider_usage: state.provider_usage.clone(),
            execution_evidence: state.execution_evidence.clone(),
            ..Default::default()
        }));
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
    let execution_evidence = state.execution_evidence;
    Ok(LlmResponse {
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
    provider: &mut OpenAiCompatibleProvider,
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
    provider: &mut OpenAiCompatibleProvider,
    body: LlmHttpBody,
    chunk_timeout: std::time::Duration,
    stream_bounds: SseStreamBounds,
    context: ResponseContext,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseContext {
        stream_events,
        provider_trace,
        url: _,
        http_summary,
        stream_termination,
        responses_resume,
        request_key,
    } = context;
    let resume_after = responses_resume
        .as_ref()
        .map(|resume| resume.starting_after);
    let mut last_sequence_number = resume_after;
    let mut sequence_cursor_valid = true;
    let mut state = responses_resume
        .map(|resume| resume.state)
        .unwrap_or_default();
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
            let sequence_number = responses_event_sequence_number(raw);
            if let Some(resume_after) = resume_after {
                if raw.trim() != "[DONE]" && sequence_number.is_none() {
                    sequence_cursor_valid = false;
                    return Err(LlmTransportError::new(
                        "OpenAI Responses resume event omitted sequence_number",
                    )
                    .with_kind(ProviderFailureKind::Stream)
                    .with_code("responses_resume_event_missing_sequence")
                    .with_retry_verdict(TransportRetryVerdict::NotRetryable));
                }
                if sequence_number.is_some_and(|sequence| sequence <= resume_after) {
                    return Ok(());
                }
            } else if raw.trim() != "[DONE]" && sequence_number.is_none() {
                sequence_cursor_valid = false;
            }
            let prev_usage = state.usage.clone();
            OpenAiCompatibleProvider::process_sse_event(raw, &mut state, Some(&mut emitted_parts))?;
            if let Some(sequence_number) = sequence_number {
                last_sequence_number = Some(
                    last_sequence_number.map_or(sequence_number, |last| last.max(sequence_number)),
                );
            }
            if let Some(tx) = &stream_events
                && (state.provider_usage.is_some() || state.execution_evidence.is_some())
            {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    provider_usage: state.provider_usage.clone(),
                    execution_evidence: state.execution_evidence.clone(),
                    ..Default::default()
                }));
            }
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
        return Err(responses_stream_failure(
            provider,
            request_key,
            state,
            last_sequence_number,
            sequence_cursor_valid,
            http_summary,
            error,
        ));
    }

    if stream_termination == StreamTermination::RequireTerminalEvidence
        && !state.terminal_event_seen
    {
        return Err(responses_stream_failure(
            provider,
            request_key,
            state,
            last_sequence_number,
            sequence_cursor_valid,
            http_summary,
            LlmTransportError::new(
                "OpenAI Responses stream ended before a terminal response event",
            )
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_terminal_response")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
        ));
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
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body: None,
        http_summary: Some(http_summary),
        execution_evidence: state.execution_evidence,
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
        http_summary: _,
        stream_termination,
        ..
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
            if let Some(tx) = &stream_events
                && (state.provider_usage.is_some() || state.execution_evidence.is_some())
            {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    provider_usage: state.provider_usage.clone(),
                    execution_evidence: state.execution_evidence.clone(),
                    ..Default::default()
                }));
            }
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

    if stream_termination == StreamTermination::RequireTerminalEvidence
        && state
            .execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.provider_finish_reason.as_ref())
            .is_none()
    {
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_finish_reason")
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    let parts = state.parts();
    if !has_response_content(&parts) {
        return Err(empty_response_error(
            state.final_response_raw.clone().unwrap_or_default(),
        ));
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
    let execution_evidence = state.execution_evidence;
    Ok(LlmResponse {
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
    let execution_evidence = state.execution_evidence;
    LlmResponse {
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
