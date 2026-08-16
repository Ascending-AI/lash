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
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;

use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
use lash_core::llm::types::{
    ExecutionEvidence, LlmRequest, LlmResponse, LlmStreamEvent, LlmStreamEvidence,
    LlmTerminalReason, LlmUsage, ProviderRouteIdentity,
};
use lash_core::provider::{Provider, ProviderOptions, StreamTermination};
use lash_llm_transport::streaming::{SseStreamBounds, drive_sse_response, emit_stream_progress};
use lash_llm_transport::timeouts::response_start_timeout;
use lash_llm_transport::util::{emit_provider_request_trace, emit_provider_trace};
use lash_llm_transport::{
    LlmHttpMethod, LlmHttpRequest, ResponseMetadataCapture, first_header_value, header_contains,
    http_error_envelope, openai_terminal_reason_from_response_value,
    openai_usage_from_response_value, read_http_body_text,
};
use lash_provider_auth::{CredentialCallError, CredentialExecuteError};

use crate::responses_shared as shared;

use super::continuation::{CodexContinuation, CodexWebsocketRequestPlan};
use super::credential::{CodexCredential, credential_transport_error};
use super::session::{CodexWebSocketAttemptError, CodexWebsocketLease};
use super::{CodexProvider, CodexTransport, PROVIDER};

#[derive(Clone, Debug)]
struct CodexWebsocketAttemptDiagnostics {
    configured_transport: CodexTransport,
    reused_connection: bool,
    cached_request: bool,
    continuation_available: bool,
    cache_miss_reason: Option<&'static str>,
    previous_response_id: Option<String>,
    full_input_items: usize,
    sent_input_items: usize,
    request_bytes: usize,
    retry_after_stale_previous_response: bool,
    retry_after_dead_reused_connection: bool,
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

