//! Provider construction: the [`AnthropicProvider`] struct and its builders.

use std::sync::{Arc, LazyLock};

use crate::support::*;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

pub(crate) static DEFAULT_HTTP_TRANSPORT: LazyLock<Arc<dyn LlmHttpTransport>> =
    LazyLock::new(|| Arc::new(ReqwestLlmHttpTransport::new()));

/// The host chooses where Anthropic receives its token. Token acquisition
/// and replacement always remain with the host's `TokenSource`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AnthropicAuthScheme {
    #[default]
    ApiKey,
    Bearer,
}

/// Anthropic API (Claude) provider state and transport.
///
/// Streamed messages accept at most 1,024 content blocks. Starts must use
/// consecutive indices beginning at zero; malformed indices end the stream
/// without retry.
#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    /// The host's token source behind its gate. A token's plaintext leaves
    /// the process only on the selected credential header.
    pub(crate) tokens: Arc<TokenGate>,
    pub auth_scheme: AnthropicAuthScheme,
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    pub(crate) attachment_credential_scope: Option<String>,
    pub extra_headers: lash_llm_transport::ExtraHeaders,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl AnthropicProvider {
    /// A provider that sends the fixed `api_key`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_token_source(Arc::new(ProviderToken::new(api_key.into())))
    }

    /// A provider that asks `tokens` for a token before every attempt.
    pub fn with_token_source(tokens: Arc<dyn TokenSource>) -> Self {
        Self {
            tokens: Arc::new(TokenGate::new(tokens, "anthropic")),
            attachment_credential_scope: None,
            auth_scheme: AnthropicAuthScheme::default(),
            base_url: None,
            options: ProviderOptions::default(),
            extra_headers: Default::default(),
            stream_termination: StreamTermination::default(),
            transport: Arc::clone(&DEFAULT_HTTP_TRANSPORT),
        }
    }

    pub fn with_auth_scheme(mut self, scheme: AnthropicAuthScheme) -> Self {
        self.auth_scheme = scheme;
        self
    }

    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        self.base_url = base_url;
        self
    }

    /// Bind uploaded file ids to a host-owned, non-secret credential identity.
    pub fn with_attachment_credential_scope(mut self, scope: impl Into<String>) -> Self {
        self.attachment_credential_scope = Some(scope.into());
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

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn anthropic_api_key_is_redacted_from_debug_output() {
        let provider = AnthropicProvider::new("sk-ant-secret-sentinel");
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-ant-secret-sentinel"), "leaked: {debug}");
        assert!(debug.contains("[redacted]"));
        assert!(
            !provider
                .serialize_config()
                .to_string()
                .contains("sk-ant-secret-sentinel")
        );
    }
}
