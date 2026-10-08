/// version_surface = "coexist"
/// version_guard(items(LASH_OPENAI_RESPONSES_REQUEST_DOMAIN_VERSION, request_fingerprint))
const LASH_OPENAI_RESPONSES_REQUEST_DOMAIN_VERSION: &str = "lash-openai-responses-request/v2";

use crate::request_work::{body_excerpt, needs_blocking, run};
use crate::support::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionEndpoint {
    Responses,
    ChatCompletions,
}

struct ResponseDecode {
    stream_events: Option<LlmEventSender>,
    provider_trace: Option<LlmProviderTraceSender>,
    url: String,
    http_summary: String,
    stream_termination: StreamTermination,
    responses_resume: Option<ResponsesResumeCheckpoint>,
    request_key: ResponsesRequestKey,
    tool_argument_decoder: crate::responses_shared::ToolArgumentDecoder,
    /// The request's recorded `expose_thinking` default: whether reasoning
    /// the provider streams is published.
    expose_thinking: bool,
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
            .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
    })?;
    url.path_segments_mut()
        .map_err(|_| {
            LlmTransportError::new("OpenAI Responses resume URL cannot carry path segments")
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
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

pub(crate) fn build_request_body(
    provider: &OpenAiCompatibleProvider,
    req: &LlmRequest,
    endpoint: CompletionEndpoint,
    stream: bool,
    origin_route: &ProviderRouteIdentity,
) -> Result<BuiltRequest, LlmTransportError> {
    let mut reserved_headers = vec![
        provider.wire.auth_header_name.as_str(),
        "content-type",
        "accept",
    ];
    if provider.resolved_compat(endpoint).cache_session_affinity {
        reserved_headers.push("x-client-request-id");
    }
    validate_extra_headers(&provider.wire.extra_headers, &reserved_headers, false)?;
    let mut built = match endpoint {
        CompletionEndpoint::Responses => {
            provider.build_responses_request_for_route(req, stream, origin_route)?
        }
        CompletionEndpoint::ChatCompletions => {
            provider
                .build_chat_request_body_with_diagnostics(req, stream)?
                .0
        }
    };
    if provider.resolved_compat(endpoint).cache_session_affinity {
        built.body["session_id"] = Value::String(req.scope.provider_session_affinity_key());
    }
    Ok(built)
}

fn request_fingerprint(body: &[u8]) -> ResponsesRequestFingerprint {
    lash_sansio::core_support::blake3_domain_hash(
        LASH_OPENAI_RESPONSES_REQUEST_DOMAIN_VERSION,
        body,
    )
}

/// The fingerprint of `body`: the bytes a resumed Responses stream belongs to.
#[expect(
    clippy::expect_used,
    reason = "the template contains only infallibly serializable JSON primitives"
)]
pub(crate) fn responses_request_fingerprint(
    body: &RecordedRequestTemplate,
) -> ResponsesRequestFingerprint {
    request_fingerprint(&serde_json::to_vec(body).expect("template serialization is infallible"))
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

/// Lower `req` to the exact body `endpoint` sends for it: reasoning from
/// another route dropped, the body built and serialized once.
pub(crate) async fn lower(
    provider: &OpenAiCompatibleProvider,
    req: &LlmRequest,
    endpoint: CompletionEndpoint,
) -> Result<RecordedRequestTemplate, LlmTransportError> {
    let route = ProviderRouteIdentity::for_endpoint(
        endpoint.provider_kind(),
        &provider.base_url,
        req.model.wire_model(),
    );
    route.validate_endpoint().map_err(|error| {
        LlmTransportError::new(error.to_string())
            .with_kind(ProviderFailureKind::Validation)
            .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
    })?;
    let mut safe = req.clone();
    safe.drop_foreign_replay(&route);
    // Clone construction settings without retaining a response checkpoint.
    let builder = OpenAiCompatibleProvider {
        tokens: std::sync::Arc::clone(&provider.tokens),
        base_url: provider.base_url.clone(),
        options: provider.options.clone(),
        attachment_credential_scope: provider.attachment_credential_scope.clone(),
        compat: provider.compat.clone(),
        wire: provider.wire.clone(),
        transport: provider.transport.clone(),
        responses_resume: None,
    };
    let build_route = route.clone();
    let stream = req.stream_events.is_some();
    let BuiltRequest { body, receipt } = run(needs_blocking(req), move || {
        build_request_body(&builder, &safe, endpoint, stream, &build_route)
    })
    .await??;
    let codec = match endpoint {
        CompletionEndpoint::Responses => crate::attachment_delivery::RESPONSES_CODEC,
        CompletionEndpoint::ChatCompletions => crate::attachment_delivery::CHAT_CODEC,
    };
    let patterns: &[&str] = match endpoint {
        CompletionEndpoint::Responses => &["/input/*/content/*", "/input/*/output/*"],
        CompletionEndpoint::ChatCompletions => &["/messages/*/content/*/image_url/url"],
    };
    let scope = provider
        .attachment_credential_scope
        .as_ref()
        .map(|credential_scope| ProviderFileScope {
            provider: route.provider.to_string(),
            endpoint: route.endpoint.to_string(),
            credential_scope: credential_scope.clone(),
        });
    let accepts = |mime: &lash_sansio::MediaType, position| match endpoint {
        CompletionEndpoint::Responses => {
            crate::attachment_delivery::responses_accepts(mime, scope.clone())
        }
        CompletionEndpoint::ChatCompletions => {
            if position == AttachmentPosition::Message && crate::attachment_delivery::image(mime) {
                ProviderAccepts {
                    bytes: true,
                    url: true,
                    provider_file: None,
                }
            } else {
                ProviderAccepts::NONE
            }
        }
    };
    lower_attachment_json(
        accepts,
        req,
        route,
        (req.stream_events.is_some(), Some(receipt)),
        &body,
        codec,
        patterns,
    )
}

/// Lower `req` and send its body, as one call of a test that scripts the
/// transport.
#[cfg(test)]
pub(crate) async fn complete(
    provider: &mut OpenAiCompatibleProvider,
    req: LlmRequest,
    endpoint: CompletionEndpoint,
) -> Result<LlmResponse, LlmTransportError> {
    let template = lower(provider, &req, endpoint).await?;
    let body =
        LiveRequestBody::fill(std::sync::Arc::new(template), Vec::new()).map_err(template_error)?;
    send(provider, &body, ResponseContext::of_request(&req), endpoint).await
}

/// Send `body`, which [`lower`] produced, to `endpoint` and read its
/// response under `context`: one attempt with the host's current token, and
/// one resend of the same body with a replaced token after a 401 that
/// arrived before any output.
pub(crate) async fn send(
    provider: &mut OpenAiCompatibleProvider,
    body: &LiveRequestBody,
    mut context: ResponseContext,
    endpoint: CompletionEndpoint,
) -> Result<LlmResponse, LlmTransportError> {
    let has_slots = body.template().slots().next().is_some();
    if has_slots {
        provider.responses_resume = None;
    }
    protect_callbacks(&mut context, body);
    let result = async {
        let route = ProviderRouteIdentity::for_endpoint(
            endpoint.provider_kind(),
            &provider.base_url,
            context.model().wire_model().to_string(),
        );
        let tokens = std::sync::Arc::clone(&provider.tokens);
        let mut lease = tokens.current(&route).await?;
        match send_attempt(provider, &context, body, endpoint, &lease.token).await {
            Err(error) if rejected_before_output(&error) => {
                match tokens
                    .replace(&route, &lease, TokenRequestReason::Rejected)
                    .await?
                {
                    Some(fresh) => {
                        lease = fresh;
                        send_attempt(provider, &context, body, endpoint, &lease.token).await
                    }
                    None => Err(error),
                }
            }
            other => other,
        }
    }
    .await;
    // A checkpoint can contain echoed operands from the previous attempt;
    // its redaction patterns must never outlive this live body.
    if has_slots {
        provider.responses_resume = None;
    }
    protect_result(result, body)
}

/// One attempt of [`send`], authenticated by `token`.
async fn send_attempt(
    provider: &mut OpenAiCompatibleProvider,
    context: &ResponseContext,
    body: &LiveRequestBody,
    endpoint: CompletionEndpoint,
    token: &ProviderToken,
) -> Result<LlmResponse, LlmTransportError> {
    let origin_model = context.model().wire_model().to_string();
    let origin_route = ProviderRouteIdentity::for_endpoint(
        endpoint.provider_kind(),
        &provider.base_url,
        origin_model.clone(),
    );
    origin_route.validate_endpoint().map_err(|error| {
        LlmTransportError::new(error.to_string())
            .with_kind(ProviderFailureKind::Validation)
            .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
    })?;
    let stream_events = context.stream_events.clone().map(|downstream| {
        let origin_route = origin_route.clone();
        LlmEventSender::new(move |mut event| {
            if let LlmStreamEvent::Part(part) = &mut event {
                let _ = part.stamp_replay_origin(&origin_route);
            }
            downstream.send(event);
        })
    });
    let provider_trace = context.provider_trace.clone();
    let expose_thinking = context.model().metadata().request_defaults.expose_thinking;
    let request_defaults = context.model().metadata().request_defaults.clone();
    let timeouts = provider.options.llm_timeouts();
    let stream = body.stream();
    let compat = provider.resolved_compat(endpoint);
    let stream_termination = context
        .model()
        .metadata()
        .capability
        .stream_termination
        .unwrap_or(compat.stream_termination);
    let request_id = context.scope.request_id.clone();
    let wire = body.wire();
    let blocking = crate::request_work::bytes_need_blocking(wire.len());
    let body_bytes = wire.as_bytes();
    let generation_disposition = body.generation();
    let fingerprint = responses_request_fingerprint(body.template());
    let request_body_for_error = body.redacted();
    emit_provider_request_trace(
        context.provider_trace.as_ref(),
        "openai_compatible",
        endpoint.request_trace_name(),
        body.redacted().as_bytes(),
    );
    let tool_argument_decoder = crate::responses_shared::ToolArgumentDecoder::for_contract(
        endpoint.provider_kind(),
        &context.contract,
        &compat.schema_capabilities,
    )?;
    let request_key = ResponsesRequestKey {
        request_id: request_id.clone(),
        fingerprint,
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
    let request_body = bytes::Bytes::copy_from_slice(body_bytes);
    let base_url = provider.base_url.trim_end_matches('/');
    let mut creation_url = match base_url.split_once('?') {
        Some((base_path, query)) => format!("{}/{}?{}", base_path, endpoint.path(), query),
        None => format!("{}/{}", base_url, endpoint.path()),
    };
    if !provider.wire.query_params.is_empty() {
        let mut parsed = reqwest::Url::parse(&creation_url).map_err(|error| {
            LlmTransportError::new(format!("Invalid OpenAI-compatible request URL: {error}"))
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
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
            lash_llm_transport::HttpHeaderValue::sensitive(format!(
                "{}{}",
                provider.wire.auth_value_prefix,
                token.secret().expose_secret()
            )),
        ),
        (
            "Content-Type".to_string(),
            "application/json".to_string().into(),
        ),
        ("Accept".to_string(), "text/event-stream".to_string().into()),
    ];
    if compat.cache_session_affinity {
        headers.push(("x-client-request-id".to_string(), request_id.clone().into()));
    }
    merge_extra_headers(&mut headers, &provider.wire.extra_headers, false)?;
    let http_request = LlmHttpRequest {
        method: http_method,
        url: url.clone(),
        headers,
        body: wire_body,
        delivery_redactor: Some(body.redactor()),
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
            response_start_timeout(
                timeouts.request_timeout,
                timeouts.response_start_timeout,
                stream,
            ),
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
            provider.options.response_body_limit(),
            timeouts.request_timeout,
            endpoint.response_body_timeout_error(),
        )
        .await;
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                let failure = error
                    .with_http_status(status)
                    .with_headers(headers)
                    .with_request_body(request_body_for_error);
                if let Some(resume) = responses_resume {
                    return Err(responses_stream_failure(
                        provider,
                        resume.request_key,
                        resume.state,
                        Some(resume.starting_after),
                        true,
                        http_summary,
                        failure,
                    ));
                }
                return Err(failure);
            }
        };
        let mut failure = run(
            crate::request_work::bytes_need_blocking(text.len()),
            move || {
                let message = format!("{} with {}", endpoint.request_failed_prefix(), status);
                let diagnostic = body_excerpt(&text);
                let value = serde_json::from_str::<Value>(&text).ok();
                let metadata = value.as_ref().and_then(crate::request_work::error_metadata);
                // Classify the original response, even when its code lies beyond
                // the diagnostic excerpt. Only bounded strings enter the envelope.
                let mut failure = http_error_envelope(
                    message,
                    status,
                    headers,
                    metadata
                        .as_deref()
                        .unwrap_or_else(|| crate::request_work::body_prefix(&text)),
                    Some(request_body_for_error),
                );
                if let Some(value) = value {
                    failure = classify_openai_error(&value, failure);
                }
                failure.raw = Some(Box::new(diagnostic));
                failure
            },
        )
        .await?;
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
    let mut capture = ResponseMetadataCapture::from_response(&request_defaults, &resp.headers);
    // Reattachment is another HTTP request for the same logical generation.
    // Its transport request id belongs in the per-attempt record, but sending
    // a second response-start Evidence event would conflict with the live
    // stream's already-established request identity and request summary.
    if responses_resume.is_none()
        && let Some(tx) = &stream_events
    {
        tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
            response_started: true,
            request_body: Some(request_body_text(request_body.clone(), blocking).await?),
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
                .with_lash_code(TurnFailureCode::ResponsesResumeNotStreaming)
                .with_retry_verdict(TransportRetryVerdict::NotRetryable),
        ));
    }

    let response_context = ResponseDecode {
        stream_events,
        provider_trace,
        url,
        http_summary,
        stream_termination,
        responses_resume,
        request_key,
        tool_argument_decoder,
        expose_thinking,
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
                            .with_lash_code(TurnFailureCode::ProviderReplayOriginConflict)
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
    response.request_body = Some(request_body_text(request_body, blocking).await?);
    response.response_metadata = capture.into_metadata();
    response.generation_disposition = generation_disposition;
    response
        .stamp_replay_origin(&origin_route)
        .map_err(|conflict| {
            LlmTransportError::new(conflict.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::ProviderReplayOriginConflict)
        })?;
    Ok(response)
}

