//! Provider construction: the [`AnthropicProvider`] struct and its builders.

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
