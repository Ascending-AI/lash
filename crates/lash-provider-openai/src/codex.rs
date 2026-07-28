//! OpenAI Codex OAuth provider (ChatGPT Plus/Pro/Team via device-code flow).

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::common::{DEFAULT_HTTP_TRANSPORT, DEFAULT_MAX_OUTPUT_TOKENS, reasoning_intent};
use crate::reasoning::ReasoningWireIntent;
use crate::responses_shared as shared;
use lash_core::llm::transport::{LlmTransportError, ProviderFailure, ProviderFailureKind};
use lash_core::llm::types::{
    GenerationDisposition, GenerationOptionDisposition, GenerationOptions, LlmOutputSpec,
    LlmRequest, LlmResponse, LlmStreamEvent, LlmTerminalReason, LlmUsage,
};
use lash_core::provider::{
    CacheRetention, DefaultProviderFailureClassifier, Provider, ProviderComponents,
    ProviderFactory, ProviderFailureClassifier, ProviderOptions, ProviderReliability,
    StreamTermination, resolve_generation_policy,
};
use lash_core::{ProviderSchemaCapabilities, SchemaPurpose};
use lash_llm_transport::streaming::{drive_sse_response, emit_stream_progress};
use lash_llm_transport::timeouts::response_start_timeout;
use lash_llm_transport::util::{emit_provider_request_trace, emit_provider_trace};
use lash_llm_transport::{
    LlmHttpMethod, LlmHttpRequest, LlmHttpTransport, first_header_value, header_contains,
    http_error_envelope, openai_terminal_reason_from_response_value,
    openai_usage_from_response_value, read_http_body_text,
};
use lash_provider_auth::{
    Credential, CredentialCallError, CredentialError, CredentialErrorKind, CredentialExecuteError,
    CredentialManager, CredentialRefresher, Lease, RefreshCause, classify_oauth_refresh_error,
};

pub mod oauth;
#[cfg(any(test, feature = "testing"))]
pub mod ws_testing;

/// Provider name used in shared-machinery error messages and trace events.
const PROVIDER: &str = "Codex";

const SESSION_WEBSOCKET_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const SESSION_WEBSOCKET_FALLBACK_TTL: Duration = Duration::from_secs(60);
const MAX_SESSION_WEBSOCKET_CACHE_ENTRIES: usize = 32;
/// Per-socket bound on the closing handshake during shutdown drain. A half-dead
/// peer that never returns its Close frame must not stall the remaining cached
/// sockets, so each close is best-effort and abandoned after this elapses.
const SESSION_WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Transport-selection knob for Codex. Production always runs `Auto` (try the
/// WebSocket transport, fall back to SSE). The non-`Auto` variants force a
/// specific path; hosts that must pin a path use
/// [`CodexProvider::force_sse_transport`] (e.g. the deterministic-simulation
/// harness driving Provider Wire Scripts through an injected transport) or
/// [`CodexProvider::force_websocket_transport`] (e.g. the runtime-level
/// WebSocket test) rather than naming these variants; `WebsocketCached`
/// remains a crate-internal test seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexTransport {
    #[default]
    Auto,
    Sse,
    Websocket,
    WebsocketCached,
}

#[derive(Clone, Debug, Default)]
struct CodexContinuation {
    previous_response_id: String,
    request_input: Vec<Value>,
    response_items: Vec<Value>,
    body_fingerprint: String,
}

type CodexWsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Default)]
struct CodexWebsocketSessionCache {
    inner: Arc<Mutex<CodexWebsocketSessions>>,
}

impl std::fmt::Debug for CodexWebsocketSessionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (sessions_len, fallback_sessions_len) = self
            .inner
            .lock()
            .map(|sessions| (sessions.by_scope.len(), sessions.fallback_by_scope.len()))
            .unwrap_or_default();
        f.debug_struct("CodexWebsocketSessionCache")
            .field("sessions", &sessions_len)
            .field("fallback_sessions", &fallback_sessions_len)
            .finish()
    }
}

#[derive(Default)]
struct CodexWebsocketSessions {
    by_scope: HashMap<String, CodexWebsocketSessionEntry>,
    fallback_by_scope: HashMap<String, CodexWebsocketFallbackState>,
}

struct CodexWebsocketFallbackState {
    until: Instant,
    reason: String,
}

struct CodexWebsocketSessionEntry {
    connection: Option<CodexWsStream>,
    continuation: Option<CodexContinuation>,
    busy: bool,
    last_used: Instant,
    credential_generation: u64,
}

impl CodexWebsocketSessionEntry {
    fn reserved(credential_generation: u64) -> Self {
        Self {
            connection: None,
            continuation: None,
            busy: true,
            last_used: Instant::now(),
            credential_generation,
        }
    }
}

struct CodexWebsocketLease {
    websocket: CodexWsStream,
    scope_key: Option<String>,
    reusable: bool,
    reused: bool,
    continuation: Option<CodexContinuation>,
    credential_generation: u64,
}

#[derive(Clone)]
struct CodexCredential {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    account_id: Option<String>,
}

impl std::fmt::Debug for CodexCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCredential")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl std::fmt::Display for CodexCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CodexCredential([REDACTED])")
    }
}

impl Credential for CodexCredential {
    fn expires_at(&self) -> Option<SystemTime> {
        (self.expires_at != 0)
            .then(|| UNIX_EPOCH.checked_add(Duration::from_secs(self.expires_at)))
            .flatten()
    }
}

#[derive(Debug)]
struct CodexCredentialRefresher;

#[async_trait]
impl CredentialRefresher<CodexCredential> for CodexCredentialRefresher {
    async fn refresh(
        &self,
        current: &CodexCredential,
        _cause: RefreshCause,
    ) -> Result<CodexCredential, CredentialError> {
        let tokens = oauth::refresh_tokens(&current.refresh_token)
            .await
            .map_err(classify_oauth_refresh_error)?;
        Ok(CodexCredential {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
            account_id: tokens.account_id.or_else(|| current.account_id.clone()),
        })
    }
}

fn credential_transport_error(error: CredentialError) -> LlmTransportError {
    let code = match error.kind {
        CredentialErrorKind::InvalidGrant => "credential_invalid_grant",
        CredentialErrorKind::Transient => "credential_refresh_transient",
        CredentialErrorKind::Other => "credential_refresh_failed",
    };
    LlmTransportError::new(error.to_string())
        .with_kind(ProviderFailureKind::Auth)
        .with_code(code)
        .retryable(error.retryable)
}

#[derive(Clone, Debug)]
struct CodexWebsocketRequestPlan {
    body: Value,
    cached: bool,
    continuation_available: bool,
    cache_miss_reason: Option<&'static str>,
    previous_response_id: Option<String>,
    full_input_items: usize,
    sent_input_items: usize,
}

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

struct CodexWebSocketAttemptError {
    error: LlmTransportError,
    events_seen: bool,
    output_started: bool,
    stale_previous_response: bool,
}

/// One-shot WebSocket retries already consumed by the current send loop.
#[derive(Clone, Copy, Default)]
struct CodexWebsocketRetryState {
    after_stale_previous_response: bool,
    after_dead_reused_connection: bool,
}

/// OpenAI Codex OAuth provider (ChatGPT Plus/Pro/Team via device-code flow).
///
/// Codex speaks the OpenAI Responses streaming protocol, so the request/stream
/// machinery is shared verbatim from [`crate::responses_shared`].
/// This module owns only the Codex-specific surface: the
/// `chatgpt.com/backend-api/codex/responses` endpoint, the `codex_cli_rs`
/// originator/User-Agent headers, the system→`instructions` request shape with
/// tool-result image folding, and Codex error/quota classification.
#[derive(Clone, Debug)]
pub struct CodexProvider {
    credentials: Arc<CredentialManager<CodexCredential>>,
    attempt_credential: Option<Lease<CodexCredential>>,
    pub options: ProviderOptions,
    pub(crate) transport: CodexTransport,
    websocket_sessions: CodexWebsocketSessionCache,
    responses_url: String,
    websocket_url: String,
    http_transport: Arc<dyn LlmHttpTransport>,
}

