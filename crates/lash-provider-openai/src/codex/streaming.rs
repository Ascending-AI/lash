//! Streaming a Codex response to completion.
//!
//! One responsibility: drive one `complete` call over whichever transport
//! applies. The WebSocket path sends a `response.create` frame, folds frames
//! into the shared Responses stream state, and owns the two one-shot retries
//! (stale continuation, dead reused connection) plus the attempt diagnostics
//! that explain the outcome. The HTTP path posts the same body and drives the
//! SSE stream. Both end in the shared response assembly, and `Auto` falls back
//! from the first to the second only while no stream events have been seen.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use lash_core::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict, TurnFailureCode,
};
use lash_core::llm::types::{
    ExecutionEvidence, LlmRequest, LlmResponse, LlmStreamEvent, LlmStreamEvidence,
    LlmTerminalReason, LlmUsage, ProviderRequestBody, ProviderRouteIdentity,
};
use lash_core::provider::{
    LlmTimeouts, Provider, ProviderOptions, StreamTermination, TokenRequestReason,
};
use lash_llm_transport::streaming::{SseStreamBounds, drive_sse_response, emit_stream_progress};
use lash_llm_transport::timeouts::response_start_timeout;
use lash_llm_transport::util::{emit_provider_request_trace, emit_provider_trace};
use lash_llm_transport::{
    LlmHttpMethod, LlmHttpRequest, ResponseMetadataCapture, first_header_value, header_contains,
    http_error_envelope, openai_terminal_reason_from_response_value,
    openai_usage_from_response_value, read_http_body_text,
};
use lash_llm_transport::{TokenLease, merge_extra_headers, rejected_before_output};
use lash_sansio::FailureCode;

use crate::common::BuiltRequest;
use crate::responses_shared as shared;

use super::continuation::{
    CodexContinuation, CodexWebsocketContextPlan, CodexWebsocketRequestPlan,
};
use super::session::{CodexAttemptProgress, CodexWebSocketAttemptError, CodexWebsocketLease};
use super::{CodexProvider, CodexTransport, PROVIDER};

#[derive(Clone, Debug)]
struct CodexWebsocketAttemptDiagnostics<'a> {
    configured_transport: CodexTransport,
    reused_connection: bool,
    context: &'a CodexWebsocketContextPlan,
    request_bytes: usize,
    retry_state: CodexWebsocketRetryState,
}

struct CodexWebsocketAttemptGuard<'a> {
    provider: &'a CodexProvider,
    lease: Option<CodexWebsocketLease>,
}

impl<'a> CodexWebsocketAttemptGuard<'a> {
    fn new(provider: &'a CodexProvider, lease: CodexWebsocketLease) -> Self {
        Self {
            provider,
            lease: Some(lease),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "the guard is constructed with a lease and only `Drop` takes it back \
                  out, so every accessor on a live guard sees `Some`"
    )]
    fn lease(&self) -> &CodexWebsocketLease {
        self.lease
            .as_ref()
            .expect("WebSocket attempt guard owns its lease")
    }

    #[expect(
        clippy::expect_used,
        reason = "the guard is constructed with a lease and only `Drop` takes it back \
                  out, so every accessor on a live guard sees `Some`"
    )]
    fn lease_mut(&mut self) -> &mut CodexWebsocketLease {
        self.lease
            .as_mut()
            .expect("WebSocket attempt guard owns its lease")
    }

    fn finish(mut self, continuation: Option<CodexContinuation>) {
        if let Some(lease) = self.lease.take() {
            self.provider
                .release_websocket_lease(lease, true, continuation);
        }
    }
}

impl Drop for CodexWebsocketAttemptGuard<'_> {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.provider.release_websocket_lease(lease, false, None);
        }
    }
}

/// One-shot WebSocket retries already consumed by the current send loop.
#[derive(Clone, Copy, Debug, Default)]
struct CodexWebsocketRetryState {
    after_stale_previous_response: bool,
    after_dead_reused_connection: bool,
}

impl CodexProvider {
    fn should_parse_stream(stream_requested: bool, content_type: Option<&str>) -> bool {
        stream_requested
            || content_type
                .map(|ct| ct.contains("text/event-stream"))
                .unwrap_or(false)
    }

    fn non_sse_body_read_error(
        status: u16,
        content_type: Option<&str>,
        mut err: LlmTransportError,
    ) -> LlmTransportError {
        let content_type_detail = content_type
            .map(|ct| format!(" ({ct})"))
            .unwrap_or_default();
        let detail = std::mem::take(&mut err.message);
        err.message = format!(
            "Codex returned HTTP {status} with non-SSE body{content_type_detail} but it could not be read: {}",
            detail
        );
        err.http_status = Some(status);
        err.code
            .get_or_insert(FailureCode::lash(TurnFailureCode::BodyReadFailed));
        err
    }