    fn lease(&self) -> &CodexWebsocketLease {
        self.lease
            .as_ref()
            .expect("WebSocket attempt guard owns its lease")
    }

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
#[derive(Clone, Copy, Default)]
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
        err: LlmTransportError,
    ) -> LlmTransportError {
        let content_type_detail = content_type
            .map(|ct| format!(" ({ct})"))
            .unwrap_or_default();
        let code = err
            .code
            .clone()
            .unwrap_or_else(|| "body_read_failed".to_string());
        LlmTransportError::new(format!(
            "Codex returned HTTP {status} with non-SSE body{content_type_detail} but it could not be read: {}",
            err.message
        ))
        .retryable(err.retryable)
        .with_code(code)
    }

    fn response_state_started_output(state: &shared::ResponsesStreamState) -> bool {
        state.output_started()
    }

    fn is_stale_previous_response_error(error: &LlmTransportError) -> bool {
        let haystack = format!(
            "{}\n{}\n{}",
            error.message,
            error.raw.as_deref().map(String::as_str).unwrap_or_default(),
            error.code.as_deref().unwrap_or_default()
        )
        .to_ascii_lowercase();
        haystack.contains("previous_response_id")
            || haystack.contains("previous response")
            || haystack.contains("previous response with id")
    }

    async fn complete_websocket(
        &self,
        req: LlmRequest,
        credential: &CodexCredential,
        credential_generation: u64,
    ) -> Result<LlmResponse, CodexWebSocketAttemptError> {
        let full_body =
            self.build_request_body(&req, true)
                .map_err(|error| CodexWebSocketAttemptError {
                    error,
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                })?;
        let timeouts = self.options.llm_timeouts();
        let connect_timeout =
            response_start_timeout(timeouts.request_timeout, timeouts.chunk_timeout, true)
                .unwrap_or(timeouts.chunk_timeout);
        let mut retry_state = CodexWebsocketRetryState::default();
        let mut allow_cached_context = self.websocket_continuation_enabled();
        loop {
            let lease = self
                .acquire_websocket(&req, connect_timeout, credential, credential_generation)
                .await?;
            let reused_connection = lease.reused;
            let plan = self.websocket_request_plan(
                &full_body,
                lease.continuation.as_ref(),
                allow_cached_context && lease.reusable,
            );
            let cached_request = plan.cached;
            match self
                .run_websocket_attempt(
                    &req,
                    &full_body,
                    lease,
                    plan,
                    retry_state,
                    timeouts.chunk_timeout,
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(err)
                    if cached_request
                        && err.stale_previous_response
                        && !err.output_started
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
                        && !err.events_seen
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

    async fn run_websocket_attempt(
        &self,
        req: &LlmRequest,
        full_body: &Value,
        lease: CodexWebsocketLease,
        plan: CodexWebsocketRequestPlan,
        retry_state: CodexWebsocketRetryState,
        read_timeout: Duration,
    ) -> Result<LlmResponse, CodexWebSocketAttemptError> {
        let mut attempt = CodexWebsocketAttemptGuard::new(self, lease);
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let stream_termination = req
            .model_capability
            .stream_termination
            .unwrap_or(StreamTermination::RequireTerminalEvidence);
        let websocket_body = Self::websocket_create_request(&plan.body);
        let request_body = match serde_json::to_string(&websocket_body) {
            Ok(request_body) => request_body,
            Err(error) => {
                return Err(CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Failed to serialize Codex WebSocket body: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                });
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
            cached_request: plan.cached,
            continuation_available: plan.continuation_available,
            cache_miss_reason: plan.cache_miss_reason,
            previous_response_id: plan.previous_response_id.clone(),
            full_input_items: plan.full_input_items,
            sent_input_items: plan.sent_input_items,
            request_bytes: request_body.len(),
            retry_after_stale_previous_response: retry_state.after_stale_previous_response,
            retry_after_dead_reused_connection: retry_state.after_dead_reused_connection,
        };
        self.emit_websocket_attempt_trace(provider_trace.as_ref(), &diagnostics);
        let mut events_seen = false;
        if let Err(error) = attempt
            .lease_mut()
            .websocket
            .send(WsMessage::Text(request_body.clone().into()))
            .await
        {
            return Err(CodexWebSocketAttemptError {
                error: LlmTransportError::new(format!("Codex WebSocket send failed: {error}"))
                    .with_request_body(request_body.clone())
                    .retryable(true)
                    .with_code("websocket_send"),
                events_seen,
                output_started: false,
                stale_previous_response: false,
            });
        }

        let mut state = shared::ResponsesStreamState::default();
        let expose_thinking = self.options.expose_thinking;
        loop {
            let next_message =
                tokio::time::timeout(read_timeout, attempt.lease_mut().websocket.next()).await;
            let Some(message) = (match next_message {
                Ok(message) => message,
                Err(_) => {
                    let output_started = Self::response_state_started_output(&state);
                    return Err(CodexWebSocketAttemptError {
                        error: LlmTransportError::new("Codex WebSocket stream chunk timed out")
                            .with_kind(ProviderFailureKind::Timeout)
                            .with_request_body(request_body.clone())
                            .retryable(true)
                            .with_code("websocket_idle_timeout"),
                        events_seen,
                        output_started,
                        stale_previous_response: false,
                    });
                }
            }) else {
                break;
            };
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    let output_started = Self::response_state_started_output(&state);
                    return Err(CodexWebSocketAttemptError {
                        error: LlmTransportError::new(format!(
                            "Codex WebSocket receive failed: {error}"
                        ))
                        .with_request_body(request_body.clone())
                        .retryable(true)
                        .with_code("websocket_receive"),
                        events_seen,
                        output_started,
                        stale_previous_response: false,
                    });
                }
            };
            let raw = match message {
                WsMessage::Text(text) => text.to_string(),
                WsMessage::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(error) => {
                        let output_started = Self::response_state_started_output(&state);
                        return Err(CodexWebSocketAttemptError {
                            error: LlmTransportError::new(format!(
                                "Codex WebSocket binary frame was not UTF-8: {error}"
                            ))
                            .with_request_body(request_body.clone())
                            .with_code("websocket_protocol"),
                            events_seen,
                            output_started,
                            stale_previous_response: false,
                        });
                    }
                },
                WsMessage::Close(_) => break,
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            };
            if !events_seen && let Some(tx) = &stream_events {
                tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
                    request_body: Some(request_body.clone()),
                    http_summary: Some(self.websocket_http_summary(&diagnostics)),
                    generation_disposition: Some(Self::generation_disposition(req, full_body)),
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
                let output_started = Self::response_state_started_output(&state);
                let stale_previous_response = Self::is_stale_previous_response_error(&error);
                let mut partial = shared::response_from_stream_state(
                    state.clone(),
                    Some(request_body.clone()),
                    self.websocket_http_summary(&diagnostics),
                );
                partial.terminal_reason = LlmTerminalReason::Unknown;
                partial.generation_disposition = Some(Self::generation_disposition(req, full_body));
                return Err(CodexWebSocketAttemptError {
                    error: error
                        .with_request_body(request_body.clone())
                        .with_partial_response(partial),
                    events_seen,
                    output_started,
                    stale_previous_response,
                });
            }
            emit_stream_progress(
                stream_events.as_ref(),
                state.take_text_deltas(),
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
                for piece in state.take_reasoning_deltas() {
                    if expose_thinking {
                        tx.send(LlmStreamEvent::ReasoningDelta(piece));
                    }
                }
                for part in emitted_parts {
                    if matches!(part, lash_core::llm::types::LlmOutputPart::Reasoning { .. })
                        && !expose_thinking
                    {
                        continue;
                    }
                    tx.send(LlmStreamEvent::Part(part));
                }
            } else {
                state.take_reasoning_deltas();
            }
            if state.terminal_event_seen {
                break;
            }
        }

        let terminal_response_seen = state.terminal_event_seen;
        if !terminal_response_seen
            && stream_termination == StreamTermination::RequireTerminalEvidence
        {
            let output_started = Self::response_state_started_output(&state);
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                Some(request_body.clone()),
                self.websocket_http_summary(&diagnostics),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = Some(Self::generation_disposition(req, full_body));
            return Err(CodexWebSocketAttemptError {
                error: LlmTransportError::new("Codex WebSocket ended before response.completed")
                    .with_request_body(request_body)
                    .with_kind(ProviderFailureKind::Stream)
                    .retryable(true)
                    .with_code("websocket_closed_before_completed")
                    .with_partial_response(partial),
                events_seen,
                output_started,
                stale_previous_response: false,
            });
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
        response.generation_disposition = Some(Self::generation_disposition(req, full_body));
        attempt.finish(continuation);
        Ok(response)
    }

    fn websocket_http_summary(&self, diagnostics: &CodexWebsocketAttemptDiagnostics) -> String {
        format!(
            "WS {} transport={:?} reused={} cached={} cache_miss={} retry_after_stale={} retry_after_dead_reused={} input_items={}/{} previous_response_id={} request_bytes={}",
            self.websocket_url,
            diagnostics.configured_transport,
            diagnostics.reused_connection,
            diagnostics.cached_request,
            diagnostics.cache_miss_reason.unwrap_or("<none>"),
            diagnostics.retry_after_stale_previous_response,
            diagnostics.retry_after_dead_reused_connection,
            diagnostics.sent_input_items,
            diagnostics.full_input_items,
            diagnostics
                .previous_response_id
                .as_deref()
                .unwrap_or("<none>"),
            diagnostics.request_bytes
        )
    }

    fn emit_websocket_attempt_trace(
        &self,
        provider_trace: Option<&lash_core::llm::types::LlmProviderTraceSender>,
        diagnostics: &CodexWebsocketAttemptDiagnostics,
    ) {
        let raw = json!({
            "type": "lash.codex.websocket_request",
            "transport": format!("{:?}", diagnostics.configured_transport),
            "reused_connection": diagnostics.reused_connection,
            "cached_request": diagnostics.cached_request,
            "continuation_available": diagnostics.continuation_available,
            "cache_miss_reason": diagnostics.cache_miss_reason,
            "retry_after_stale_previous_response": diagnostics.retry_after_stale_previous_response,
            "retry_after_dead_reused_connection": diagnostics.retry_after_dead_reused_connection,
            "previous_response_id": diagnostics.previous_response_id,
            "full_input_items": diagnostics.full_input_items,
            "sent_input_items": diagnostics.sent_input_items,
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
    error.code = Some("provider_replay_origin_conflict".to_string());
    error.retryable = false;
    error
}

fn stamp_codex_partial_or_attach_conflict(
    mut error: LlmTransportError,
    route: &ProviderRouteIdentity,
) -> Result<LlmTransportError, LlmTransportError> {
    if let Some(partial) = error.partial_response.as_deref_mut()
        && let Err(conflict) = partial.stamp_replay_origin(route)
    {
        return Err(codex_replay_origin_conflict(conflict, Some(error)));
    }
    Ok(error)
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
        let credential = self.credentials.snapshot();
        let mut map = serde_json::Map::new();
        map.insert(
            "access_token".to_string(),
            serde_json::Value::String(credential.access_token),
        );
        map.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(credential.refresh_token),
        );
        map.insert(
            "expires_at".to_string(),
            serde_json::Value::Number(credential.expires_at.into()),
        );
        if let Some(account_id) = &credential.account_id {
            map.insert(
                "account_id".to_string(),
                serde_json::Value::String(account_id.clone()),
            );
        } else {
            map.insert("account_id".to_string(), serde_json::Value::Null);
        }
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

    async fn complete(&mut self, mut req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        let route = self.route_identity(&req.model);
        route.validate_endpoint().map_err(|error| {
            LlmTransportError::new(error.to_string())
                .with_kind(ProviderFailureKind::Validation)
                .with_code("invalid_provider_endpoint")
        })?;
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
        if self.attempt_credential.is_none() {
            let manager = Arc::clone(&self.credentials);
            let provider = self.clone();
            let minting_route = route.clone();
            return manager
                .execute(move |lease| {
                    let mut provider = provider.clone();
                    let req = req.clone();
                    let minting_route = minting_route.clone();
                    provider.attempt_credential = Some(lease);
                    async move {
                        match Box::pin(provider.complete(req)).await {
                            Ok(mut response) => {
                                response.stamp_replay_origin(&minting_route).map_err(
                                    |conflict| {
                                        CredentialCallError::Failed(codex_replay_origin_conflict(
                                            conflict, None,
                                        ))
                                    },
                                )?;
                                Ok(response)
                            }
                            Err(error) if error.status == Some(401) => {
                                let error =
                                    stamp_codex_partial_or_attach_conflict(error, &minting_route)
                                        .map_err(CredentialCallError::Failed)?;
                                Err(CredentialCallError::PreOutputAuth(error))
                            }
                            Err(error) => {
                                let error =
                                    stamp_codex_partial_or_attach_conflict(error, &minting_route)
                                        .map_err(CredentialCallError::Failed)?;
                                Err(CredentialCallError::Failed(error))
                            }
                        }
                    }
                })
                .await
                .map_err(|error| match error {
                    CredentialExecuteError::Credential(error) => credential_transport_error(error),
                    CredentialExecuteError::Call(error) => error,
                });
        }
        let credential_lease = self
            .attempt_credential
            .take()
            .expect("credential attempt is configured");
        let credential = &credential_lease.value;
        let stream_termination = req
            .model_capability
            .stream_termination
            .unwrap_or(StreamTermination::RequireTerminalEvidence);
        if !matches!(self.transport, CodexTransport::Sse) {
            let fallback_reason = matches!(self.transport, CodexTransport::Auto)
                .then(|| self.websocket_fallback_reason(&req))
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
                match self
                    .complete_websocket(req.clone(), credential, credential_lease.generation)
                    .await
                {
                    Ok(response) => {
                        self.clear_websocket_fallback(&req);
                        return Ok(response);
                    }
                    Err(err)
                        if matches!(self.transport, CodexTransport::Auto) && !err.events_seen =>
                    {
                        self.record_websocket_fallback(&req, &err.error);
                        tracing::debug!(
                            target: "lash_core::llm::codex_oauth",
                            error = %err.error.message,
                            "Codex WebSocket failed before stream start; falling back to SSE"
                        );
                    }
                    Err(err) => {
                        self.clear_continuation(&req);
                        return Err(err.error.with_output_started(err.output_started));
                    }
                }
            }
        }
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let timeouts = self.options.llm_timeouts();

        let body = self.build_request_body(&req, stream_events.is_some())?;
        let generation_disposition = Some(Self::generation_disposition(&req, &body));

        let request_body = serde_json::to_string(&body).ok();
        let body_bytes = serde_json::to_vec(&body).map_err(|e| {
            LlmTransportError::new(format!("Failed to serialize Codex request: {e}"))
        })?;
        emit_provider_request_trace(provider_trace.as_ref(), "codex", "responses", &body_bytes);
        let access_token = credential.access_token.clone();
        let account_id = credential.account_id.clone();
        let mut headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "text/event-stream".to_string()),
            (
                "OpenAI-Beta".to_string(),
                "responses=experimental".to_string(),
            ),
            ("originator".to_string(), Self::CODEX_ORIGINATOR.to_string()),
            ("User-Agent".to_string(), Self::codex_user_agent()),
            ("session-id".to_string(), req.scope.session_id.clone()),
            (
                "x-client-request-id".to_string(),
                req.scope.request_id.clone(),
            ),
        ];
        if let Some(id) = account_id.as_deref() {
            headers.push(("ChatGPT-Account-ID".to_string(), id.to_string()));
        }
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
                    timeouts.chunk_timeout,
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
                timeouts.request_timeout,
                "Codex response body timed out",
            )
            .await
            .unwrap_or_default();
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
        let mut response_metadata =
            ResponseMetadataCapture::from_response(&self.options, &response_headers);
        if let Some(tx) = &stream_events {
            tx.send(LlmStreamEvent::Evidence(LlmStreamEvidence {
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
                timeouts.request_timeout,
                "Codex response body timed out",
            )
            .await
            .map_err(|err| Self::non_sse_body_read_error(status, content_type.as_deref(), err))?;
            response_metadata.capture_body_text(&text);
            emit_provider_trace(provider_trace.as_ref(), "codex", &text);
            if Self::looks_like_sse_payload(&text) {
                let mut state = shared::ResponsesStreamState {
                    execution_evidence: provider_request_id.clone().map(|provider_request_id| {
                        ExecutionEvidence {
                            provider_request_id: Some(provider_request_id),
                            ..Default::default()
                        }
                    }),
                    ..Default::default()
                };
                shared::parse_sse_payload(PROVIDER, &text, &mut state)?;
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
                    for part in &response.parts {
                        if let lash_core::llm::types::LlmOutputPart::Text { text, .. } = part
                            && !text.is_empty()
                        {
                            tx.send(LlmStreamEvent::Delta(text.clone()));
                        }
                    }
                    for part in &response.parts {
                        match part {
                            lash_core::llm::types::LlmOutputPart::ToolCall { .. } => {
                                tx.send(LlmStreamEvent::Part(part.clone()));
                            }
                            lash_core::llm::types::LlmOutputPart::Reasoning { text, .. }
                                if !text.is_empty() && self.options.expose_thinking =>
                            {
                                tx.send(LlmStreamEvent::ReasoningDelta(text.clone()));
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
                if !content.is_empty() {
                    tx.send(LlmStreamEvent::Delta(content.clone()));
                }
            }
            let terminal_reason = openai_terminal_reason_from_response_value(&value, &parts);
            return Ok(LlmResponse {
                full_text: content,
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
            execution_evidence: provider_request_id.map(|provider_request_id| ExecutionEvidence {
                provider_request_id: Some(provider_request_id),
                ..Default::default()
            }),
            ..Default::default()
        };
        let expose_thinking = self.options.expose_thinking;
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
                    state.take_text_deltas(),
                    &state.usage,
                    &prev_usage,
                );
                if let Some(tx) = &stream_events {
                    for piece in state.take_reasoning_deltas() {
                        if expose_thinking {
                            tx.send(LlmStreamEvent::ReasoningDelta(piece));
                        }
                    }
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

        if let Err(error) = stream_result {
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

        if stream_termination == StreamTermination::RequireTerminalEvidence
            && !state.terminal_event_seen
        {
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
            .with_code("stream_ended_before_terminal_response")
            .retryable(true)
            .with_output_started(output_started)
            .with_partial_response(partial));
        }

        if state.final_response.is_none()
            && state.parts.is_empty()
            && state.pending_text_deltas.is_empty()
        {
            return Err(LlmTransportError::new(format!(
                "Codex stream ended without SSE events (HTTP {}{})",
                status,
                content_type
                    .as_deref()
                    .map(|ct| format!(", content-type {ct}"))
                    .unwrap_or_else(|| ", missing content-type".to_string())
            ))
            .retryable(true)
            .with_code("empty_stream"));
        }

        let mut response = shared::response_from_stream_state(
            state,
            request_body,
            format!("HTTP POST {} (stream)", self.responses_url),
        );
        response.generation_disposition = generation_disposition;
        response.response_metadata = response_metadata.into_metadata();
        Ok(response)
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