async fn request_body_text(
    body: bytes::Bytes,
    blocking: bool,
) -> Result<String, LlmTransportError> {
    run(blocking, move || {
        String::from_utf8_lossy(&body).into_owned()
    })
    .await
}

async fn complete_buffered_response(
    provider: &OpenAiCompatibleProvider,
    endpoint: CompletionEndpoint,
    body: LlmHttpBody,
    timeout: Option<std::time::Duration>,
    context: ResponseDecode,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseDecode {
        stream_events,
        provider_trace,
        url,
        http_summary,
        stream_termination,
        tool_argument_decoder,
        expose_thinking,
        ..
    } = context;
    let stream_termination = stream_events.is_some().then_some(stream_termination);
    let text = read_http_body_text(
        body,
        provider.options.response_body_limit(),
        timeout,
        endpoint.response_body_timeout_error(),
    )
    .await?;
    capture.capture_body_text(&text);
    emit_provider_trace(provider_trace.as_ref(), "openai_compatible", &text);
    match endpoint {
        CompletionEndpoint::Responses => complete_buffered_responses(
            text,
            stream_events,
            http_summary,
            stream_termination,
            tool_argument_decoder,
            expose_thinking,
        ),
        CompletionEndpoint::ChatCompletions => complete_buffered_chat(
            text,
            stream_events,
            url,
            stream_termination,
            tool_argument_decoder,
            expose_thinking,
        ),
    }
}

