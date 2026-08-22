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

use crate::common::{DEFAULT_HTTP_TRANSPORT, DEFAULT_MAX_OUTPUT_TOKENS, reasoning_intent};
use crate::reasoning::ReasoningWireIntent;
use crate::responses_shared as shared;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    GenerationOptionOutcome, GenerationReceipt, LlmOutputSpec, LlmRequest,
};
use lash_core::provider::{
    CacheRetention, Provider, ProviderComponents, ProviderOptions, ProviderReliability,
    resolve_generation_policy,
};
use lash_core::{facade_support::ProviderSchemaCapabilities, facade_support::SchemaPurpose};
use lash_llm_transport::LlmHttpTransport;
use lash_provider_auth::{CredentialManager, Lease};

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

    /// Which of the caller's generation options a Codex request carries: none
    /// of them. The Responses dialect Codex speaks has no seed field, and this
    /// adapter sends neither a temperature nor a token cap, for the same
    /// reason it leaves the rest of the sampling surface to the endpoint.
    fn generation_disposition(req: &LlmRequest, body: &Value) -> GenerationReceipt {
        GenerationReceipt {
            output_token_cap: GenerationOptionOutcome::unsupported(
                req.generation.output_token_cap.is_some(),
            ),
            temperature: GenerationOptionOutcome::unsupported(req.generation.temperature.is_some()),
            seed: GenerationOptionOutcome::unsupported(req.generation.seed.is_some()),
            stop_sequences: GenerationOptionOutcome::unsupported(
                !req.generation.stop_sequences.is_empty(),
            ),
            cache: lash_llm_transport::cache_intent_disposition(req, Some(body)),
        }
    }

    pub(crate) fn build_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        let safe_request = req.replay_safe_for(&serving_route);
        let req = safe_request.as_ref();
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
}

impl CodexProvider {
    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
            .with_failure_classifier(std::sync::Arc::new(CodexFailureClassifier))
    }
}

#[cfg(test)]
mod tests;
