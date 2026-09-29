//! OpenAI Codex OAuth provider (ChatGPT Plus/Pro/Team via device-code flow).
//!
//! [`CodexProvider`] is the facade: this file owns the provider's shape
//! (construction, configuration, the Codex-specific request body) and
//! delegates the rest to modules that each own one concern —
//! `credential` for OAuth material and refresh, `session` for the WebSocket
//! session cache and its leases, `continuation` for cached-context planning,
//! `streaming` for driving a response over either transport, and `failure` for
//! Codex error classification.

mod continuation;
mod credential;
mod failure;
pub mod oauth;
mod session;
mod streaming;
#[cfg(any(test, feature = "testing"))]
pub mod ws_testing;

use std::sync::Arc;

use serde_json::{Value, json};

use crate::common::{BuiltRequest, DEFAULT_HTTP_TRANSPORT, reasoning_retention_transport_error};
use crate::config::OpenAiReasoningDialect;
use crate::driver::CompletionEndpoint;
use crate::reasoning::{apply_reasoning, reasoning_object};
use crate::responses_shared as shared;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmOutputSpec, LlmRequest, ProviderReasoningRetentionSupport, ReasoningRetentionSelection,
};
use lash_core::provider::{
    CacheRetention, GenerationEmission, GenerationWire, OutputCapWire, Provider,
    ProviderComponents, ProviderOptions, ProviderReliability, ResolvedGenerationPolicy,
    ThinkingSummaryWire, resolve_generation_policy,
};
use lash_core::{facade_support::ProviderSchemaCapabilities, facade_support::SchemaPurpose};
use lash_llm_transport::{
    LlmHttpTransport, merge_extra_body, reserved_generation_paths, validate_extra_headers,
};
use lash_provider_auth::CredentialManager;
use lash_sansio::Redacted;

use credential::{CodexCredential, CodexCredentialRefresher};
use failure::CodexFailureClassifier;
use session::CodexWebsocketSessionCache;

/// Provider name used in shared-machinery error messages and trace events.
const PROVIDER: &str = "Codex";

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