fn complete_buffered_responses(
    text: String,
    stream_events: Option<LlmEventSender>,
    http_summary: String,
    stream_termination: Option<StreamTermination>,
    tool_argument_decoder: crate::responses_shared::ToolArgumentDecoder,
    expose_thinking: bool,
) -> Result<LlmResponse, LlmTransportError> {
    let mut state = ResponsesStreamState::with_tool_argument_decoder(tool_argument_decoder);
    state.expose_thinking = expose_thinking;
    let body_was_sse = text.trim_start().starts_with("data:") || text.contains("\ndata:");
    if body_was_sse {
        OpenAiCompatibleProvider::parse_sse_payload(&text, &mut state)?;
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Responses JSON: {e}"))
                .with_raw(body_excerpt(&text))
        })?;
        state.capture_execution_evidence(&value, true)?;
        state.provider_usage = value.get("usage").cloned();
        state.usage = usage_from_response_value(&value);
        state.parts = crate::responses_shared::response_parts_from_value_with_decoder(
            &value,
            &state.tool_argument_decoder,
        );
        state.completed_status_seen =
            value.get("status").and_then(Value::as_str) == Some("completed");
        state.final_response = Some(value);
    }
    let terminal_event_seen = state.terminal_event_seen
        || state
            .final_response
            .as_ref()
            .and_then(|response| response.get("status").and_then(Value::as_str))
            .is_some_and(|status| matches!(status, "completed" | "incomplete" | "failed"));
    if !terminal_event_seen
        && (stream_termination == Some(StreamTermination::RequireTerminalEvidence)
            || (body_was_sse && !state.has_output()))
    {
        let output_started = state.output_started();
        let mut partial = shared_response_from_state(state, http_summary);
        partial.terminal_reason = LlmTerminalReason::Unknown;
        return Err(LlmTransportError::new(
            "OpenAI Responses stream ended before a terminal response event",
        )
        .with_kind(ProviderFailureKind::Stream)
        .with_lash_code(TurnFailureCode::StreamEndedBeforeTerminalResponse)
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
    if invalid_empty_response(&parts, terminal_reason, state.completed_status_seen) {
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
        if body_was_sse {
            // The body was itself an SSE payload: the stream events were
            // already minted while folding it.
            for event in state.take_block_events() {
                if !expose_thinking && is_reasoning_block_event(&event) {
                    continue;
                }
                tx.send(event);
            }
            if expose_thinking {
                for part in &parts {
                    if matches!(part, LlmOutputPart::Reasoning { .. }) {
                        tx.send(LlmStreamEvent::Part(part.clone()));
                    }
                }
            }
        } else {
            let mut next_ordinal = 0u64;
            if expose_thinking {
                for part in &parts {
                    if let LlmOutputPart::Reasoning { .. } = part {
                        for (block, text) in reasoning_part_block_texts(part, &mut next_ordinal) {
                            if text.is_empty() {
                                continue;
                            }
                            tx.send(LlmStreamEvent::ReasoningBlockStart {
                                block: block.clone(),
                            });
                            tx.send(LlmStreamEvent::ReasoningDelta {
                                block: block.clone(),
                                text: text.clone(),
                            });
                            tx.send(LlmStreamEvent::ReasoningBlockEnd { block, text });
                        }
                        tx.send(LlmStreamEvent::Part(part.clone()));
                    }
                }
            }
            // Each visible message item is its own text block, mirroring the
            // live SSE mint (`message:{item_id}` / `text:{ordinal}`).
            for part in &parts {
                let LlmOutputPart::Text {
                    text,
                    response_meta,
                } = part
                else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                let block = crate::responses_shared::text_part_block_identity(
                    response_meta.as_ref().and_then(|meta| meta.id.as_deref()),
                    &mut next_ordinal,
                );
                tx.send(LlmStreamEvent::TextBlockStart {
                    block: block.clone(),
                });
                tx.send(LlmStreamEvent::Delta {
                    block: block.clone(),
                    text: text.clone(),
                });
                tx.send(LlmStreamEvent::TextBlockEnd {
                    block,
                    text: text.clone(),
                });
            }
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
        expose_thinking: Some(state.expose_thinking),
    })
}

fn complete_buffered_chat(
    text: String,
    stream_events: Option<LlmEventSender>,
    url: String,
    stream_termination: Option<StreamTermination>,
    tool_argument_decoder: crate::responses_shared::ToolArgumentDecoder,
    expose_thinking: bool,
) -> Result<LlmResponse, LlmTransportError> {
    let mut state = ChatStreamState::with_tool_argument_decoder(tool_argument_decoder);
    state.expose_thinking = expose_thinking;
    let mut parsed_parts = None;
    if text.trim_start().starts_with("data:") || text.contains("\ndata:") {
        OpenAiCompatibleProvider::parse_chat_sse_payload(&text, &mut state)?;
    } else {
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            LlmTransportError::new(format!("Invalid Chat Completions JSON: {e}"))
                .with_raw(body_excerpt(&text))
        })?;
        state.capture_response_value(&value)?;
        state.provider_usage = value.get("usage").cloned();
        state.usage = usage_from_response_value(&value);
        let parts = OpenAiCompatibleProvider::chat_response_parts_from_value_with_decoder(
            &value,
            &state.tool_argument_decoder,
        );
        let terminal_reason = terminal_reason_from_chat_value(&value, &parts);
        state.full_text = parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        parsed_parts = Some(parts);
        state.terminal_reason = terminal_reason;
    }
    let body_was_sse = parsed_parts.is_none();
    let parts = parsed_parts.unwrap_or_else(|| state.parts());
    if state
        .execution_evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_finish_reason.as_ref())
        .is_none()
        && (stream_termination == Some(StreamTermination::RequireTerminalEvidence)
            || (body_was_sse && parts.is_empty()))
    {
        state.final_response_raw = Some(text);
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    if invalid_empty_response(&parts, state.terminal_reason, state.normal_stop_seen) {
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
        if body_was_sse {
            // The body was itself an SSE payload: the stream events were
            // already minted while folding it.
            for event in state.finish_blocks() {
                if !expose_thinking && is_reasoning_block_event(&event) {
                    continue;
                }
                tx.send(event);
            }
            if expose_thinking {
                for part in &parts {
                    if matches!(part, LlmOutputPart::Reasoning { .. }) {
                        tx.send(LlmStreamEvent::Part(part.clone()));
                    }
                }
            }
        } else {
            let mut next_ordinal = 0u64;
            if expose_thinking {
                for part in parts
                    .iter()
                    .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
                {
                    for (block, text) in reasoning_part_block_texts(part, &mut next_ordinal) {
                        if text.is_empty() {
                            continue;
                        }
                        tx.send(LlmStreamEvent::ReasoningBlockStart {
                            block: block.clone(),
                        });
                        tx.send(LlmStreamEvent::ReasoningDelta {
                            block: block.clone(),
                            text: text.clone(),
                        });
                        tx.send(LlmStreamEvent::ReasoningBlockEnd { block, text });
                    }
                    tx.send(LlmStreamEvent::Part(part.clone()));
                }
            }
            if !state.full_text.is_empty() {
                let block = StreamBlockIdentity::new(format!("text:{next_ordinal}"), next_ordinal);
                tx.send(LlmStreamEvent::TextBlockStart {
                    block: block.clone(),
                });
                tx.send(LlmStreamEvent::Delta {
                    block: block.clone(),
                    text: state.full_text.clone(),
                });
                tx.send(LlmStreamEvent::TextBlockEnd {
                    block,
                    text: state.full_text.clone(),
                });
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
        expose_thinking: Some(state.expose_thinking),
    })
}

async fn drive_streaming_response(
    provider: &mut OpenAiCompatibleProvider,
    endpoint: CompletionEndpoint,
    body: LlmHttpBody,
    chunk_timeout: Option<std::time::Duration>,
    stream_bounds: SseStreamBounds,
    context: ResponseDecode,
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
            drive_streaming_chat(body, chunk_timeout, stream_bounds, context, capture).await
        }
    }
}