    #[allow(
        clippy::result_large_err,
        reason = "the attempt error carries the transport error plus lease evidence; boxing it would push the cost onto every caller"
    )]
    async fn complete_websocket(
        &self,
        req: LlmRequest,
        built_request: &BuiltRequest,
        lease: &TokenLease,
    ) -> Result<LlmResponse, CodexWebSocketAttemptError> {
        let timeouts = self.options.llm_timeouts();
        // WebSocket connection policy is separate from the response-start
        // wait. Preserve its existing request/chunk-derived bound here.
        let connect_timeout = match (timeouts.request_timeout, timeouts.chunk_timeout) {
            (Some(request), Some(chunk)) => Some(request.min(chunk)),
            (request, chunk) => request.or(chunk),
        };
        let mut retry_state = CodexWebsocketRetryState::default();
        let mut allow_cached_context = self.websocket_continuation_enabled();
        loop {
            let websocket = self.acquire_websocket(&req, connect_timeout, lease).await?;
            let reused_connection = websocket.reused;
            let plan = self.websocket_request_plan(
                &built_request.body,
                websocket.continuation.as_ref(),
                allow_cached_context && websocket.reusable,
            );
            match self
                .run_websocket_attempt(&req, built_request, websocket, &plan, retry_state, timeouts)
                .await
            {
                Ok(response) => return Ok(response),
                Err(err)
                    if plan.context.is_continued()
                        && err.is_stale_previous_response()
                        && err.progress() < CodexAttemptProgress::OutputStarted
                        && !retry_state.after_stale_previous_response =>
                {
                    self.clear_continuation(&req);
                    retry_state.after_stale_previous_response = true;
                    allow_cached_context = false;
                    tracing::debug!(
                        target: "lash_core::llm::codex_oauth",
                        error = %err.error.message,
                        "Codex WebSocket cached continuation was stale; retrying once with full context"
                    );
                }
                Err(err)
                    if reused_connection
                        && err.progress() == CodexAttemptProgress::BeforeSend
                        && !retry_state.after_dead_reused_connection =>
                {
                    retry_state.after_dead_reused_connection = true;
                    allow_cached_context = false;
                    tracing::debug!(
                        target: "lash_core::llm::codex_oauth",
                        error = %err.error.message,
                        "Codex WebSocket cached connection failed before stream start; reconnecting once with full context"
                    );
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[allow(
        clippy::result_large_err,
        reason = "the attempt error carries the transport error plus lease evidence; boxing it would push the cost onto every caller"
    )]
    async fn run_websocket_attempt(
        &self,
        req: &LlmRequest,
        built_request: &BuiltRequest,
        lease: CodexWebsocketLease,
        plan: &CodexWebsocketRequestPlan,
        retry_state: CodexWebsocketRetryState,
        timeouts: LlmTimeouts,
    ) -> Result<LlmResponse, CodexWebSocketAttemptError> {
        let BuiltRequest {
            body: full_body,
            receipt,
        } = built_request;
        let mut attempt = CodexWebsocketAttemptGuard::new(self, lease);
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let stream_termination = req
            .model
            .metadata()
            .capability
            .stream_termination
            .unwrap_or_default();
        let websocket_body = Self::websocket_create_request(&plan.body);
        let request_body = match serde_json::to_string(&websocket_body) {
            Ok(request_body) => request_body,
            Err(error) => {
                return Err(CodexWebSocketAttemptError::before_send(
                    LlmTransportError::new(format!(
                        "Failed to serialize Codex WebSocket body: {error}"
                    )),
                ));
            }
        };
        emit_provider_request_trace(
            provider_trace.as_ref(),
            "codex",
            "responses",
            request_body.as_bytes(),
        );
        let diagnostics = CodexWebsocketAttemptDiagnostics {
            configured_transport: self.transport,
            reused_connection: attempt.lease().reused,
            context: &plan.context,
            request_bytes: request_body.len(),
            retry_state,
        };
        self.emit_websocket_attempt_trace(provider_trace.as_ref(), &diagnostics);
        let mut events_seen = false;
        let mut state = shared::ResponsesStreamState {
            expose_thinking: req.model.metadata().request_defaults.expose_thinking,
            ..Default::default()
        };
        if let Err(error) = attempt
            .lease_mut()
            .websocket
            .send(WsMessage::Text(request_body.clone().into()))
            .await
        {
            return Err(CodexWebSocketAttemptError::during_stream(
                LlmTransportError::new(format!("Codex WebSocket send failed: {error}"))
                    .with_request_body(request_body.clone())
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_lash_code(TurnFailureCode::WebsocketSend),
                events_seen,
                &state,
            ));
        }

        let expose_thinking = req.model.metadata().request_defaults.expose_thinking;
        let response_start_deadline = response_start_timeout(
            timeouts.request_timeout,
            timeouts.response_start_timeout,
            true,
        )
        .map(|timeout| tokio::time::Instant::now() + timeout);
        // One absolute cap for the whole request, as SSE keeps: once output
        // starts, the per-frame idle window must not let a steadily producing
        // stream outlive the configured request timeout.
        let absolute_deadline = timeouts
            .request_timeout
            .map(|timeout| tokio::time::Instant::now() + timeout);
        loop {
            let idle_deadline = if events_seen {
                timeouts
                    .chunk_timeout
                    .map(|timeout| tokio::time::Instant::now() + timeout)
            } else {
                response_start_deadline
            };
            let (read_deadline, absolute_deadline_wins) = match (absolute_deadline, idle_deadline) {
                (Some(absolute), Some(idle)) if absolute <= idle => (Some(absolute), true),
                (Some(absolute), None) => (Some(absolute), true),
                (_, idle) => (idle, false),
            };
            let next_message = match read_deadline {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, attempt.lease_mut().websocket.next()).await
                }
                None => Ok(attempt.lease_mut().websocket.next().await),
            };
            let Some(message) = (match next_message {
                Ok(message) => message,
                Err(_) => {
                    let (message, code) = if absolute_deadline_wins {
                        (
                            "Codex WebSocket request timed out",
                            TurnFailureCode::Timeout,
                        )
                    } else if events_seen {
                        (
                            "Codex WebSocket stream chunk timed out",
                            TurnFailureCode::WebsocketIdleTimeout,
                        )
                    } else {
                        (
                            "Codex WebSocket response start timed out",
                            TurnFailureCode::WebsocketIdleTimeout,
                        )
                    };
                    return Err(CodexWebSocketAttemptError::during_stream(
                        LlmTransportError::new(message)
                            .with_kind(ProviderFailureKind::Timeout)
                            .with_request_body(request_body.clone())
                            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                            .with_lash_code(code),
                        events_seen,
                        &state,
                    ));
                }
            }) else {
                break;
            };
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    return Err(CodexWebSocketAttemptError::during_stream(
                        LlmTransportError::new(format!("Codex WebSocket receive failed: {error}"))
                            .with_request_body(request_body.clone())
                            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                            .with_lash_code(TurnFailureCode::WebsocketReceive),
                        events_seen,
                        &state,
                    ));
                }
            };
            let raw = match message {
                WsMessage::Text(text) => text.to_string(),
                WsMessage::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(error) => {
                        return Err(CodexWebSocketAttemptError::during_stream(
                            LlmTransportError::new(format!(
                                "Codex WebSocket binary frame was not UTF-8: {error}"
                            ))
                            .with_request_body(request_body.clone())
                            .with_lash_code(TurnFailureCode::WebsocketProtocol),
                            events_seen,
                            &state,
                        ));
                    }
                },
                WsMessage::Close(_) => break,
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            };
            if !events_seen && let Some(tx) = &stream_events {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    response_started: true,
                    request_body: Some(request_body.clone()),
                    http_summary: Some(self.websocket_http_summary(&diagnostics)),
                    generation_disposition: Some(*receipt),
                    ..Default::default()
                }));
            }
            emit_provider_trace(provider_trace.as_ref(), "codex", &raw);
            events_seen = true;
            let prev_usage = state.usage.clone();
            let mut emitted_parts = Vec::new();
            let process_result = if Self::looks_like_sse_payload(&raw) {
                shared::parse_sse_payload(PROVIDER, &raw, &mut state)
            } else {
                shared::process_sse_event(PROVIDER, &raw, &mut state, Some(&mut emitted_parts))
            };
            if let Err(error) = process_result {
                let mut partial = shared::response_from_stream_state(
                    state.clone(),
                    Some(request_body.clone()),
                    self.websocket_http_summary(&diagnostics),
                );
                partial.terminal_reason = LlmTerminalReason::Unknown;
                partial.generation_disposition = Some(*receipt);
                return Err(CodexWebSocketAttemptError::during_stream(
                    error
                        .with_request_body(request_body.clone())
                        .with_partial_response(partial),
                    events_seen,
                    &state,
                ));
            }
            emit_stream_progress(
                stream_events.as_ref(),
                state.take_block_events().into_iter().filter(|event| {
                    expose_thinking || !crate::support::is_reasoning_block_event(event)
                }),
                &state.usage,
                &prev_usage,
            );
            if let Some(tx) = &stream_events
                && (state.provider_usage.is_some() || state.execution_evidence.is_some())
            {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    provider_usage: state.provider_usage.clone(),
                    execution_evidence: state.execution_evidence.clone(),
                    ..Default::default()
                }));
            }
            if let Some(tx) = &stream_events {
                for part in emitted_parts {
                    if matches!(part, lash_core::llm::types::LlmOutputPart::Reasoning { .. })
                        && !expose_thinking
                    {
                        continue;
                    }
                    tx.send(LlmStreamEvent::Part(part));
                }
            }
            if state.terminal_event_seen {
                break;
            }
        }

        let terminal_response_seen = state.terminal_event_seen;
        // A socket that closed before its terminal event completes when the
        // route tolerates EOF and the response produced output; one that
        // produced none (a dead reused socket) failed, whatever the route
        // tolerates.
        if !terminal_response_seen
            && (stream_termination == StreamTermination::RequireTerminalEvidence
                || !state.output_started())
        {
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                Some(request_body.clone()),
                self.websocket_http_summary(&diagnostics),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = Some(*receipt);
            return Err(CodexWebSocketAttemptError::during_stream(
                LlmTransportError::new("Codex WebSocket ended before response.completed")
                    .with_request_body(request_body)
                    .with_kind(ProviderFailureKind::Stream)
                    .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
                    .with_lash_code(TurnFailureCode::WebsocketClosedBeforeCompleted)
                    .with_partial_response(partial),
                events_seen,
                &state,
            ));
        }

        let final_response = state.final_response.clone();
        let continuation = final_response.as_ref().and_then(|response| {
            self.websocket_continuation_enabled()
                .then(|| Self::continuation_from_response(full_body, response))
                .flatten()
        });
        let mut response = shared::response_from_stream_state(
            state,
            Some(request_body.clone()),
            self.websocket_http_summary(&diagnostics),
        );
        response.http_summary = Some(self.websocket_http_summary(&diagnostics));
        response.generation_disposition = Some(*receipt);
        attempt.finish(continuation);
        Ok(response)
    }

    /// One attempt: the WebSocket path when it applies, else HTTP/SSE, with
    /// `lease`'s token bound to the request headers.
    async fn send_once(
        &self,
        req: &LlmRequest,
        built: &BuiltRequest,
        admitted: &ProviderRequestBody,
        lease: &TokenLease,
    ) -> Result<LlmResponse, LlmTransportError> {
        let stream_termination = req
            .model
            .metadata()
            .capability
            .stream_termination
            .unwrap_or_default();
        if !matches!(self.transport, CodexTransport::Sse) {
            let fallback_reason = matches!(self.transport, CodexTransport::Auto)
                .then(|| self.websocket_fallback_reason(req))
                .flatten();
            if let Some(reason) = fallback_reason {
                emit_provider_trace(
                    req.provider_trace.as_ref(),
                    "codex",
                    &json!({
                        "type": "lash.codex.websocket_fallback_skip",
                        "transport": format!("{:?}", self.transport),
                        "reason": reason,
                    })
                    .to_string(),
                );
                tracing::debug!(
                    target: "lash_core::llm::codex_oauth",
                    reason = %reason,
                    "Skipping Codex WebSocket for session with active Auto fallback"
                );
            } else {
                match self.complete_websocket(req.clone(), built, lease).await {
                    Ok(response) => {
                        self.clear_websocket_fallback(req);
                        return Ok(response);
                    }
                    // A rejected handshake goes to the token gate, not to an
                    // SSE fallback that would send the same token again.
                    Err(err)
                        if matches!(self.transport, CodexTransport::Auto)
                            && err.progress() == CodexAttemptProgress::BeforeSend
                            && err.error.http_status != Some(401) =>
                    {
                        self.record_websocket_fallback(req, &err.error);
                        tracing::debug!(
                            target: "lash_core::llm::codex_oauth",
                            error = %err.error.message,
                            "Codex WebSocket failed before stream start; falling back to SSE"
                        );
                    }
                    Err(err) => {
                        self.clear_continuation(req);
                        let output_started = err.progress() == CodexAttemptProgress::OutputStarted;
                        return Err(err.error.with_output_started(output_started));
                    }
                }
            }
        }
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let timeouts = self.options.llm_timeouts();

        let generation_disposition = Some(built.receipt);
        let request_body = Some(admitted.body.to_string());
        let body_bytes = admitted.body.as_bytes().to_vec();
        emit_provider_request_trace(provider_trace.as_ref(), "codex", "responses", &body_bytes);
        let mut headers = vec![
            (
                "Authorization".to_string(),
                lash_llm_transport::HttpHeaderValue::sensitive(format!(
                    "Bearer {}",
                    lease.token.secret().expose_secret()
                )),
            ),
            (
                "Content-Type".to_string(),
                "application/json".to_string().into(),
            ),
            ("Accept".to_string(), "text/event-stream".to_string().into()),
            (
                "OpenAI-Beta".to_string(),
                "responses=experimental".to_string().into(),
            ),
            ("originator".to_string(), Self::CODEX_ORIGINATOR.into()),
            ("User-Agent".to_string(), Self::codex_user_agent().into()),
            (
                "session-id".to_string(),
                req.scope.provider_session_affinity_key().into(),
            ),
            (
                "x-client-request-id".to_string(),
                req.scope.request_id.clone().into(),
            ),
        ];
        if let Some(id) = lease.token.account() {
            headers.push((
                "ChatGPT-Account-ID".to_string(),
                lash_llm_transport::HttpHeaderValue::sensitive(id.expose_secret()),
            ));
        }
        merge_extra_headers(&mut headers, &self.extra_headers, false)?;
        let http_request = LlmHttpRequest {
            method: LlmHttpMethod::Post,
            url: self.responses_url.clone(),
            headers,
            body: bytes::Bytes::from(body_bytes),
            body_for_error: request_body.clone(),
            response_start_timeout_message: Some("Codex response start timed out".to_string()),
        };
        let stream_bounds = SseStreamBounds::new(timeouts.request_timeout, &self.options);
        let resp = self
            .http_transport
            .send(
                http_request,
                response_start_timeout(
                    timeouts.request_timeout,
                    timeouts.response_start_timeout,
                    stream_events.is_some(),
                ),
            )
            .await?;
        let status = resp.status;
        let content_type = first_header_value(&resp.headers, "content-type").map(str::to_string);
        let response_headers = resp.headers.clone();
        let provider_request_id =
            first_header_value(&response_headers, "x-request-id").map(str::to_string);
        let is_sse = header_contains(&resp.headers, "content-type", "text/event-stream");
        let success = resp.is_success();
        let body = resp.body;
        if !success {
            let text = read_http_body_text(
                body,
                self.options.response_body_limit(),
                timeouts.request_timeout,
                "Codex response body timed out",
            )
            .await?;
            let message = Self::codex_error_summary(status, &text).unwrap_or_else(|| {
                format!(
                    "Codex request failed with {}{}",
                    status,
                    content_type
                        .as_deref()
                        .map(|ct| format!(" ({ct})"))
                        .unwrap_or_default()
                )
            });

            // Retryability is decided centrally by `CodexFailureClassifier`
            // from the attached HTTP status; no inline override here.
            return Err(http_error_envelope(
                message,
                status,
                response_headers,
                text,
                request_body.clone(),
            ));
        }
        let mut response_metadata = ResponseMetadataCapture::from_response(
            &req.model.metadata().request_defaults,
            &response_headers,
        );
        if let Some(tx) = &stream_events {
            tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                response_started: true,
                request_body: request_body.clone(),
                http_summary: Some(format!("HTTP POST {} (stream)", self.responses_url)),
                execution_evidence: provider_request_id.clone().map(|provider_request_id| {
                    ExecutionEvidence {
                        provider_request_id: Some(provider_request_id),
                        ..Default::default()
                    }
                }),
                generation_disposition,
                response_metadata: response_metadata.metadata(),
                ..Default::default()
            }));
        }

        let parse_stream =
            Self::should_parse_stream(stream_events.is_some(), content_type.as_deref());

        if !parse_stream {
            let text = read_http_body_text(
                body,
                self.options.response_body_limit(),
                timeouts.request_timeout,
                "Codex response body timed out",
            )
            .await
            .map_err(|err| Self::non_sse_body_read_error(status, content_type.as_deref(), err))?;
            response_metadata.capture_body_text(&text);
            emit_provider_trace(provider_trace.as_ref(), "codex", &text);
            if Self::looks_like_sse_payload(&text) {
                let mut state = shared::ResponsesStreamState {
                    expose_thinking: req.model.metadata().request_defaults.expose_thinking,
                    execution_evidence: provider_request_id.clone().map(|provider_request_id| {
                        ExecutionEvidence {
                            provider_request_id: Some(provider_request_id),
                            ..Default::default()
                        }
                    }),
                    ..Default::default()
                };
                shared::parse_sse_payload(PROVIDER, &text, &mut state)?;
                let block_events = state.take_block_events();
                let mut response = shared::response_from_stream_state(
                    state,
                    request_body,
                    format!("HTTP POST {} (stream/fallback)", self.responses_url),
                );
                response.generation_disposition = generation_disposition;
                response.response_metadata = response_metadata.into_metadata();
                if let Some(tx) = &stream_events {
                    tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                        provider_usage: response.provider_usage.clone(),
                        execution_evidence: response.execution_evidence.clone(),
                        ..Default::default()
                    }));
                    if response.usage != LlmUsage::default() {
                        tx.send(LlmStreamEvent::Usage(response.usage.clone()));
                    }
                    // The body was itself an SSE payload: the block events
                    // were already minted while folding it.
                    for event in block_events {
                        if !req.model.metadata().request_defaults.expose_thinking
                            && crate::support::is_reasoning_block_event(&event)
                        {
                            continue;
                        }
                        tx.send(event);
                    }
                    for part in &response.parts {
                        match part {
                            lash_core::llm::types::LlmOutputPart::ToolCall { .. } => {
                                tx.send(LlmStreamEvent::Part(part.clone()));
                            }
                            lash_core::llm::types::LlmOutputPart::Reasoning { .. }
                                if req.model.metadata().request_defaults.expose_thinking =>
                            {
                                tx.send(LlmStreamEvent::Part(part.clone()));
                            }
                            _ => {}
                        }
                    }
                }
                return Ok(response);
            }
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                LlmTransportError::new(format!("Invalid Codex response JSON: {e}"))
                    .with_raw(text.clone())
            })?;
            let mut evidence_state = shared::ResponsesStreamState {
                execution_evidence: provider_request_id.map(|provider_request_id| {
                    ExecutionEvidence {
                        provider_request_id: Some(provider_request_id),
                        ..Default::default()
                    }
                }),
                ..Default::default()
            };
            evidence_state.capture_execution_evidence(&value, true)?;
            let execution_evidence = evidence_state.execution_evidence;
            let content = shared::extract_text(&value);
            let provider_usage = value.get("usage").cloned();
            let usage = openai_usage_from_response_value(&value);
            let mut parts = shared::response_parts_from_value(&value);
            if parts.is_empty() && !content.is_empty() {
                parts.push(lash_core::llm::types::LlmOutputPart::Text {
                    text: content.clone(),
                    response_meta: None,
                });
            }
            if let Some(tx) = &stream_events {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    provider_usage: provider_usage.clone(),
                    execution_evidence: execution_evidence.clone(),
                    ..Default::default()
                }));
                if usage != LlmUsage::default() {
                    tx.send(LlmStreamEvent::Usage(usage.clone()));
                }
                let mut next_ordinal = 0u64;
                if req.model.metadata().request_defaults.expose_thinking {
                    for part in parts.iter().filter(|part| {
                        matches!(part, lash_core::llm::types::LlmOutputPart::Reasoning { .. })
                    }) {
                        for (block, text) in
                            crate::support::reasoning_part_block_texts(part, &mut next_ordinal)
                        {
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
                // Each visible message item is its own text block, mirroring
                // the live SSE mint (`message:{item_id}` / `text:{ordinal}`).
                for part in &parts {
                    let lash_core::llm::types::LlmOutputPart::Text {
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
            let terminal_reason = openai_terminal_reason_from_response_value(&value, &parts);
            return Ok(LlmResponse {
                parts,
                usage,
                terminal_reason,
                terminal_diagnostic: None,
                provider_usage,
                request_body,
                http_summary: Some(format!("HTTP POST {}", self.responses_url)),
                execution_evidence,
                generation_disposition,
                response_metadata: response_metadata.into_metadata(),
                expose_thinking: Some(req.model.metadata().request_defaults.expose_thinking),
            });
        }

        if stream_events.is_some() && !is_sse {
            tracing::debug!(
                target: "lash_core::llm::codex_oauth",
                status,
                content_type = content_type.as_deref().unwrap_or("<missing>"),
                "Codex streaming response did not advertise SSE; parsing as stream because stream=true was requested"
            );
        }

        let mut state = shared::ResponsesStreamState {
            expose_thinking: req.model.metadata().request_defaults.expose_thinking,
            execution_evidence: provider_request_id.map(|provider_request_id| ExecutionEvidence {
                provider_request_id: Some(provider_request_id),
                ..Default::default()
            }),
            ..Default::default()
        };
        let expose_thinking = req.model.metadata().request_defaults.expose_thinking;
        let stream_result = drive_sse_response(
            body,
            timeouts.chunk_timeout,
            stream_bounds,
            "Codex stream chunk timed out",
            "Codex request timed out",
            &mut response_metadata,
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "codex", raw);
                let prev_usage = state.usage.clone();
                let mut emitted_parts = Vec::new();
                shared::process_sse_event(PROVIDER, raw, &mut state, Some(&mut emitted_parts))?;
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
                    state.take_block_events().into_iter().filter(|event| {
                        expose_thinking || !crate::support::is_reasoning_block_event(event)
                    }),
                    &state.usage,
                    &prev_usage,
                );
                if let Some(tx) = &stream_events {
                    for part in emitted_parts {
                        if matches!(part, lash_core::llm::types::LlmOutputPart::Reasoning { .. })
                            && !expose_thinking
                        {
                            continue;
                        }
                        tx.send(LlmStreamEvent::Part(part));
                    }
                }
                Ok(())
            },
        )
        .await;

        let seal_open_blocks = |state: &mut shared::ResponsesStreamState| {
            if let Some(tx) = &stream_events {
                for event in state.finish_blocks() {
                    if !expose_thinking && crate::support::is_reasoning_block_event(&event) {
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
            let output_started = state.output_started();
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                request_body.clone(),
                format!("HTTP POST {} (stream)", self.responses_url),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = generation_disposition;
            partial.response_metadata = response_metadata.into_metadata();
            return Err(error
                .with_output_started(output_started)
                .with_partial_response(partial));
        }

        if !state.terminal_event_seen
            && (stream_termination == StreamTermination::RequireTerminalEvidence
                || !state.output_started())
        {
            seal_open_blocks(&mut state);
            let output_started = state.output_started();
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                request_body.clone(),
                format!("HTTP POST {} (stream)", self.responses_url),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = generation_disposition;
            partial.response_metadata = response_metadata.into_metadata();
            return Err(LlmTransportError::new(
                "Codex stream ended before a terminal response event",
            )
            .with_kind(ProviderFailureKind::Stream)
            .with_lash_code(TurnFailureCode::StreamEndedBeforeTerminalResponse)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_output_started(output_started)
            .with_partial_response(partial));
        }

        if state.final_response.is_none()
            && state.parts.is_empty()
            && !state.streamed_item_content_received
        {
            return Err(LlmTransportError::new(format!(
                "Codex stream ended without SSE events (HTTP {}{})",
                status,
                content_type
                    .as_deref()
                    .map(|ct| format!(", content-type {ct}"))
                    .unwrap_or_else(|| ", missing content-type".to_string())
            ))
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_lash_code(TurnFailureCode::EmptyStream));
        }

        seal_open_blocks(&mut state);
        let mut response = shared::response_from_stream_state(
            state,
            request_body,
            format!("HTTP POST {} (stream)", self.responses_url),
        );
        response.generation_disposition = generation_disposition;
        response.response_metadata = response_metadata.into_metadata();
        Ok(response)
    }

    fn websocket_http_summary(&self, diagnostics: &CodexWebsocketAttemptDiagnostics<'_>) -> String {
        let context = diagnostics.context.rendered();
        format!(
            "WS {} transport={:?} reused={} cached={} cache_miss={} retry_after_stale={} retry_after_dead_reused={} input_items={}/{} previous_response_id={} request_bytes={}",
            self.websocket_url,
            diagnostics.configured_transport,
            diagnostics.reused_connection,
            context.cached_request,
            context.cache_miss_reason.unwrap_or("<none>"),
            diagnostics.retry_state.after_stale_previous_response,
            diagnostics.retry_state.after_dead_reused_connection,
            context.sent_input_items,
            context.full_input_items,
            context.previous_response_id.unwrap_or("<none>"),
            diagnostics.request_bytes
        )
    }

    fn emit_websocket_attempt_trace(
        &self,
        provider_trace: Option<&lash_core::llm::types::LlmProviderTraceSender>,
        diagnostics: &CodexWebsocketAttemptDiagnostics<'_>,
    ) {
        let context = diagnostics.context.rendered();
        let raw = json!({
            "type": "lash.codex.websocket_request",
            "transport": format!("{:?}", diagnostics.configured_transport),
            "reused_connection": diagnostics.reused_connection,
            "cached_request": context.cached_request,
            "continuation_available": context.continuation_available,
            "cache_miss_reason": context.cache_miss_reason,
            "retry_after_stale_previous_response": diagnostics.retry_state.after_stale_previous_response,
            "retry_after_dead_reused_connection": diagnostics.retry_state.after_dead_reused_connection,
            "previous_response_id": context.previous_response_id,
            "full_input_items": context.full_input_items,
            "sent_input_items": context.sent_input_items,
            "request_bytes": diagnostics.request_bytes,
        })
        .to_string();
        emit_provider_trace(provider_trace, "codex", &raw);
    }

    fn looks_like_sse_payload(payload: &str) -> bool {
        let trimmed = payload.trim_start();
        trimmed.starts_with("event:")
            || trimmed.starts_with("data:")
            || payload.contains("\nevent:")
            || payload.contains("\ndata:")
    }

    #[cfg(test)]
    pub(super) fn process_sse_event(
        raw: &str,
        state: &mut shared::ResponsesStreamState,
        emitted_parts: Option<&mut Vec<lash_core::llm::types::LlmOutputPart>>,
    ) -> Result<(), LlmTransportError> {
        shared::process_sse_event(PROVIDER, raw, state, emitted_parts)
    }
}

fn codex_replay_origin_conflict(
    conflict: lash_core::llm::types::ProviderReplayOriginConflict,
    original: Option<LlmTransportError>,
) -> LlmTransportError {
    let has_original = original.is_some();
    let mut error = original.unwrap_or_else(|| LlmTransportError::new(conflict.to_string()));
    if has_original {
        error.message = format!(
            "{conflict}; original LLM Provider failure: {}",
            error.message
        );
    }
    error.kind = ProviderFailureKind::Validation;
    error.code = Some(FailureCode::lash(
        TurnFailureCode::ProviderReplayOriginConflict,
    ));
    error.retry_verdict = TransportRetryVerdict::Forbidden;
    error
}

/// `error` with its partial response stamped with the minting route, or the
/// replay-origin conflict that stamping found.
fn stamped_codex_failure(
    mut error: LlmTransportError,
    route: &ProviderRouteIdentity,
) -> LlmTransportError {
    if let Some(partial) = error.partial_response.as_deref_mut()
        && let Err(conflict) = partial.stamp_replay_origin(route)
    {
        return codex_replay_origin_conflict(conflict, Some(error));
    }
    error
}

#[async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> &'static str {
        "codex"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        let endpoint = match self.transport {
            CodexTransport::Websocket | CodexTransport::WebsocketCached => &self.websocket_url,
            // Auto may fall back from WebSocket to SSE within one logical call,
            // so its stable serving route remains the fallback-capable Responses
            // endpoint. A transport-pinned WebSocket provider has no such
            // ambiguity and reports the endpoint it actually serves from.
            CodexTransport::Auto | CodexTransport::Sse => &self.responses_url,
        };
        ProviderRouteIdentity::for_endpoint(self.kind(), endpoint, model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        // No credential: the host re-attaches its token source when it
        // rebuilds the provider.
        let mut map = serde_json::Map::new();
        if !self.options.is_default() {
            map.insert(
                "options".to_string(),
                serde_json::to_value(&self.options).unwrap_or(serde_json::Value::Null),
            );
        }
        if self.transport != CodexTransport::Auto {
            map.insert(
                "transport".to_string(),
                serde_json::to_value(self.transport).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(map)
    }

    fn requires_streaming(&self) -> bool {
        true
    }

    async fn lower(&mut self, req: &LlmRequest) -> Result<ProviderRequestBody, LlmTransportError> {
        let route = self.route_identity(req.model.wire_model());
        route.validate_endpoint().map_err(|error| {
            LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
        })?;
        // Every refusal lands before the host's token source is asked.
        self.preflight(req)?;
        let stream = req.stream_events.is_some();
        let BuiltRequest { body, receipt } = self.build_request(req, stream)?;
        let body = serde_json::to_string(&body).map_err(|e| {
            LlmTransportError::new(format!("Failed to serialize Codex request: {e}"))
        })?;
        Ok(ProviderRequestBody {
            route,
            stream,
            generation: Some(receipt),
            body: body.into(),
        })
    }

    async fn send(
        &mut self,
        mut req: LlmRequest,
        body: &ProviderRequestBody,
    ) -> Result<LlmResponse, LlmTransportError> {
        let route = self.route_identity(req.model.wire_model());
        route.validate_endpoint().map_err(|error| {
            LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::InvalidProviderEndpoint)
        })?;
        let built = BuiltRequest {
            body: serde_json::from_str(&body.body).map_err(|e| {
                LlmTransportError::new(format!("The Codex request body does not decode: {e}"))
                    .with_kind(ProviderFailureKind::Validation)
                    .with_lash_code(TurnFailureCode::AdmittedRequestUnavailable)
                    .with_retry_verdict(TransportRetryVerdict::Forbidden)
            })?,
            receipt: body.generation.unwrap_or_default(),
        };
        if let Some(downstream) = req.stream_events.take() {
            let stream_route = route.clone();
            req.stream_events = Some(lash_core::llm::types::LlmEventSender::new(
                move |mut event| {
                    if let LlmStreamEvent::Part(part) = &mut event {
                        let _ = part.stamp_replay_origin(&stream_route);
                    }
                    downstream.send(event);
                },
            ));
        }
        let tokens = Arc::clone(&self.tokens);
        let mut lease = tokens.current(&route).await?;
        let mut replaced = false;
        loop {
            match self.send_once(&req, &built, body, &lease).await {
                Ok(mut response) => {
                    response
                        .stamp_replay_origin(&route)
                        .map_err(|conflict| codex_replay_origin_conflict(conflict, None))?;
                    return Ok(response);
                }
                Err(error) if rejected_before_output(&error) && !replaced => {
                    match tokens
                        .replace(&route, &lease, TokenRequestReason::Rejected)
                        .await?
                    {
                        // Resend the admitted body once with the fresh token.
                        Some(fresh) => {
                            lease = fresh;
                            replaced = true;
                        }
                        None => return Err(stamped_codex_failure(error, &route)),
                    }
                }
                Err(error) => return Err(stamped_codex_failure(error, &route)),
            }
        }
    }

    async fn close(&self) -> Result<(), LlmTransportError> {
        // Drain the provider-local WebSocket session cache with real Close
        // frames. The cache is shared across clones (Arc), so closing any handle
        // a host retained releases the cached sockets for all of them.
        self.close_websocket_sessions().await;
        Ok(())
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