/// OpenAI Codex OAuth provider (ChatGPT Plus/Pro/Team via device-code flow).
///
/// Codex speaks the OpenAI Responses streaming protocol, so the request/stream
/// machinery is shared verbatim from [`crate::responses_shared`].
/// This module owns only the Codex-specific surface: the
/// `chatgpt.com/backend-api/codex/responses` endpoint, the `codex_cli_rs`
/// originator/User-Agent headers, the explicit `instructions` request shape, and
/// Codex error/quota classification.
#[derive(Clone, Debug)]
pub struct CodexProvider {
    credentials: Arc<CredentialManager<CodexCredential>>,
    pub options: ProviderOptions,
    pub extra_headers: lash_llm_transport::ExtraHeaders,
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
            access_token: Redacted::new(access_token),
            refresh_token: Redacted::new(refresh_token),
            expires_at,
            account_id: None,
        };
        Self {
            credentials: Arc::new(CredentialManager::new(
                credential,
                Arc::new(CodexCredentialRefresher),
            )),
            options: ProviderOptions {
                reliability: ProviderReliability::codex(),
                ..ProviderOptions::default()
            },
            extra_headers: Default::default(),
            transport: CodexTransport::Auto,
            websocket_sessions: CodexWebsocketSessionCache::default(),
            responses_url: Self::CODEX_RESPONSES_URL.to_string(),
            websocket_url: Self::CODEX_RESPONSES_WS_URL.to_string(),
            http_transport: DEFAULT_HTTP_TRANSPORT.clone(),
        }
    }

    pub fn with_account_id(mut self, account_id: Option<String>) -> Self {
        let mut credential = self.credentials.snapshot();
        credential.account_id = account_id.map(Redacted::new);
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

    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers.into();
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
    /// into provider config, so tests can point a provider instance at local
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

    /// What the Codex Responses wire, as this adapter speaks it, can carry.
    /// Codex takes no cap, sampling controls or stop sequences; a host that
    /// sets one is refused rather than silently ignored.
    const GENERATION_WIRE: GenerationWire = GenerationWire {
        label: "OpenAI Codex",
        output_token_cap: OutputCapWire::Unsupported,
        temperature: false,
        seed: false,
        stop_sequences: false,
        parallel_tool_calls: true,
        thinking_summary: ThinkingSummaryWire::Always,
        active_thinking_pins_sampling: false,
    };

    /// Refuse, before any credential, WebSocket or HTTP I/O, every host
    /// setting this request carries that the Codex wire cannot send.
    pub(crate) fn preflight(&self, req: &LlmRequest) -> Result<(), LlmTransportError> {
        validate_extra_headers(
            &self.extra_headers,
            &[
                "authorization",
                "content-type",
                "accept",
                "openai-beta",
                "originator",
                "user-agent",
                "session-id",
                "x-client-request-id",
                "chatgpt-account-id",
            ],
            false,
        )?;
        self.build_request(req, req.stream_events.is_some())
            .map(|_| ())
    }

    /// Run `then` over the retention-safe request with every host setting
    /// resolved. Pure, so the builder and the preflight agree.
    fn validated<T>(
        &self,
        req: &LlmRequest,
        then: impl FnOnce(&LlmRequest, ResolvedGenerationPolicy, Value) -> Result<T, LlmTransportError>,
    ) -> Result<T, LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        let safe_request = req
            .reasoning_retention_safe_for(
                &serving_route,
                "OpenAI Codex",
                ProviderReasoningRetentionSupport::OpenAiContext,
            )
            .map_err(reasoning_retention_transport_error)?;
        let req = safe_request.as_ref();
        shared::validate_responses_attachments(req, "OpenAI Codex")?;
        let policy =
            resolve_generation_policy(req, &self.options, self.kind(), &Self::GENERATION_WIRE)?;
        // Codex is Responses in the OpenAI reasoning dialect.
        let mut reasoning_body = json!({});
        if let Some(intent) = &policy.reasoning {
            apply_reasoning(
                CompletionEndpoint::Responses,
                Some(OpenAiReasoningDialect::OpenAi),
                intent,
                &mut reasoning_body,
            )?;
        }
        then(req, policy, reasoning_body)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        self.build_request(req, stream).map(|built| built.body)
    }

    pub(crate) fn build_request(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<BuiltRequest, LlmTransportError> {
        self.validated(req, |req, policy, reasoning_body| {
            self.build_validated(req, stream, policy, reasoning_body)
        })
    }

    fn build_validated(
        &self,
        req: &LlmRequest,
        stream: bool,
        policy: ResolvedGenerationPolicy,
        reasoning_body: Value,
    ) -> Result<BuiltRequest, LlmTransportError> {
        let tools = Self::build_tools(req)?;
        let input = shared::build_responses_input(req);
        let mut emission = GenerationEmission {
            reasoning: policy.reasoning.is_some(),
            ..GenerationEmission::default()
        };
        // `store:false` and the encrypted reasoning include are replay
        // mechanics of this stateless wire, not host settings.
        let mut body = json!({
            "model": req.model,
            "input": input,
            "tools": tools,
            "stream": stream,
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });
        body["instructions"] = json!(req.instructions.as_deref().unwrap_or(""));
        if let Some(parallel_tool_calls) = policy.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel_tool_calls);
            emission.parallel_tool_calls = true;
        }
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
        if let Value::Object(fields) = reasoning_body {
            for (key, value) in fields {
                body[key] = value;
            }
        }
        if let ReasoningRetentionSelection::OpenAiContext { context } =
            req.model_capability.reasoning_retention.selection
        {
            reasoning_object(&mut body)["context"] = json!(context.as_str());
            emission.reasoning_retention = true;
        }
        if policy.request_thinking_summary {
            reasoning_object(&mut body)["summary"] = json!("auto");
            emission.thinking_summary = true;
        }
        emission.cache = policy.cache_retention != CacheRetention::None;
        if emission.cache {
            body["prompt_cache_key"] = json!(req.provider_prompt_cache_key());
        }
        if let Some(output_spec) = &req.output_spec {
            let format = match output_spec {
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
            body["text"] = json!({ "format": format });
        }
        let passthrough = merge_extra_body(
            &mut body,
            &req.extra_body,
            &reserved_generation_paths(req, "/stop", "/temperature"),
        )?;
        let mut receipt = policy.receipt(req, &emission);
        receipt.passthrough = if !self.extra_headers.is_empty() {
            lash_core::GenerationOptionOutcome::Applied
        } else {
            passthrough
        };
        Ok(BuiltRequest { body, receipt })
    }
}

impl CodexProvider {
    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
            .with_failure_classifier(std::sync::Arc::new(CodexFailureClassifier))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn codex_tokens_are_redacted_from_debug_output() {
        let provider = CodexProvider::new("codex-access-sentinel", "codex-refresh-sentinel", 7)
            .with_account_id(Some("codex-account-sentinel".to_string()));
        let debug = format!("{provider:?}");
        assert!(!debug.contains("codex-access-sentinel"), "leaked: {debug}");
        assert!(!debug.contains("codex-refresh-sentinel"), "leaked: {debug}");
        assert!(!debug.contains("codex-account-sentinel"), "leaked: {debug}");
        assert!(debug.contains("[redacted]"));

        let tokens = oauth::CodexTokens {
            access_token: Redacted::new("codex-access-sentinel"),
            refresh_token: Redacted::new("codex-refresh-sentinel"),
            expires_at: 7,
            account_id: Some(Redacted::new("codex-account-sentinel")),
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("sentinel"), "leaked: {debug}");
    }
}