async fn drive_streaming_responses(
    provider: &mut OpenAiCompatibleProvider,
    body: LlmHttpBody,
    chunk_timeout: Option<std::time::Duration>,
    stream_bounds: SseStreamBounds,
    context: ResponseDecode,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseDecode {
        stream_events,
        provider_trace,
        url: _,
        http_summary,
        stream_termination,
        responses_resume,
        request_key,
        tool_argument_decoder,
        expose_thinking,
    } = context;
    let resume_after = responses_resume
        .as_ref()
        .map(|resume| resume.starting_after);
    let mut last_sequence_number = resume_after;
    let mut sequence_cursor_valid = true;
    let mut state = responses_resume.map_or_else(
        || ResponsesStreamState::with_tool_argument_decoder(tool_argument_decoder),
        |resume| resume.state,
    );
    state.expose_thinking = expose_thinking;
    let mut emitted_parts = Vec::new();
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
                    .with_lash_code(TurnFailureCode::ResponsesResumeEventMissingSequence)
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
                state
                    .take_block_events()
                    .into_iter()
                    .filter(|event| expose_thinking || !is_reasoning_block_event(event)),
                &state.usage,
                &prev_usage,
            );
            if let Some(tx) = &stream_events {
                for part in emitted_parts.drain(..) {
                    if matches!(part, LlmOutputPart::Reasoning { .. }) && !expose_thinking {
                        continue;
                    }
                    tx.send(LlmStreamEvent::Part(part));
                }
            } else {
                emitted_parts.clear();
            }
            Ok(())
        },
    )
    .await;

    let seal_open_blocks = |state: &mut ResponsesStreamState| {
        if let Some(tx) = &stream_events {
            for event in state.finish_blocks() {
                if !expose_thinking && is_reasoning_block_event(&event) {
                    continue;
                }
                tx.send(event);
            }
        } else {
            state.finish_blocks();
        }
    };
    if let Err(error) = stream_result {
        seal_open_blocks(&mut state);
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

    // A stream that ended without its terminal event completes when its
    // route tolerates EOF and it produced output; one that produced none
    // failed before it began, whatever the route tolerates.
    if !state.terminal_event_seen
        && (stream_termination == StreamTermination::RequireTerminalEvidence || !state.has_output())
    {
        seal_open_blocks(&mut state);
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
            .with_lash_code(TurnFailureCode::StreamEndedBeforeTerminalResponse)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient),
        ));
    }
    seal_open_blocks(&mut state);

    let parts = state.response_parts();
    let terminal_reason = state
        .final_response
        .as_ref()
        .map(|value| terminal_reason_from_responses_value(value, &parts))
        .unwrap_or_else(|| terminal_reason_from_parts(&parts));
    if invalid_empty_response(&parts, terminal_reason, state.completed_status_seen) {
        return Err(empty_response_diagnostic(
            state
                .final_response
                .as_ref()
                .map(crate::request_work::json_excerpt)
                .unwrap_or_else(|| body_excerpt("")),
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
        expose_thinking: Some(state.expose_thinking),
    })
}

