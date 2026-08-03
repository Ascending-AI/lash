//! Provider construction: the [`AnthropicProvider`] struct, its builders, and
//! the [`AnthropicProviderFactory`] that materializes one from a stored config.

use std::sync::{Arc, LazyLock};

use crate::support::*;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub(crate) static DEFAULT_HTTP_TRANSPORT: LazyLock<Arc<dyn LlmHttpTransport>> =
    LazyLock::new(|| Arc::new(ReqwestLlmHttpTransport::new()));

/// Anthropic API (Claude) provider state and transport.
#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: None,
            options: ProviderOptions::default(),
            stream_termination: StreamTermination::RequireTerminalEvidence,
            transport: Arc::clone(&DEFAULT_HTTP_TRANSPORT),
        }
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.base_url = base_url;
        self
    }

    pub fn with_options(mut self, options: ProviderOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_stream_termination(mut self, policy: StreamTermination) -> Self {
        self.stream_termination = policy;
        self
    }

    pub fn with_transport(mut self, transport: Arc<dyn LlmHttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Share an embedder-provided `reqwest::Client` instead of building
    /// a fresh one. Saves ~42 MB of TLS state per provider when the
    /// host pools connections across sessions.
    pub fn with_client(self, client: Arc<reqwest::Client>) -> Self {
        self.with_transport(Arc::new(ReqwestLlmHttpTransport::from_client(
            (*client).clone(),
        )))
    }

    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

/// Deserialize payload for `ProviderSpec::config` when building an
/// `AnthropicProvider` from a stored [`lash_core::LashConfig`].
#[derive(Deserialize)]
struct AnthropicProviderConfig {
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    options: ProviderOptions,
    #[serde(default = "default_stream_termination")]
    stream_termination: StreamTermination,
}

fn default_stream_termination() -> StreamTermination {
    StreamTermination::RequireTerminalEvidence
}

/// Factory that materializes [`AnthropicProvider`] from a host-owned
/// [`ProviderSpec`](lash_core::facade_support::ProviderSpec).
pub struct AnthropicProviderFactory;

impl ProviderFactory for AnthropicProviderFactory {
    fn kind(&self) -> &'static str {
        "anthropic"
    }
    fn deserialize(&self, config: serde_json::Value) -> Result<ProviderComponents, String> {
        let cfg: AnthropicProviderConfig =
            serde_json::from_value(config).map_err(|err| err.to_string())?;
        Ok(AnthropicProvider {
            api_key: cfg.api_key,
            base_url: cfg.base_url,
            options: cfg.options,
            stream_termination: cfg.stream_termination,
            transport: Arc::clone(&DEFAULT_HTTP_TRANSPORT),
        }
        .into_components())
    }
}