impl CodexProvider {
    const CODEX_ORIGINATOR: &'static str = "codex_cli_rs";
    const CODEX_RESPONSES_URL: &'static str = "https://chatgpt.com/backend-api/codex/responses";
    const CODEX_RESPONSES_WS_URL: &'static str = "wss://chatgpt.com/backend-api/codex/responses";
    const CODEX_RESPONSES_WS_BETA: &'static str = "responses_websockets=2026-02-06";

    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: u64,
    ) -> Self {
        let credential = CodexCredential {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at,
            account_id: None,
        };
        Self {
            credentials: Arc::new(CredentialManager::new(
                credential,
                Arc::new(CodexCredentialRefresher),
            )),
            attempt_credential: None,
            options: ProviderOptions {
                reliability: ProviderReliability::codex(),
                ..ProviderOptions::default()
            },
            transport: CodexTransport::Auto,
            websocket_sessions: CodexWebsocketSessionCache::default(),
            responses_url: Self::CODEX_RESPONSES_URL.to_string(),
            websocket_url: Self::CODEX_RESPONSES_WS_URL.to_string(),
            http_transport: DEFAULT_HTTP_TRANSPORT.clone(),
        }
    }

    pub fn with_account_id(mut self, account_id: Option<String>) -> Self {
        let mut credential = self.credentials.snapshot();
        credential.account_id = account_id;
        self.credentials = Arc::new(CredentialManager::new(
            credential,
            Arc::new(CodexCredentialRefresher),
        ));
        self
    }

    pub fn with_options(mut self, options: ProviderOptions) -> Self {
        self.options = options;
        self
    }

    #[cfg(test)]
    fn with_transport(mut self, transport: CodexTransport) -> Self {
        self.transport = transport;
        self
    }

    /// Pin Codex to the HTTP/SSE transport, skipping the WebSocket path. This
    /// lets a host (notably the deterministic-simulation harness) drive Codex's
    /// HTTP/SSE path through an injected [`LlmHttpTransport`] without exposing
    /// the internal [`CodexTransport`] variants.
    pub fn force_sse_transport(mut self) -> Self {
        self.transport = CodexTransport::Sse;
        self
    }

    /// Pin Codex to the WebSocket transport, skipping the SSE fallback. The
    /// WebSocket counterpart of [`CodexProvider::force_sse_transport`]: a host
    /// (notably the runtime-level WebSocket test, which points the provider at
    /// a local scripted server via [`CodexProvider::with_endpoint_urls`]) uses
    /// it to exercise the WebSocket path deterministically instead of relying
    /// on `Auto`'s try-then-fall-back behavior.
    pub fn force_websocket_transport(mut self) -> Self {
        self.transport = CodexTransport::Websocket;
        self
    }

    /// Override the Codex Responses HTTP and WebSocket endpoint URLs. This is
    /// a constructor-level injection seam in the same spirit as
    /// [`CodexProvider::with_http_transport`]: production always uses the
    /// built-in `chatgpt.com` endpoints, and the override is never serialized
    /// into provider config ([`CodexProviderFactory`] always rebuilds with the
    /// production URLs), so tests can point a provider instance at local
    /// scripted servers without adding a user-facing behavior surface.
    pub fn with_endpoint_urls(
        mut self,
        responses_url: impl Into<String>,
        websocket_url: impl Into<String>,
    ) -> Self {
        self.responses_url = responses_url.into();
        self.websocket_url = websocket_url.into();
        self
    }

    /// Inject the HTTP/SSE transport seam. Production uses the shared reqwest
    /// transport; the deterministic-simulation harness and tests inject a
    /// scripted [`LlmHttpTransport`] to drive Provider Wire Scripts.
    pub fn with_http_transport(mut self, transport: Arc<dyn LlmHttpTransport>) -> Self {
        self.http_transport = transport;
        self
    }

    /// Translate a Codex error body into a user-friendly one-line message.
    /// Mirrors pi-mono's `openai-codex-responses.ts:880-904`: for a
    /// `usage_limit_reached`/`rate_limit_exceeded` code (or any 429),
    /// parse the `plan_type` and `resets_at` epoch and render
    /// `"You have hit your ChatGPT usage limit (plus plan). Try again in
    /// ~12 min."`. Returns `None` when the body isn't parseable or the
    /// status doesn't match the pattern, so the caller falls back to the
    /// raw status.
    fn codex_error_summary(status: u16, body_text: &str) -> Option<String> {
        let parsed: Value = serde_json::from_str(body_text).ok()?;
        if let Some(detail) = parsed.get("detail").and_then(|v| v.as_str()) {
            return Some(format!("Codex request failed with {status}: {detail}"));
        }
        let err = parsed.get("error")?;
        let code = err
            .get("code")
            .and_then(|v| v.as_str())
            .or_else(|| err.get("type").and_then(|v| v.as_str()))
            .unwrap_or("");
        let code_matches = {
            let lc = code.to_ascii_lowercase();
            lc.contains("usage_limit_reached")
                || lc.contains("usage_not_included")
                || lc.contains("rate_limit_exceeded")
        };
        if !code_matches && status != 429 {
            // Prefer the raw `error.message` if the server gave us one —
            // useful for refusals, invalid-request errors, etc.
            let msg = err.get("message").and_then(|v| v.as_str())?;
            return Some(format!("Codex request failed with {status}: {msg}"));
        }

        let plan = err
            .get("plan_type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|p| format!(" ({} plan)", p.to_ascii_lowercase()))
            .unwrap_or_default();
        let resets_at_secs = err.get("resets_at").and_then(|v| v.as_i64());
        let mins = resets_at_secs.and_then(|ts| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            let delta_secs = ts - now;
            if delta_secs <= 0 {
                Some(0)
            } else {
                Some(((delta_secs + 30) / 60).max(0))
            }
        });
        let when = match mins {
            Some(m) => format!(" Try again in ~{m} min."),
            None => String::new(),
        };
        Some(format!(
            "You have hit your ChatGPT usage limit{plan}.{when}"
        ))
    }

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

    fn build_tools(req: &LlmRequest) -> Result<Vec<Value>, LlmTransportError> {
        shared::build_tools(PROVIDER, req)
    }

    fn codex_user_agent() -> String {
        format!(
            "{}/{} ({}; {}) lash",
            Self::CODEX_ORIGINATOR,
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }

    /// Which of the caller's generation options a Codex request carries: none
    /// of them. The Responses dialect Codex speaks has no seed field, and this
    /// adapter sends neither a temperature nor a token cap, for the same
    /// reason it leaves the rest of the sampling surface to the endpoint.
    fn generation_disposition(generation: &GenerationOptions) -> GenerationDisposition {
        GenerationDisposition {
            output_token_cap: GenerationOptionDisposition::unsupported(
                generation.output_token_cap.is_some(),
            ),
            temperature: GenerationOptionDisposition::unsupported(generation.temperature.is_some()),
            seed: GenerationOptionDisposition::unsupported(generation.seed.is_some()),
        }
    }

    pub(crate) fn build_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        shared::validate_responses_attachments(req, "OpenAI Codex")?;
        let tools = Self::build_tools(req)?;
        let (instructions, input) =
            shared::build_responses_input(req, shared::ResponsesInputOptions::CODEX);
        let requested_reasoning = reasoning_intent(req);
        let policy = resolve_generation_policy(
            &req.generation,
            &self.options,
            DEFAULT_MAX_OUTPUT_TOKENS,
            requested_reasoning,
        );
        let mut body = json!({
            "model": req.model,
            "instructions": instructions,
            "input": input,
            "tools": tools,
            "parallel_tool_calls": !req.tools.is_empty(),
            "stream": stream,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "text": {
                "verbosity": "medium",
            },
        });
        // `tool_choice` is only meaningful when the request advertises tools.
        // In RLM mode we intentionally send `tools: []` because tools are
        // documented in the prompt body and invoked via `lashlang`, not the
        // native tool-call envelope. Sending `tool_choice: "none"` on top of
        // an empty tool list adds a second "definitely don't call any
        // function" signal that reasoning-capable Codex models take literally,
        // causing them to refuse to emit `call` expressions in lashlang.
        if !req.tools.is_empty() {
            body["tool_choice"] = json!(shared::tool_choice_value(&req.tool_choice));
        }
        if let Some(config) = policy.thinking {
            let mut reasoning = match config {
                ReasoningWireIntent::Effort(effort) => json!({ "effort": effort }),
                ReasoningWireIntent::Budget(max_tokens) => json!({ "max_tokens": max_tokens }),
                ReasoningWireIntent::ToggleFalse => json!({ "enabled": false }),
            };
            if policy.expose_thinking {
                reasoning["summary"] = json!("auto");
            }
            body["reasoning"] = reasoning;
        }
        if policy.cache_retention != CacheRetention::None {
            body["prompt_cache_key"] = json!(req.continuation_key());
        }
        if let Some(output_spec) = &req.output_spec {
            body["text"]["format"] = match output_spec {
                LlmOutputSpec::JsonObject => json!({ "type": "json_object" }),
                LlmOutputSpec::JsonSchema(schema) => {
                    let capabilities = ProviderSchemaCapabilities::openai(false);
                    let projected = shared::projected_schema(
                        PROVIDER,
                        &schema.schema,
                        &capabilities,
                        SchemaPurpose::StructuredOutput,
                    )?;
                    json!({
                        "type": "json_schema",
                        "name": schema.name,
                        "schema": projected,
                        "strict": schema.strict,
                    })
                }
            };
        }
        Ok(body)
    }

    fn body_input(body: &Value) -> Vec<Value> {
        body.get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn body_fingerprint(body: &Value) -> String {
        let mut comparable = body.clone();
        if let Some(obj) = comparable.as_object_mut() {
            obj.remove("input");
            obj.remove("previous_response_id");
        }
        comparable.to_string()
    }

    fn response_output_items(final_response: &Value) -> Vec<Value> {
        final_response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn cached_websocket_body(continuation: &CodexContinuation, full_body: &Value) -> Option<Value> {
        Self::cached_websocket_body_result(continuation, full_body).ok()
    }

    fn cached_websocket_body_result(
        continuation: &CodexContinuation,
        full_body: &Value,
    ) -> Result<Value, &'static str> {
        let current_fingerprint = Self::body_fingerprint(full_body);
        let current_input = Self::body_input(full_body);
        if continuation.body_fingerprint != current_fingerprint {
            return Err("body_fingerprint_mismatch");
        }
        let mut baseline = continuation.request_input.clone();
        baseline.extend(continuation.response_items.clone());
        if current_input.len() < baseline.len()
            || !current_input
                .iter()
                .take(baseline.len())
                .eq(baseline.iter())
        {
            return Err("input_prefix_mismatch");
        }

        let mut body = full_body.clone();
        body["previous_response_id"] = json!(continuation.previous_response_id);
        body["input"] = Value::Array(current_input[baseline.len()..].to_vec());
        Ok(body)
    }

    fn websocket_continuation_enabled(&self) -> bool {
        matches!(
            self.transport,
            CodexTransport::Auto | CodexTransport::WebsocketCached
        )
    }

    fn websocket_request_plan(
        &self,
        full_body: &Value,
        continuation: Option<&CodexContinuation>,
        allow_cached_context: bool,
    ) -> CodexWebsocketRequestPlan {
        let full_input_items = Self::body_input(full_body).len();
        let continuation_available = continuation.is_some();
        let (body, cached, cache_miss_reason) = match (allow_cached_context, continuation) {
            (false, _) => (full_body.clone(), false, Some("disabled")),
            (true, None) => (full_body.clone(), false, Some("missing_continuation")),
            (true, Some(cached)) => match Self::cached_websocket_body_result(cached, full_body) {
                Ok(body) => (body, true, None),
                Err(reason) => (full_body.clone(), false, Some(reason)),
            },
        };
        let previous_response_id = body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let sent_input_items = Self::body_input(&body).len();
        CodexWebsocketRequestPlan {
            body,
            cached,
            continuation_available,
            cache_miss_reason,
            previous_response_id,
            full_input_items,
            sent_input_items,
        }
    }

    fn websocket_create_request(body: &Value) -> Value {
        let mut request = body
            .as_object()
            .cloned()
            .unwrap_or_else(serde_json::Map::new);
        request.insert("type".to_string(), json!("response.create"));
        Value::Object(request)
    }

    fn continuation_from_response(
        full_body: &Value,
        final_response: &Value,
    ) -> Option<CodexContinuation> {
        let completed = final_response
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "completed");
        let response_id = final_response.get("id").and_then(Value::as_str)?;
        if !completed || response_id.is_empty() {
            return None;
        }
        Some(CodexContinuation {
            previous_response_id: response_id.to_string(),
            request_input: Self::body_input(full_body),
            response_items: Self::response_output_items(final_response),
            body_fingerprint: Self::body_fingerprint(full_body),
        })
    }

    fn clear_continuation(&self, req: &LlmRequest) {
        let scope_key = req.continuation_key();
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = sessions.by_scope.get_mut(&scope_key) {
            entry.continuation = None;
        }
    }

    fn remove_websocket_scope(&self, scope_key: &str) {
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.by_scope.remove(scope_key);
    }

    fn prune_idle_websocket_sessions(sessions: &mut CodexWebsocketSessions) {
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(sessions, now);
        // Dropping a cached WebSocketStream closes the socket. This prune path is
        // deliberately synchronous because the cache lock is provider-local.
        sessions.by_scope.retain(|_, entry| {
            entry.busy || now.duration_since(entry.last_used) <= SESSION_WEBSOCKET_CACHE_TTL
        });
    }

    fn prune_expired_websocket_fallbacks(sessions: &mut CodexWebsocketSessions, now: Instant) {
        sessions
            .fallback_by_scope
            .retain(|_, fallback| fallback.until > now);
    }

    fn enforce_websocket_session_cache_cap(sessions: &mut CodexWebsocketSessions) {
        let excess = sessions
            .by_scope
            .len()
            .saturating_sub(MAX_SESSION_WEBSOCKET_CACHE_ENTRIES);
        if excess == 0 {
            return;
        }

        let mut removable = sessions
            .by_scope
            .iter()
            .filter(|(_, entry)| !entry.busy)
            .map(|(scope_key, entry)| (scope_key.clone(), entry.last_used))
            .collect::<Vec<_>>();
        removable.sort_by_key(|(_, last_used)| *last_used);
        for (scope_key, _) in removable.into_iter().take(excess) {
            sessions.by_scope.remove(&scope_key);
        }
    }

    fn evict_websocket_sessions_for_generation(
        sessions: &mut CodexWebsocketSessions,
        credential_generation: u64,
    ) {
        sessions
            .by_scope
            .retain(|_, entry| entry.credential_generation == credential_generation);
    }

    /// Drain the WebSocket session cache, sending a proper Close frame on every
    /// idle cached connection before dropping it.
    ///
    /// This is the shutdown counterpart to the synchronous idle prune: the prune
    /// path drops streams (a TCP-level close), whereas a host-driven shutdown
    /// wants the WebSocket closing handshake. Busy entries are leased out to an
    /// in-flight `complete` call — their stream is not held in the cache — so
    /// this closes only idle, reusable sessions; the lease closes or re-caches
    /// its own connection on release. The cache lock is provider-local and
    /// non-async, so connections are taken out under the lock and closed after
    /// it is released.
    async fn close_websocket_sessions(&self) {
        let connections: Vec<CodexWsStream> = {
            let mut sessions = self
                .websocket_sessions
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let drained = sessions
                .by_scope
                .drain()
                .filter_map(|(_, entry)| entry.connection)
                .collect();
            sessions.fallback_by_scope.clear();
            drained
        };
        for mut websocket in connections {
            // Best-effort: a peer that already vanished cannot receive the frame,
            // and shutdown must not fail because one socket is already gone. Bound
            // each close so a half-dead peer that never returns its Close frame
            // cannot stall the drain of the sockets still queued behind it.
            let _ =
                tokio::time::timeout(SESSION_WEBSOCKET_CLOSE_TIMEOUT, websocket.close(None)).await;
        }
    }

    fn websocket_fallback_reason(&self, req: &LlmRequest) -> Option<String> {
        let scope_key = req.continuation_key();
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::prune_expired_websocket_fallbacks(&mut sessions, Instant::now());
        sessions
            .fallback_by_scope
            .get(&scope_key)
            .map(|fallback| fallback.reason.clone())
    }

    fn record_websocket_fallback(&self, req: &LlmRequest, error: &LlmTransportError) {
        let scope_key = req.continuation_key();
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        Self::prune_expired_websocket_fallbacks(&mut sessions, now);
        let reason = error
            .code
            .as_deref()
            .map(|code| format!("{code}: {}", error.message))
            .unwrap_or_else(|| error.message.clone());
        sessions.fallback_by_scope.insert(
            scope_key,
            CodexWebsocketFallbackState {
                until: now + SESSION_WEBSOCKET_FALLBACK_TTL,
                reason,
            },
        );
    }

    fn clear_websocket_fallback(&self, req: &LlmRequest) {
        let scope_key = req.continuation_key();
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.fallback_by_scope.remove(&scope_key);
    }

    async fn connect_websocket(
        &self,
        req: &LlmRequest,
        connect_timeout: Duration,
        credential: &CodexCredential,
    ) -> Result<CodexWsStream, CodexWebSocketAttemptError> {
        let mut ws_request =
            self.websocket_url
                .as_str()
                .into_client_request()
                .map_err(|error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Failed to build Codex WebSocket request: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                })?;
        let headers = ws_request.headers_mut();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", credential.access_token)).map_err(
                |error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket authorization header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                },
            )?,
        );
        headers.insert(
            "OpenAI-Beta",
            HeaderValue::from_static(Self::CODEX_RESPONSES_WS_BETA),
        );
        headers.insert(
            "originator",
            HeaderValue::from_static(Self::CODEX_ORIGINATOR),
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&Self::codex_user_agent()).map_err(|error| {
                CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket user-agent header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                }
            })?,
        );
        let session_value = HeaderValue::from_str(&req.scope.session_id).map_err(|error| {
            CodexWebSocketAttemptError {
                error: LlmTransportError::new(format!(
                    "Invalid Codex WebSocket session header: {error}"
                )),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })?;
        let request_value = HeaderValue::from_str(&req.scope.request_id).map_err(|error| {
            CodexWebSocketAttemptError {
                error: LlmTransportError::new(format!(
                    "Invalid Codex WebSocket request header: {error}"
                )),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })?;
        headers.insert("session-id", session_value);
        headers.insert("x-client-request-id", request_value);
        if let Some(account_id) = credential.account_id.as_deref() {
            headers.insert(
                "ChatGPT-Account-ID",
                HeaderValue::from_str(account_id).map_err(|error| CodexWebSocketAttemptError {
                    error: LlmTransportError::new(format!(
                        "Invalid Codex WebSocket account header: {error}"
                    )),
                    events_seen: false,
                    output_started: false,
                    stale_previous_response: false,
                })?,
            );
        }

        let connect = tokio::time::timeout(connect_timeout, connect_async(ws_request))
            .await
            .map_err(|_| CodexWebSocketAttemptError {
                error: LlmTransportError::new("Codex WebSocket connect timed out")
                    .with_kind(ProviderFailureKind::Timeout)
                    .retryable(true)
                    .with_code("websocket_connect_timeout"),
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            })?;
        connect.map(|(websocket, _)| websocket).map_err(|error| {
            let status = match &error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    Some(response.status().as_u16())
                }
                _ => None,
            };
            let mut transport_error =
                LlmTransportError::new(format!("Codex WebSocket connect failed: {error}"))
                    .retryable(true)
                    .with_code("websocket_connect");
            if let Some(status) = status {
                transport_error = transport_error.with_status(status);
            }
            CodexWebSocketAttemptError {
                error: transport_error,
                events_seen: false,
                output_started: false,
                stale_previous_response: false,
            }
        })
    }

    async fn acquire_websocket(
        &self,
        req: &LlmRequest,
        connect_timeout: Duration,
        credential: &CodexCredential,
        credential_generation: u64,
    ) -> Result<CodexWebsocketLease, CodexWebSocketAttemptError> {
        let scope_key = req.continuation_key();

        enum AcquireDecision {
            Reuse(Box<CodexWebsocketLease>),
            ConnectReusable(String),
            ConnectEphemeral,
        }

        let decision = {
            let mut sessions = self
                .websocket_sessions
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Self::prune_idle_websocket_sessions(&mut sessions);
            Self::enforce_websocket_session_cache_cap(&mut sessions);
            Self::evict_websocket_sessions_for_generation(&mut sessions, credential_generation);
            if let Some(entry) = sessions.by_scope.get_mut(&scope_key) {
                if entry.busy {
                    AcquireDecision::ConnectEphemeral
                } else if let Some(websocket) = entry.connection.take() {
                    entry.busy = true;
                    entry.last_used = Instant::now();
                    AcquireDecision::Reuse(Box::new(CodexWebsocketLease {
                        websocket,
                        scope_key: Some(scope_key),
                        reusable: true,
                        reused: true,
                        continuation: entry.continuation.clone(),
                        credential_generation,
                    }))
                } else {
                    *entry = CodexWebsocketSessionEntry::reserved(credential_generation);
                    AcquireDecision::ConnectReusable(scope_key.clone())
                }
            } else {
                sessions.by_scope.insert(
                    scope_key.clone(),
                    CodexWebsocketSessionEntry::reserved(credential_generation),
                );
                AcquireDecision::ConnectReusable(scope_key.clone())
            }
        };

        match decision {
            AcquireDecision::Reuse(lease) => Ok(*lease),
            AcquireDecision::ConnectEphemeral => {
                let websocket = self
                    .connect_websocket(req, connect_timeout, credential)
                    .await?;
                Ok(CodexWebsocketLease {
                    websocket,
                    scope_key: None,
                    reusable: false,
                    reused: false,
                    continuation: None,
                    credential_generation,
                })
            }
            AcquireDecision::ConnectReusable(scope_key) => {
                let websocket = match self
                    .connect_websocket(req, connect_timeout, credential)
                    .await
                {
                    Ok(websocket) => websocket,
                    Err(error) => {
                        self.remove_websocket_scope(&scope_key);
                        return Err(error);
                    }
                };
                Ok(CodexWebsocketLease {
                    websocket,
                    scope_key: Some(scope_key),
                    reusable: true,
                    reused: false,
                    continuation: None,
                    credential_generation,
                })
            }
        }
    }

    fn release_websocket_lease(
        &self,
        lease: CodexWebsocketLease,
        keep_connection: bool,
        continuation: Option<CodexContinuation>,
    ) {
        let Some(scope_key) = lease.scope_key else {
            return;
        };
        if !lease.reusable || !keep_connection {
            self.remove_websocket_scope(&scope_key);
            return;
        }
        let mut sessions = self
            .websocket_sessions
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.by_scope.insert(
            scope_key,
            CodexWebsocketSessionEntry {
                connection: Some(lease.websocket),
                continuation,
                busy: false,
                last_used: Instant::now(),
                credential_generation: lease.credential_generation,
            },
        );
        Self::prune_idle_websocket_sessions(&mut sessions);
        Self::enforce_websocket_session_cache_cap(&mut sessions);
    }

    fn response_state_started_output(state: &shared::ResponsesStreamState) -> bool {
        !state.parts.is_empty()
            || !state.full_text.is_empty()
            || !state.pending_text_deltas.is_empty()
            || !state.reasoning_deltas.is_empty()
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
                self.release_websocket_lease(lease, false, None);
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
            reused_connection: lease.reused,
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
        let mut lease = Some(lease);
        let mut events_seen = false;
        if let Err(error) = lease
            .as_mut()
            .expect("websocket lease is present")
            .websocket
            .send(WsMessage::Text(request_body.clone().into()))
            .await
        {
            self.release_websocket_lease(
                lease.take().expect("websocket lease is present"),
                false,
                None,
            );
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
            let next_message = tokio::time::timeout(
                read_timeout,
                lease
                    .as_mut()
                    .expect("websocket lease is present")
                    .websocket
                    .next(),
            )
            .await;
            let Some(message) = (match next_message {
                Ok(message) => message,
                Err(_) => {
                    let output_started = Self::response_state_started_output(&state);
                    self.release_websocket_lease(
                        lease.take().expect("websocket lease is present"),
                        false,
                        None,
                    );
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
                    self.release_websocket_lease(
                        lease.take().expect("websocket lease is present"),
                        false,
                        None,
                    );
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
                        self.release_websocket_lease(
                            lease.take().expect("websocket lease is present"),
                            false,
                            None,
                        );
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
                self.release_websocket_lease(
                    lease.take().expect("websocket lease is present"),
                    false,
                    None,
                );
                return Err(CodexWebSocketAttemptError {
                    error: error.with_request_body(request_body.clone()),
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
            partial.generation_disposition = Some(Self::generation_disposition(&req.generation));
            self.release_websocket_lease(
                lease.take().expect("websocket lease is present"),
                false,
                None,
            );
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
        response.generation_disposition = Some(Self::generation_disposition(&req.generation));
        self.release_websocket_lease(
            lease.take().expect("websocket lease is present"),
            true,
            continuation,
        );
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
    fn process_sse_event(
        raw: &str,
        state: &mut shared::ResponsesStreamState,
        emitted_parts: Option<&mut Vec<lash_core::llm::types::LlmOutputPart>>,
    ) -> Result<(), LlmTransportError> {
        shared::process_sse_event(PROVIDER, raw, state, emitted_parts)
    }
}

impl CodexProvider {
    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
            .with_failure_classifier(std::sync::Arc::new(CodexFailureClassifier))
    }
}

#[derive(Debug)]
struct CodexFailureClassifier;

impl ProviderFailureClassifier for CodexFailureClassifier {
    fn classify(&self, failure: ProviderFailure) -> ProviderFailure {
        // The default classifier already covers everything Codex needs from a
        // status/text standpoint: HTTP-status → kind/retryability, the
        // usage-limit/quota and content-filter text markers, and context
        // overflow. Codex's only genuine delta is rewriting the user-facing
        // message into a friendly "you hit your ChatGPT usage limit" form.
        let status = failure
            .status
            .or_else(|| failure.code.as_deref().and_then(|code| code.parse().ok()));
        let summary = status.and_then(|status| {
            CodexProvider::codex_error_summary(
                status,
                failure
                    .raw
                    .as_deref()
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
        });
        let mut failure = DefaultProviderFailureClassifier.classify(failure);
        if let Some(summary) = summary {
            failure.message = summary;
        }
        failure
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn kind(&self) -> &'static str {
        "codex"
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

    async fn complete(&mut self, req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        if self.attempt_credential.is_none() {
            let manager = Arc::clone(&self.credentials);
            let provider = self.clone();
            return manager
                .execute(move |lease| {
                    let mut provider = provider.clone();
                    let req = req.clone();
                    provider.attempt_credential = Some(lease);
                    async move {
                        match Box::pin(provider.complete(req)).await {
                            Ok(response) => Ok(response),
                            Err(error) if error.status == Some(401) => {
                                Err(CredentialCallError::PreOutputAuth(error))
                            }
                            Err(error) => Err(CredentialCallError::Failed(error)),
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
                        return Err(err.error);
                    }
                }
            }
        }
        let stream_events = req.stream_events.clone();
        let provider_trace = req.provider_trace.clone();
        let timeouts = self.options.llm_timeouts();

        let body = self.build_request_body(&req, stream_events.is_some())?;

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
            emit_provider_trace(provider_trace.as_ref(), "codex", &text);
            if Self::looks_like_sse_payload(&text) {
                let mut state = shared::ResponsesStreamState::default();
                shared::parse_sse_payload(PROVIDER, &text, &mut state)?;
                let mut response = shared::response_from_stream_state(
                    state,
                    request_body,
                    format!("HTTP POST {} (stream/fallback)", self.responses_url),
                );
                response.generation_disposition =
                    Some(Self::generation_disposition(&req.generation));
                if let Some(tx) = &stream_events {
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
                execution_evidence: None,
                generation_disposition: Some(Self::generation_disposition(&req.generation)),
                response_metadata: Default::default(),
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

        let mut state = shared::ResponsesStreamState::default();
        let expose_thinking = self.options.expose_thinking;
        let stream_result = drive_sse_response(
            body,
            timeouts.chunk_timeout,
            "Codex stream chunk timed out",
            |raw| {
                emit_provider_trace(provider_trace.as_ref(), "codex", raw);
                let prev_usage = state.usage.clone();
                let mut emitted_parts = Vec::new();
                shared::process_sse_event(PROVIDER, raw, &mut state, Some(&mut emitted_parts))?;
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
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                request_body.clone(),
                format!("HTTP POST {} (stream)", self.responses_url),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = Some(Self::generation_disposition(&req.generation));
            return Err(error.with_partial_response(partial));
        }

        if stream_termination == StreamTermination::RequireTerminalEvidence
            && !state.terminal_event_seen
        {
            let mut partial = shared::response_from_stream_state(
                state.clone(),
                request_body.clone(),
                format!("HTTP POST {} (stream)", self.responses_url),
            );
            partial.terminal_reason = LlmTerminalReason::Unknown;
            partial.generation_disposition = Some(Self::generation_disposition(&req.generation));
            return Err(LlmTransportError::new(
                "Codex stream ended before a terminal response event",
            )
            .with_kind(ProviderFailureKind::Stream)
            .with_code("stream_ended_before_terminal_response")
            .retryable(true)
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
        response.generation_disposition = Some(Self::generation_disposition(&req.generation));
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

#[derive(Deserialize)]
struct CodexProviderConfig {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default = "default_codex_options")]
    options: ProviderOptions,
    #[serde(default)]
    transport: CodexTransport,
}

fn default_codex_options() -> ProviderOptions {
    ProviderOptions {
        reliability: ProviderReliability::codex(),
        ..ProviderOptions::default()
    }
}

/// Factory that materializes [`CodexProvider`] from a host-owned
/// [`ProviderSpec`](lash_core::ProviderSpec).
pub struct CodexProviderFactory;

impl ProviderFactory for CodexProviderFactory {
    fn kind(&self) -> &'static str {
        "codex"
    }
    fn deserialize(&self, config: serde_json::Value) -> Result<ProviderComponents, String> {
        let cfg: CodexProviderConfig =
            serde_json::from_value(config).map_err(|err| err.to_string())?;
        Ok(CodexProvider {
            options: cfg.options,
            transport: cfg.transport,
            ..CodexProvider::new(cfg.access_token, cfg.refresh_token, cfg.expires_at)
                .with_account_id(cfg.account_id)
        }
        .into_components())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::llm::types::{
        LlmJsonSchema, LlmMessage, LlmOutputPart, LlmProviderTraceSender, LlmRequestScope, LlmRole,
        LlmTerminalReason, LlmToolChoice, LlmToolSpec, ResponseTextMeta,
    };
    use lash_core::provider::{ModelCapability, Provider, ReasoningCapability, RequestTimeout};
    use shared::ResponsesStreamState as CodexStreamState;
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use ws_testing::{ScriptedWsAction, assistant_item, spawn_scripted_websocket};

    fn process_event(state: &mut CodexStreamState, event: Value) {
        CodexProvider::process_sse_event(&event.to_string(), state, None).unwrap();
    }

    fn process_event_with_parts(
        state: &mut CodexStreamState,
        event: Value,
        emitted_parts: &mut Vec<LlmOutputPart>,
    ) {
        CodexProvider::process_sse_event(&event.to_string(), state, Some(emitted_parts)).unwrap();
    }

    fn response_from_state(state: CodexStreamState) -> LlmResponse {
        shared::response_from_stream_state(state, None, "test".to_string())
    }

    fn reasoning_capability() -> ModelCapability {
        ModelCapability {
            reasoning: Some(ReasoningCapability {
                efforts: vec!["medium".to_string(), "high".to_string()],
                default_effort: Some("medium".to_string()),
                disable: Some(lash_core::provider::ReasoningDisableEncoding::Effort(
                    "none".to_string(),
                )),
                ..ReasoningCapability::default()
            }),
            cache_control: None,
            stream_termination: None,
            sampling: lash_core::SamplingCapability::Configurable,
        }
    }

    fn request(messages: Vec<LlmMessage>) -> LlmRequest {
        LlmRequest {
            model: "gpt-5.4".to_string(),
            messages,
            attachments: Vec::new(),
            resolved_stored: Default::default(),
            tools: Arc::new(Vec::<LlmToolSpec>::new()),
            tool_choice: LlmToolChoice::Auto,
            model_variant: Default::default(),
            model_capability: ModelCapability::default(),
            scope: LlmRequestScope::new(
                "session-1",
                "session-1:frame:test",
                "session-1:request:test",
            ),
            output_spec: None,
            stream_events: None,
            generation: lash_core::GenerationOptions::default(),
            provider_trace: None,
        }
    }

    fn traced_request(messages: Vec<LlmMessage>, trace: Arc<Mutex<Vec<Value>>>) -> LlmRequest {
        let mut req = request(messages);
        req.provider_trace = Some(LlmProviderTraceSender::new(move |event| {
            if let Ok(value) = serde_json::from_str::<Value>(&event.raw) {
                trace
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(value);
            }
        }));
        req
    }

    fn websocket_diagnostics(trace: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
        trace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|value| {
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "lash.codex.websocket_request")
            })
            .cloned()
            .collect()
    }

    fn websocket_test_provider(
        transport: CodexTransport,
        responses_url: String,
        websocket_url: String,
    ) -> CodexProvider {
        CodexProvider::new("access", "refresh", 0)
            .with_transport(transport)
            .with_options(ProviderOptions {
                reliability: ProviderReliability::codex()
                    .request_timeout(Some(RequestTimeout::Millis(5_000)))
                    .stream_chunk_timeout_ms(Some(50)),
                ..ProviderOptions::default()
            })
            .with_endpoint_urls(responses_url, websocket_url)
    }

    fn assistant_message_with_meta(message_id: &str, text: &str) -> LlmMessage {
        LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: text.into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some(message_id.to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        )
    }

    struct HttpSseServer {
        url: String,
        captured: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    impl HttpSseServer {
        fn captured(&self) -> Vec<String> {
            self.captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn captured_len(&self) -> usize {
            self.captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    impl Drop for HttpSseServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn spawn_http_sse(
        response_id: &'static str,
        message_id: &'static str,
        text: &'static str,
    ) -> HttpSseServer {
        spawn_http_sse_sequence(vec![(response_id, message_id, text)]).await
    }

    async fn spawn_http_sse_sequence(
        responses: Vec<(&'static str, &'static str, &'static str)>,
    ) -> HttpSseServer {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind http");
        let addr = listener.local_addr().expect("http addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let task_captured = Arc::clone(&captured);
        let task = tokio::spawn(async move {
            for (response_id, message_id, text) in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let Ok(n) = stream.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                task_captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(String::from_utf8_lossy(&request).into_owned());
                let item = assistant_item(message_id, text);
                let body = format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"type":"response.output_item.done","output_index":0,"item":item}),
                    json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[assistant_item(message_id, text)],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        HttpSseServer {
            url: format!("http://{addr}/codex/responses"),
            captured,
            task,
        }
    }

    #[test]
    fn codex_null_incomplete_details_does_not_map_to_output_limit() {
        let terminal_reason = openai_terminal_reason_from_response_value(
            &json!({"status":"completed","incomplete_details":null}),
            &[LlmOutputPart::Text {
                text: "Hi".to_string(),
                response_meta: None,
            }],
        );

        assert_eq!(terminal_reason, LlmTerminalReason::Stop);
    }

    #[test]
    fn codex_content_filter_incomplete_maps_to_content_filter() {
        let terminal_reason = openai_terminal_reason_from_response_value(
            &json!({"status":"incomplete","incomplete_details":{"reason":"content_filter"}}),
            &[],
        );

        assert_eq!(terminal_reason, LlmTerminalReason::ContentFilter);
    }

    #[test]
    fn codex_request_body_emits_reasoning_from_capability_variant() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model = "custom-codex-model".to_string();
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
        req.model_capability = reasoning_capability();

        let body = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, true)
            .unwrap();

        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
    }

    #[test]
    fn codex_request_body_emits_none_effort_for_disabled_selection() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
        req.model_capability = reasoning_capability();

        let body = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, true)
            .unwrap();

        assert_eq!(body["reasoning"], json!({ "effort": "none" }));
    }

    #[test]
    fn codex_request_body_omits_reasoning_without_capability() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model = "custom-codex-model".to_string();
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());

        let body = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, true)
            .unwrap();

        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn codex_request_body_exposes_reasoning_summary_only_when_configured() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
        req.model_capability = reasoning_capability();

        let hidden = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, true)
            .unwrap();
        assert_eq!(hidden["reasoning"], json!({ "effort": "medium" }));

        let exposed = CodexProvider::new("access", "refresh", 0)
            .with_options(ProviderOptions {
                expose_thinking: true,
                ..ProviderOptions::default()
            })
            .build_request_body(&req, true)
            .unwrap();
        assert_eq!(exposed["reasoning"]["summary"], "auto");
    }

    #[test]
    fn codex_request_omits_output_token_cap() {
        let provider = CodexProvider::new("access", "refresh", 0).with_options(ProviderOptions {
            max_output_tokens: Some(9_999),
            ..ProviderOptions::default()
        });
        let provider_limited = provider
            .build_request_body(
                &request(vec![LlmMessage::text(LlmRole::User, "hello")]),
                false,
            )
            .unwrap();
        assert!(provider_limited.get("max_output_tokens").is_none());

        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.generation.output_token_cap = NonZeroUsize::new(2_048);
        let request_limited = provider.build_request_body(&req, false).unwrap();
        assert!(request_limited.get("max_output_tokens").is_none());
    }

    #[test]
    fn codex_error_summary_uses_top_level_detail() {
        let summary =
            CodexProvider::codex_error_summary(400, r#"{"detail":"Unsupported parameter: foo"}"#);
        assert_eq!(
            summary.as_deref(),
            Some("Codex request failed with 400: Unsupported parameter: foo")
        );
    }

    #[test]
    fn response_failed_server_error_is_retryable() {
        let mut state = CodexStreamState::default();
        let err = CodexProvider::process_sse_event(
            r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"internal stream ended unexpectedly"}}}"#,
            &mut state,
            None,
        )
        .unwrap_err();

        assert!(err.retryable);
        assert_eq!(err.message, "internal stream ended unexpectedly");
    }

    #[test]
    fn codex_request_uses_openai_schema_projection() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.tools = Arc::new(vec![LlmToolSpec {
            name: "empty".to_string(),
            description: "Empty".to_string(),
            input_schema: json!({"type": "object"}).into(),
            output_schema: json!({}).into(),
        }]);
        req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
            name: "result".to_string(),
            schema: json!({
                "type": "object",
                "properties": { "summary": { "type": "string" } }
            })
            .into(),
            strict: true,
        }));

        let body = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, false)
            .unwrap();
        assert_eq!(body["tools"][0]["parameters"]["properties"], json!({}));
        assert_eq!(
            body["text"]["format"]["schema"]["required"],
            json!(["summary"])
        );
        assert_eq!(
            body["text"]["format"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn codex_request_history_preserves_assistant_message_metadata() {
        let req = request(vec![LlmMessage::new(
            LlmRole::Assistant,
            vec![lash_core::llm::types::LlmContentBlock::Text {
                text: "final".into(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("final_answer".to_string()),
                    ..ResponseTextMeta::default()
                }),
                cache_breakpoint: false,
            }],
        )]);

        let body = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, false)
            .unwrap();

        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["id"], "msg_1");
        assert_eq!(body["input"][0]["status"], "completed");
        assert_eq!(body["input"][0]["phase"], "final_answer");
        assert_eq!(body["input"][0]["content"][0]["type"], "output_text");
        assert!(body["input"][0]["content"][0]["annotations"].is_array());
    }

    #[test]
    fn codex_cached_continuation_sends_delta_after_prior_request_and_response_items() {
        let provider = CodexProvider::new("access", "refresh", 0)
            .with_transport(CodexTransport::WebsocketCached);
        let first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        let first_body = provider.build_request_body(&first, true).unwrap();
        let assistant_item = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": "answer", "annotations": []}]
        });
        let continuation = CodexProvider::continuation_from_response(
            &first_body,
            &json!({
                "id": "resp_1",
                "status": "completed",
                "output": [assistant_item]
            }),
        )
        .expect("completed continuation");

        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![lash_core::llm::types::LlmContentBlock::Text {
                    text: "answer".into(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: Some("final_answer".to_string()),
                        ..ResponseTextMeta::default()
                    }),
                    cache_breakpoint: false,
                }],
            ),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let second_body = provider.build_request_body(&second, true).unwrap();
        let cached_body =
            CodexProvider::cached_websocket_body(&continuation, &second_body).expect("cached body");

        assert_eq!(cached_body["previous_response_id"], "resp_1");
        assert_eq!(
            cached_body["input"].as_array().expect("delta input").len(),
            1
        );
        assert_eq!(cached_body["input"][0]["role"], "user");
        assert_eq!(cached_body["input"][0]["content"][0]["text"], "next");
    }

    #[test]
    fn codex_websocket_cache_miss_reasons_are_explicit() {
        let provider = CodexProvider::new("access", "refresh", 0)
            .with_transport(CodexTransport::WebsocketCached);
        let first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        let first_body = provider.build_request_body(&first, true).unwrap();
        let continuation = CodexProvider::continuation_from_response(
            &first_body,
            &json!({
                "id": "resp_1",
                "status": "completed",
                "output": [assistant_item("msg_1", "answer")]
            }),
        )
        .expect("completed continuation");

        let mut fingerprint_mismatch_body = first_body.clone();
        fingerprint_mismatch_body["model"] = json!("gpt-5-codex");
        let fingerprint_plan =
            provider.websocket_request_plan(&fingerprint_mismatch_body, Some(&continuation), true);
        assert!(!fingerprint_plan.cached);
        assert_eq!(
            fingerprint_plan.cache_miss_reason,
            Some("body_fingerprint_mismatch")
        );

        let prefix_mismatch = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::text(LlmRole::User, "next without prior assistant"),
        ]);
        let prefix_mismatch_body = provider.build_request_body(&prefix_mismatch, true).unwrap();
        let prefix_plan =
            provider.websocket_request_plan(&prefix_mismatch_body, Some(&continuation), true);
        assert!(!prefix_plan.cached);
        assert_eq!(prefix_plan.cache_miss_reason, Some("input_prefix_mismatch"));
    }

    #[test]
    fn codex_websocket_request_uses_response_create_event_shape() {
        let provider = CodexProvider::new("access", "refresh", 0);
        let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        let body = provider.build_request_body(&req, true).unwrap();
        let websocket_body = CodexProvider::websocket_create_request(&body);

        assert_eq!(websocket_body["type"], "response.create");
        assert_eq!(websocket_body["model"], body["model"]);
        assert_eq!(websocket_body["input"], body["input"]);
        assert_eq!(websocket_body["stream"], true);
        assert!(websocket_body.get("response").is_none());
    }

    #[test]
    fn codex_websocket_request_keeps_cached_previous_response_id() {
        let provider = CodexProvider::new("access", "refresh", 0);
        let req = request(vec![LlmMessage::text(LlmRole::User, "next")]);
        let mut body = provider.build_request_body(&req, true).unwrap();
        body["previous_response_id"] = json!("resp_1");
        body["input"] = json!([]);

        let websocket_body = CodexProvider::websocket_create_request(&body);

        assert_eq!(websocket_body["type"], "response.create");
        assert_eq!(websocket_body["previous_response_id"], "resp_1");
        assert_eq!(websocket_body["input"], json!([]));
    }

    #[test]
    fn codex_websocket_scope_cache_prunes_idle_entries_and_caps_oldest() {
        let now = Instant::now();
        let mut sessions = CodexWebsocketSessions::default();
        sessions.by_scope.insert(
            "idle".to_string(),
            CodexWebsocketSessionEntry {
                connection: None,
                continuation: None,
                busy: false,
                last_used: now - SESSION_WEBSOCKET_CACHE_TTL - Duration::from_secs(1),
                credential_generation: 0,
            },
        );
        sessions.by_scope.insert(
            "busy".to_string(),
            CodexWebsocketSessionEntry {
                connection: None,
                continuation: None,
                busy: true,
                last_used: now - SESSION_WEBSOCKET_CACHE_TTL - Duration::from_secs(1),
                credential_generation: 0,
            },
        );

        CodexProvider::prune_idle_websocket_sessions(&mut sessions);

        assert!(!sessions.by_scope.contains_key("idle"));
        assert!(sessions.by_scope.contains_key("busy"));

        sessions.by_scope.clear();
        for index in 0..(MAX_SESSION_WEBSOCKET_CACHE_ENTRIES + 3) {
            sessions.by_scope.insert(
                format!("scope-{index}"),
                CodexWebsocketSessionEntry {
                    connection: None,
                    continuation: None,
                    busy: false,
                    last_used: now - Duration::from_secs((100 - index) as u64),
                    credential_generation: 0,
                },
            );
        }

        CodexProvider::enforce_websocket_session_cache_cap(&mut sessions);

        assert_eq!(sessions.by_scope.len(), MAX_SESSION_WEBSOCKET_CACHE_ENTRIES);
        assert!(!sessions.by_scope.contains_key("scope-0"));
        assert!(sessions.by_scope.contains_key(&format!(
            "scope-{}",
            MAX_SESSION_WEBSOCKET_CACHE_ENTRIES + 2
        )));
    }

    #[test]
    fn codex_websocket_scope_cache_evicts_rotated_credentials() {
        let mut sessions = CodexWebsocketSessions::default();
        sessions.by_scope.insert(
            "old".to_string(),
            CodexWebsocketSessionEntry {
                connection: None,
                continuation: None,
                busy: false,
                last_used: Instant::now(),
                credential_generation: 4,
            },
        );
        sessions.by_scope.insert(
            "current".to_string(),
            CodexWebsocketSessionEntry {
                connection: None,
                continuation: None,
                busy: false,
                last_used: Instant::now(),
                credential_generation: 5,
            },
        );

        CodexProvider::evict_websocket_sessions_for_generation(&mut sessions, 5);

        assert!(!sessions.by_scope.contains_key("old"));
        assert!(sessions.by_scope.contains_key("current"));
    }

    async fn assert_trace_cached_delta_for_transport(transport: CodexTransport) {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "done",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            transport,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );
        let trace = Arc::new(Mutex::new(Vec::new()));

        provider
            .complete(traced_request(
                vec![LlmMessage::text(LlmRole::User, "hello")],
                Arc::clone(&trace),
            ))
            .await
            .expect("first response");
        let response = provider
            .complete(traced_request(
                vec![
                    LlmMessage::text(LlmRole::User, "hello"),
                    assistant_message_with_meta("msg_1", "answer"),
                    LlmMessage::text(LlmRole::User, "next"),
                ],
                Arc::clone(&trace),
            ))
            .await
            .expect("cached follow-up response");

        assert_eq!(response.full_text, "done");
        let diagnostics = websocket_diagnostics(&trace);
        assert_eq!(diagnostics.len(), 2, "{transport:?}");
        assert_eq!(diagnostics[0]["transport"], format!("{transport:?}"));
        assert_eq!(diagnostics[0]["cached_request"], false);
        assert_eq!(diagnostics[0]["cache_miss_reason"], "missing_continuation");
        assert_eq!(diagnostics[1]["transport"], format!("{transport:?}"));
        assert_eq!(diagnostics[1]["reused_connection"], true);
        assert_eq!(diagnostics[1]["cached_request"], true);
        assert_eq!(diagnostics[1]["previous_response_id"], "resp_1");
        assert_eq!(diagnostics[1]["sent_input_items"], 1);
        assert_eq!(diagnostics[1]["retry_after_stale_previous_response"], false);
    }

    async fn assert_trace_stale_retry_for_transport(transport: CodexTransport) {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Error {
                message: "Previous response with id 'resp_1' not found",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "recovered",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            transport,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );
        let trace = Arc::new(Mutex::new(Vec::new()));

        provider
            .complete(traced_request(
                vec![LlmMessage::text(LlmRole::User, "hello")],
                Arc::clone(&trace),
            ))
            .await
            .expect("first response");
        let response = provider
            .complete(traced_request(
                vec![
                    LlmMessage::text(LlmRole::User, "hello"),
                    assistant_message_with_meta("msg_1", "answer"),
                    LlmMessage::text(LlmRole::User, "next"),
                ],
                Arc::clone(&trace),
            ))
            .await
            .expect("stale retry response");

        assert_eq!(response.full_text, "recovered");
        let diagnostics = websocket_diagnostics(&trace);
        assert_eq!(diagnostics.len(), 3, "{transport:?}");
        assert_eq!(diagnostics[1]["reused_connection"], true);
        assert_eq!(diagnostics[1]["cached_request"], true);
        assert_eq!(diagnostics[1]["previous_response_id"], "resp_1");
        assert_eq!(diagnostics[2]["cached_request"], false);
        assert_eq!(diagnostics[2]["cache_miss_reason"], "disabled");
        assert_eq!(diagnostics[2]["retry_after_stale_previous_response"], true);
        assert!(
            diagnostics[2]
                .get("previous_response_id")
                .is_none_or(Value::is_null)
        );
    }

    #[tokio::test]
    async fn codex_scripted_websocket_trace_diagnostics_cover_cached_delta_and_stale_retry() {
        for transport in [CodexTransport::WebsocketCached, CodexTransport::Auto] {
            assert_trace_cached_delta_for_transport(transport).await;
            assert_trace_stale_retry_for_transport(transport).await;
        }
    }

    #[tokio::test]
    async fn codex_scripted_websocket_full_turn_sends_response_create() {
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "ok",
        }])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::Websocket,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        let response = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("websocket response");

        assert_eq!(response.full_text, "ok");
        let captured = ws.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["type"], "response.create");
        assert!(captured[0].get("previous_response_id").is_none());
        let headers = ws.handshakes();
        assert_eq!(headers.len(), 1);
        let header = |name: &str| {
            headers[0]
                .iter()
                .find_map(|(header_name, value)| (header_name == name).then_some(value.as_str()))
        };
        assert_eq!(header("session-id"), Some("session-1"));
        assert_eq!(
            header("x-client-request-id"),
            Some("session-1:request:test")
        );
        assert_eq!(header("session_id"), None);
    }

    #[tokio::test]
    async fn codex_scripted_websocket_cached_follow_up_omits_previous_assistant_output() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "done",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![lash_core::llm::types::LlmContentBlock::Text {
                    text: "answer".into(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: Some("final_answer".to_string()),
                        ..ResponseTextMeta::default()
                    }),
                    cache_breakpoint: false,
                }],
            ),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let response = provider.complete(second).await.expect("second response");

        assert_eq!(response.full_text, "done");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("cached=true")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[1]["previous_response_id"], "resp_1");
        assert_eq!(captured[1]["input"].as_array().unwrap().len(), 1);
        assert_eq!(captured[1]["input"][0]["content"][0]["text"], "next");
    }

    #[tokio::test]
    async fn codex_provider_close_sends_websocket_close_frame_for_cached_session() {
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        }])
        .await;
        let provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        // A completed turn leaves a reusable WebSocket session cached.
        let mut running = provider.clone();
        running
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        assert_eq!(ws.close_frame_count(), 0, "no close before shutdown");

        // The host-callable close drains the cache with a proper Close frame,
        // not a bare TCP drop. The cache is shared across clones, so closing the
        // retained clone releases the socket the running handle cached.
        provider.close().await.expect("provider close");

        let deadline = Instant::now() + Duration::from_secs(5);
        while ws.close_frame_count() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            ws.close_frame_count(),
            1,
            "close() must send a WebSocket Close frame to the cached session"
        );

        // The cache is empty after close.
        assert!(
            provider
                .websocket_sessions
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .by_scope
                .is_empty(),
            "close() drains the session cache"
        );
    }

    #[tokio::test]
    async fn codex_provider_close_drains_a_dead_cached_socket_within_bound() {
        // A peer that closed its side leaves a dead socket in the cache. The
        // bounded, best-effort per-socket close must tolerate it: close() returns
        // promptly and still empties the cache, so a wedged socket can never fail
        // the drain or stall the sockets queued behind it.
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::CompleteAndClose {
            response_id: "resp_1",
            message_id: "msg_1",
            text: "answer",
        }])
        .await;
        let provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        let mut running = provider.clone();
        running
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        // Let the peer's Close frame land so the cached socket is genuinely dead,
        // without a reuse ever polling (and evicting) it first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            provider
                .websocket_sessions
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .by_scope
                .len(),
            1,
            "the completed turn leaves a (now dead) socket cached for the drain"
        );

        // An unbounded close on a wedged socket could hang here; the per-socket
        // timeout keeps the drain moving. The outer guard turns a regression into
        // a failure instead of hanging the whole suite.
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(20), provider.close())
            .await
            .expect("close() must not hang draining a dead cached socket")
            .expect("provider close");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "each socket close is bounded, drain took {:?}",
            started.elapsed()
        );
        assert!(
            provider
                .websocket_sessions
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .by_scope
                .is_empty(),
            "close() drains the cache even when a cached socket is dead"
        );
    }

    #[tokio::test]
    async fn codex_scripted_websocket_same_session_different_frame_does_not_reuse_continuation() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "done",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        let mut second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![lash_core::llm::types::LlmContentBlock::Text {
                    text: "answer".into(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: Some("final_answer".to_string()),
                        ..ResponseTextMeta::default()
                    }),
                    cache_breakpoint: false,
                }],
            ),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        second.scope = LlmRequestScope::new(
            "session-1",
            "session-1:frame:other",
            "session-1:request:other",
        );
        let response = provider.complete(second).await.expect("second response");

        assert_eq!(response.full_text, "done");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("cache_miss=missing_continuation")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 2);
        assert!(captured[1].get("previous_response_id").is_none());
        assert_eq!(captured[1]["input"].as_array().unwrap().len(), 3);
        let handshakes = ws.handshakes();
        assert_eq!(
            handshakes[1]
                .iter()
                .find_map(|(name, value)| (name == "session-id").then_some(value.as_str())),
            Some("session-1")
        );
        assert_eq!(
            handshakes[1].iter().find_map(|(name, value)| {
                (name == "x-client-request-id").then_some(value.as_str())
            }),
            Some("session-1:request:other")
        );
    }

    #[tokio::test]
    async fn codex_scripted_websocket_stale_previous_response_retries_full_context_once() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Error {
                message: "Previous response with id 'resp_1' not found",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "recovered",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![lash_core::llm::types::LlmContentBlock::Text {
                    text: "answer".into(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: Some("final_answer".to_string()),
                        ..ResponseTextMeta::default()
                    }),
                    cache_breakpoint: false,
                }],
            ),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let full_body = provider.build_request_body(&second, true).unwrap();
        let response = provider
            .complete(second)
            .await
            .expect("stale retry response");

        assert_eq!(response.full_text, "recovered");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("retry_after_stale=true")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 3);
        assert_eq!(captured[1]["previous_response_id"], "resp_1");
        assert!(captured[2].get("previous_response_id").is_none());
        assert_eq!(captured[2]["input"], full_body["input"]);
    }

    #[tokio::test]
    async fn codex_scripted_websocket_dead_reused_socket_reconnects_full_context() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::CompleteAndClose {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "answer",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "reconnected",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first response");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            LlmMessage::new(
                LlmRole::Assistant,
                vec![lash_core::llm::types::LlmContentBlock::Text {
                    text: "answer".into(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: Some("final_answer".to_string()),
                        ..ResponseTextMeta::default()
                    }),
                    cache_breakpoint: false,
                }],
            ),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let full_body = provider.build_request_body(&second, true).unwrap();
        let response = provider
            .complete(second)
            .await
            .expect("dead reused socket reconnect response");

        assert_eq!(response.full_text, "reconnected");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("retry_after_dead_reused=true")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 2);
        assert!(captured[1].get("previous_response_id").is_none());
        assert_eq!(captured[1]["input"], full_body["input"]);
        assert_eq!(ws.handshakes().len(), 2);
    }

    #[tokio::test]
    async fn codex_scripted_websocket_incomplete_terminal_response_is_not_cached() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Incomplete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "partial",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "fresh",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::WebsocketCached,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );

        provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("incomplete terminal response");
        let second = request(vec![
            LlmMessage::text(LlmRole::User, "hello"),
            assistant_message_with_meta("msg_1", "partial"),
            LlmMessage::text(LlmRole::User, "next"),
        ]);
        let full_body = provider.build_request_body(&second, true).unwrap();
        let response = provider
            .complete(second)
            .await
            .expect("fresh response after incomplete terminal");

        assert_eq!(response.full_text, "fresh");
        assert!(
            response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("cache_miss=missing_continuation")
        );
        let captured = ws.captured();
        assert_eq!(captured.len(), 2);
        assert!(captured[1].get("previous_response_id").is_none());
        assert_eq!(captured[1]["input"], full_body["input"]);
    }

    #[tokio::test]
    async fn codex_auto_with_distinct_scopes_uses_uncached_websockets() {
        let ws = spawn_scripted_websocket(vec![
            ScriptedWsAction::Complete {
                response_id: "resp_1",
                message_id: "msg_1",
                text: "one",
            },
            ScriptedWsAction::Complete {
                response_id: "resp_2",
                message_id: "msg_2",
                text: "two",
            },
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::Auto,
            "http://127.0.0.1:9/unused".to_string(),
            ws.url.clone(),
        );
        let mut first = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        first.scope = LlmRequestScope::new("direct-a", "direct-a:frame", "direct-a:request");
        let mut second = request(vec![LlmMessage::text(LlmRole::User, "next")]);
        second.scope = LlmRequestScope::new("direct-b", "direct-b:frame", "direct-b:request");

        let first_response = provider.complete(first).await.expect("first response");
        let second_response = provider.complete(second).await.expect("second response");

        assert_eq!(first_response.full_text, "one");
        assert_eq!(second_response.full_text, "two");
        assert!(
            second_response
                .http_summary
                .as_deref()
                .unwrap_or_default()
                .contains("reused=false")
        );
        assert_eq!(ws.captured().len(), 2);
        let handshakes = ws.handshakes();
        assert_eq!(handshakes.len(), 2);
        fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
            headers
                .iter()
                .find_map(|(header_name, value)| (header_name == name).then_some(value.as_str()))
        }
        assert_eq!(header(&handshakes[0], "session-id"), Some("direct-a"));
        assert_eq!(
            header(&handshakes[0], "x-client-request-id"),
            Some("direct-a:request")
        );
        assert_eq!(header(&handshakes[1], "session-id"), Some("direct-b"));
        assert_eq!(
            header(&handshakes[1], "x-client-request-id"),
            Some("direct-b:request")
        );
    }

    #[tokio::test]
    async fn codex_scripted_websocket_mid_stream_failure_does_not_fallback() {
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::MidStreamError {
            message_id: "msg_1",
            text: "partial",
            message: "stream exploded",
        }])
        .await;
        let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
        let mut provider =
            websocket_test_provider(CodexTransport::Auto, http.url.clone(), ws.url.clone());

        let err = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect_err("mid-stream websocket failure");

        assert!(err.message.contains("stream exploded"));
        assert_eq!(http.captured_len(), 0);
        assert_eq!(ws.captured().len(), 1);
    }

    #[tokio::test]
    async fn codex_scripted_websocket_idle_before_start_falls_back_to_sse() {
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::IdleBeforeStart]).await;
        let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
        let mut provider =
            websocket_test_provider(CodexTransport::Auto, http.url.clone(), ws.url.clone());

        let response = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("sse fallback response");

        assert_eq!(response.full_text, "fallback");
        assert_eq!(ws.captured().len(), 1);
        assert_eq!(http.captured_len(), 1);
    }

    #[tokio::test]
    async fn codex_auto_skips_websocket_while_session_fallback_is_active() {
        let http = spawn_http_sse_sequence(vec![
            ("resp_http_1", "msg_http_1", "fallback-one"),
            ("resp_http_2", "msg_http_2", "fallback-two"),
        ])
        .await;
        let mut provider = websocket_test_provider(
            CodexTransport::Auto,
            http.url.clone(),
            "ws://127.0.0.1:1/codex/responses".to_string(),
        );

        let first = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect("first SSE fallback response");

        assert_eq!(first.full_text, "fallback-one");
        assert!(
            provider
                .websocket_fallback_reason(&request(vec![LlmMessage::text(LlmRole::User, "hello")]))
                .is_some()
        );

        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::Complete {
            response_id: "resp_ws",
            message_id: "msg_ws",
            text: "should-not-run",
        }])
        .await;
        provider.websocket_url = ws.url.clone();
        let second = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "next")]))
            .await
            .expect("second SSE fallback response");

        assert_eq!(second.full_text, "fallback-two");
        assert_eq!(ws.captured().len(), 0);
        assert_eq!(http.captured_len(), 2);
        let sse_request = http.captured().remove(0);
        assert!(sse_request.contains("session-id: session-1"));
        assert!(sse_request.contains("x-client-request-id: session-1:request:test"));
        assert!(!sse_request.contains("session_id:"));
    }

    #[tokio::test]
    async fn codex_scripted_websocket_idle_after_output_is_terminal_error() {
        let ws = spawn_scripted_websocket(vec![ScriptedWsAction::IdleAfterStart {
            message_id: "msg_1",
            text: "partial",
        }])
        .await;
        let http = spawn_http_sse("resp_http", "msg_http", "fallback").await;
        let mut provider =
            websocket_test_provider(CodexTransport::Auto, http.url.clone(), ws.url.clone());

        let err = provider
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect_err("idle after output");

        assert_eq!(err.code.as_deref(), Some("websocket_idle_timeout"));
        assert_eq!(http.captured_len(), 0);
        assert_eq!(ws.captured().len(), 1);
    }

    #[tokio::test]
    async fn codex_websocket_clean_eof_requires_terminal_event_unless_explicitly_tolerated() {
        let action = ScriptedWsAction::CloseAfterStart {
            response_id: "resp_partial",
            message_id: "msg_partial",
            text: "partial",
        };
        let strict_ws = spawn_scripted_websocket(vec![action.clone()]).await;
        let http = spawn_http_sse("resp_http", "msg_http", "unused").await;
        let mut strict = websocket_test_provider(
            CodexTransport::Websocket,
            http.url.clone(),
            strict_ws.url.clone(),
        );

        let error = strict
            .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
            .await
            .expect_err("clean EOF without a terminal event must fail");

        assert_eq!(
            error.code.as_deref(),
            Some("websocket_closed_before_completed")
        );
        let partial = error.partial_response.as_deref().expect("partial response");
        assert_eq!(partial.full_text, "partial");
        assert_eq!(partial.usage.input_tokens, 4);
        assert_eq!(partial.usage.output_tokens, 1);
        assert!(partial.provider_usage.is_some());
        assert_eq!(http.captured_len(), 0);

        let tolerant_ws = spawn_scripted_websocket(vec![action]).await;
        let mut tolerant = websocket_test_provider(
            CodexTransport::Websocket,
            http.url.clone(),
            tolerant_ws.url.clone(),
        );
        let mut tolerant_request = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        tolerant_request.model_capability.stream_termination =
            Some(StreamTermination::EofTolerated);
        let response = tolerant
            .complete(tolerant_request)
            .await
            .expect("explicit EOF tolerance accepts clean close");
        assert_eq!(response.full_text, "partial");
        assert_eq!(http.captured_len(), 0);
    }

    #[test]
    fn codex_schema_projection_failure_is_local_validation_error() {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
            name: "bad".to_string(),
            schema: json!({"type": "object", "allOf": []}).into(),
            strict: true,
        }));

        let err = CodexProvider::new("access", "refresh", 0)
            .build_request_body(&req, false)
            .unwrap_err();
        assert_eq!(err.kind, ProviderFailureKind::Validation);
        assert!(err.message.contains("allOf"));
    }

    #[test]
    fn codex_stream_response_carries_raw_usage_sidecar() {
        let mut state = CodexStreamState::default();
        process_event(
            &mut state,
            json!({"type":"response.completed","response":{
                "id":"resp_usage",
                "status":"completed",
                "output":[assistant_item("msg_usage","hi")],
                "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
            }}),
        );

        let response = response_from_state(state);

        assert_eq!(
            response.provider_usage,
            Some(json!({"input_tokens":3,"output_tokens":2,"total_tokens":5}))
        );
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 2);
    }

    #[test]
    fn codex_stream_assembles_single_message_item_once() {
        let mut state = CodexStreamState::default();

        process_event(
            &mut state,
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1","status":"in_progress","phase":"commentary"}}),
        );
        process_event(
            &mut state,
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hel"}),
        );
        process_event(
            &mut state,
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Hello"}]}}),
        );

        let response = response_from_state(state);
        assert_eq!(response.full_text, "Hello");
        assert_eq!(response.parts.len(), 1);
        assert_eq!(
            response.parts[0],
            LlmOutputPart::Text {
                text: "Hello".to_string(),
                response_meta: Some(ResponseTextMeta {
                    id: Some("msg_1".to_string()),
                    status: Some("completed".to_string()),
                    phase: Some("commentary".to_string()),
                    ..ResponseTextMeta::default()
                }),
            }
        );
    }

    #[test]
    fn codex_stream_replayed_message_item_does_not_duplicate_text() {
        let mut state = CodexStreamState::default();

        for event in [
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"The sentence."}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"The sentence."}]}}),
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"The sentence."}]}}),
        ] {
            process_event(&mut state, event);
        }

        let response = response_from_state(state);
        assert_eq!(response.full_text, "The sentence.");
        assert_eq!(
            response
                .parts
                .iter()
                .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn codex_stream_completed_response_merges_existing_message_by_id() {
        let mut state = CodexStreamState::default();

        for event in [
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Final answer."}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Final answer."}]}}),
            json!({"type":"response.completed","response":{"id":"resp_1","output_text":"Final answer.","output":[{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"Final answer."}]}]}}),
        ] {
            process_event(&mut state, event);
        }

        let response = response_from_state(state);
        assert_eq!(response.full_text, "Final answer.");
        assert_eq!(response.parts.len(), 1);
    }

    #[test]
    fn codex_stream_distinct_message_ids_stay_separate_without_inserted_separator() {
        let mut state = CodexStreamState::default();

        for event in [
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"One."}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"One."}]}}),
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_2"}}),
            json!({"type":"response.output_text.delta","item_id":"msg_2","delta":"Two."}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_2","status":"completed","content":[{"type":"output_text","text":"Two."}]}}),
        ] {
            process_event(&mut state, event);
        }

        let response = response_from_state(state);
        assert_eq!(response.full_text, "One.Two.");
        assert_eq!(response.parts.len(), 2);
        assert_eq!(
            response.parts,
            vec![
                LlmOutputPart::Text {
                    text: "One.".to_string(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_1".to_string()),
                        status: Some("completed".to_string()),
                        phase: None,
                        ..ResponseTextMeta::default()
                    }),
                },
                LlmOutputPart::Text {
                    text: "Two.".to_string(),
                    response_meta: Some(ResponseTextMeta {
                        id: Some("msg_2".to_string()),
                        status: Some("completed".to_string()),
                        phase: None,
                        ..ResponseTextMeta::default()
                    }),
                },
            ]
        );
    }

    #[test]
    fn codex_stream_preserves_reasoning_message_and_tool_call_once() {
        let mut state = CodexStreamState::default();
        let mut emitted_parts = Vec::new();

        for event in [
            json!({"type":"response.reasoning_summary_part.added"}),
            json!({"type":"response.reasoning_summary_text.delta","delta":"Think"}),
            json!({"type":"response.reasoning_summary_part.done"}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"enc"}}),
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg_1","phase":"final_answer"}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hi"}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Hi"}]}}),
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"x\""}),
            json!({"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"x\":1}"}),
            json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":"{\"x\":1}","status":"completed"}}),
        ] {
            process_event_with_parts(&mut state, event, &mut emitted_parts);
        }
        process_event_with_parts(
            &mut state,
            json!({"type":"response.completed","response":{"id":"resp_1","output":[
                {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"enc"},
                {"type":"message","id":"msg_1","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Hi"}]},
                {"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":"{\"x\":1}","status":"completed"}
            ],"output_text":"Hi"}}),
            &mut emitted_parts,
        );

        let response = response_from_state(state);
        assert_eq!(response.full_text, "Hi");
        assert_eq!(emitted_parts.len(), 1);
        assert_eq!(
            response
                .parts
                .iter()
                .filter(|part| matches!(part, LlmOutputPart::Reasoning { .. }))
                .count(),
            1
        );
        assert_eq!(
            response
                .parts
                .iter()
                .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
                .count(),
            1
        );
        assert_eq!(
            response
                .parts
                .iter()
                .filter(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
                .count(),
            1
        );
    }

    /// Cross-provider response-normalization conformance. Codex shares OpenAI's
    /// Responses-API normalizers (`shared::*`), so this wires those into the
    /// shared suite with Responses-API wire fixtures.
    #[cfg(feature = "testing")]
    mod conformance {
        use super::super::{PROVIDER, shared};
        use lash_core::llm::types::{LlmOutputPart, LlmTerminalReason, LlmUsage};
        use lash_llm_transport::conformance::{
            CanonicalUsage as U, ProviderNormalizer, ProviderWire, Scenario, StreamAssembly,
            provider_conformance,
        };
        use lash_llm_transport::{
            openai_terminal_reason_from_response_value, openai_usage_from_response_value,
        };
        use serde_json::{Value, json};

        struct CodexNormalizer;

        impl ProviderNormalizer for CodexNormalizer {
            fn name(&self) -> &str {
                "codex-responses"
            }

            fn wire_for(&self, scenario: Scenario) -> Option<ProviderWire> {
                let wire = match scenario {
                    Scenario::PlainTextStop => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "hello" }]
                        }],
                        "usage": { "input_tokens": U::BASE_INPUT, "output_tokens": U::BASE_OUTPUT }
                    })),
                    Scenario::OutputCapped => ProviderWire::body(json!({
                        "status": "incomplete",
                        "incomplete_details": { "reason": "max_output_tokens" },
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "incomplete",
                            "content": [{ "type": "output_text", "text": "trunc" }]
                        }]
                    })),
                    Scenario::ContentFilter => ProviderWire::body(json!({
                        "status": "incomplete",
                        "incomplete_details": { "reason": "content_filter" },
                        "output": []
                    })),
                    Scenario::NonStreamingToolUse => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "function_call", "id": "fc_1", "call_id": "call_1",
                            "name": "lookup", "arguments": "{\"q\":\"x\"}", "status": "completed"
                        }]
                    })),
                    Scenario::StreamingTextAssembly => {
                        ProviderWire::body(json!({})).with_text_stream(
                            vec![
                                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"hello "}"#.to_string(),
                                r#"{"type":"response.output_text.delta","item_id":"msg_1","delta":"world"}"#.to_string(),
                            ],
                            "hello world",
                        )
                    }
                    Scenario::StreamingToolArgumentMerge => {
                        ProviderWire::body(json!({})).with_tool_call_stream(
                            vec![
                                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":""}}"#.to_string(),
                                // arguments deliberately split across two delta events
                                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"q\":"}"#.to_string(),
                                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"x\"}"}"#.to_string(),
                                r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}","status":"completed"}}"#.to_string(),
                            ],
                            "lookup",
                            json!({ "q": "x" }),
                        )
                    }
                    Scenario::UsageCacheHit => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "ok" }]
                        }],
                        "usage": {
                            "input_tokens": U::BASE_INPUT,
                            "output_tokens": U::BASE_OUTPUT,
                            "input_tokens_details": { "cached_tokens": U::CACHED_INPUT }
                        }
                    })),
                    Scenario::UsageReasoning => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [{
                            "type": "message", "id": "msg_1", "status": "completed",
                            "content": [{ "type": "output_text", "text": "ok" }]
                        }],
                        "usage": {
                            "input_tokens": U::BASE_INPUT,
                            "output_tokens": U::OUTPUT_WITH_REASONING,
                            "output_tokens_details": { "reasoning_tokens": U::REASONING }
                        }
                    })),
                    Scenario::ReasoningExtraction => ProviderWire::body(json!({
                        "status": "completed",
                        "output": [
                            {
                                "type": "reasoning", "id": "rs_1",
                                "summary": [{ "type": "summary_text", "text": "thinking about it" }]
                            },
                            {
                                "type": "message", "id": "msg_1", "status": "completed",
                                "content": [{ "type": "output_text", "text": "answer" }]
                            }
                        ]
                    }))
                    .with_reasoning_text("thinking about it"),
                    Scenario::StreamingUsageMerge => {
                        ProviderWire::body(json!({})).with_usage_merge_stream(vec![
                            // input arrives on an early event
                            format!(
                                r#"{{"type":"response.output_text.delta","delta":"hi","usage":{{"input_tokens":{}}}}}"#,
                                U::BASE_INPUT
                            ),
                            // output arrives on a later event; merge must keep input
                            format!(
                                r#"{{"type":"response.output_text.delta","delta":"!","usage":{{"output_tokens":{}}}}}"#,
                                U::BASE_OUTPUT
                            ),
                        ])
                    }
                };
                Some(wire)
            }

            fn parts_from_wire(&self, body: &Value) -> Vec<LlmOutputPart> {
                shared::response_parts_from_value(body)
            }

            fn usage_from_wire(&self, body: &Value) -> LlmUsage {
                openai_usage_from_response_value(body)
            }

            fn terminal_from_wire(
                &self,
                body: &Value,
                parts: &[LlmOutputPart],
            ) -> LlmTerminalReason {
                openai_terminal_reason_from_response_value(body, parts)
            }

            fn assemble_stream(&self, sse_events: &[String]) -> StreamAssembly {
                let mut state = shared::ResponsesStreamState::default();
                for raw in sse_events {
                    shared::process_sse_event(PROVIDER, raw, &mut state, None)
                        .expect("responses sse event parses");
                }
                StreamAssembly {
                    parts: state.response_parts(),
                    usage: state.usage.clone(),
                }
            }
        }

        #[test]
        fn codex_satisfies_provider_conformance() {
            provider_conformance(&CodexNormalizer);
        }
    }
}