async fn drive_streaming_chat(
    body: LlmHttpBody,
    chunk_timeout: Option<std::time::Duration>,
    stream_bounds: SseStreamBounds,
    context: ResponseDecode,
    capture: &mut ResponseMetadataCapture,
) -> Result<LlmResponse, LlmTransportError> {
    let ResponseDecode {
        stream_events,
        provider_trace,
        url,
        http_summary: _,
        stream_termination,
        tool_argument_decoder,
        expose_thinking,
        ..
    } = context;
    let mut state = ChatStreamState::with_tool_argument_decoder(tool_argument_decoder);
    state.expose_thinking = expose_thinking;
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
                state
                    .take_block_events()
                    .into_iter()
                    .filter(|event| expose_thinking || !is_reasoning_block_event(event)),
                &state.usage,
                &prev_usage,
            );
            if let Some(tx) = &stream_events {
                for part in state.take_completed_tool_call_parts() {
                    tx.send(LlmStreamEvent::Part(part));
                }
            }
            Ok(())
        },
    )
    .await;

    let seal_open_blocks = |state: &mut ChatStreamState| {
        if let Some(tx) = &stream_events {
            for event in state.finish_blocks() {
                if !expose_thinking && is_reasoning_block_event(&event) {
                    continue;
                }
                tx.send(event);
            }
        } else {
            state.finish_blocks();
        }
    };
    if let Err(error) = stream_result {
        seal_open_blocks(&mut state);
        return Err(error.with_partial_response(chat_response_from_state(state, &url)));
    }

    if state
        .execution_evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_finish_reason.as_ref())
        .is_none()
        && (stream_termination == StreamTermination::RequireTerminalEvidence
            || state.parts().is_empty())
    {
        seal_open_blocks(&mut state);
        return Err(LlmTransportError::new("Stream ended without finish_reason")
            .with_kind(ProviderFailureKind::Stream)
            .with_lash_code(TurnFailureCode::StreamEndedBeforeFinishReason)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(chat_response_from_state(state, &url)));
    }
    let parts = state.parts();
    if invalid_empty_response(&parts, state.terminal_reason, state.normal_stop_seen) {
        return Err(empty_response_error(
            state.final_response_raw.take().unwrap_or_default(),
        ));
    }
    if let Some(tx) = &stream_events {
        for event in state.finish_blocks() {
            if !expose_thinking && is_reasoning_block_event(&event) {
                continue;
            }
            tx.send(event);
        }
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
        expose_thinking: Some(state.expose_thinking),
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
        expose_thinking: Some(state.expose_thinking),
    }
}
