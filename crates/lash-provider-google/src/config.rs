//! Provider construction: the [`GoogleOAuthProvider`] struct, its builders,
//! endpoint-URL helpers, and host-owned file scope configuration.

use std::sync::{Arc, LazyLock, OnceLock};

use crate::support::*;
#[cfg(test)]
use lash_core::provider::ProviderToken;

pub(crate) const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const CODE_ASSIST_API_VERSION: &str = "v1internal";

pub(crate) static DEFAULT_HTTP_TRANSPORT: LazyLock<Arc<dyn LlmHttpTransport>> =
    LazyLock::new(|| Arc::new(ReqwestLlmHttpTransport::new()));

/// Google (Gemini via Code Assist) provider. The host owns the Google OAuth
/// login, refresh and client registration, and supplies access tokens through
/// a [`TokenSource`].
#[derive(Clone, Debug)]
pub struct GoogleOAuthProvider {
    pub(crate) tokens: Arc<TokenGate>,
    pub(crate) endpoint: String,
    pub(crate) api_version: String,
    pub project_id: Option<String>,
    /// The project Code Assist resolved for an unconfigured provider, shared
    /// by every clone of it. Lash runs each model call on its own copy of the
    /// provider and hands nothing back, so a copy's own `project_id` is gone
    /// after its call; this keeps the lookup to once per provider.
    pub(crate) resolved_project_id: Arc<OnceLock<String>>,
    pub options: ProviderOptions,
    pub(crate) attachment_credential_scope: Option<String>,
    pub extra_headers: lash_llm_transport::ExtraHeaders,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl GoogleOAuthProvider {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(Arc::new(ProviderToken::new("access")))
    }

    /// A provider that asks `tokens` for an access token before every attempt.
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self {
            tokens: Arc::new(TokenGate::new(tokens, Self::PROVIDER_KIND)),
            attachment_credential_scope: None,
            endpoint: CODE_ASSIST_ENDPOINT.to_string(),
            api_version: CODE_ASSIST_API_VERSION.to_string(),
            project_id: None,
            resolved_project_id: Arc::new(OnceLock::new()),
            options: ProviderOptions::default(),
            extra_headers: Default::default(),
            stream_termination: StreamTermination::default(),
            transport: Arc::clone(&DEFAULT_HTTP_TRANSPORT),
        }
    }

    pub fn with_project_id(mut self, project_id: Option<String>) -> Self {
        self.project_id = project_id;
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        let endpoint = endpoint.trim().trim_end_matches('/');
        assert!(!endpoint.is_empty(), "Google endpoint must not be empty");
        self.endpoint = endpoint.to_string();
        self
    }

    pub fn with_api_version(mut self, api_version: impl Into<String>) -> Self {
        let api_version = api_version.into();
        let api_version = api_version.trim();
        assert!(
            !api_version.is_empty(),
            "Google API version must not be empty"
        );
        self.api_version = api_version.to_string();
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

    pub fn with_client(mut self, client: std::sync::Arc<reqwest::Client>) -> Self {
        self.transport = Arc::new(ReqwestLlmHttpTransport::from_client((*client).clone()));
        self
    }

    pub(crate) fn endpoint_base_url(&self) -> String {
        format!("{}/{}", self.endpoint, self.api_version)
    }

    pub(crate) fn method_url(&self, method: &str) -> String {
        format!("{}:{method}", self.endpoint_base_url())
    }

    pub(crate) fn route_identity_for_model(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(Self::PROVIDER_KIND, &self.endpoint_base_url(), model)
    }

    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn google_tokens_never_reach_debug_output_or_serialized_config() {
        let provider =
            GoogleOAuthProvider::new(Arc::new(ProviderToken::new("google-access-sentinel")));
        let debug = format!("{provider:?}");
        assert!(!debug.contains("google-access-sentinel"), "leaked: {debug}");
        assert!(debug.contains("[redacted]"));
        let config = provider.serialize_config().to_string();
        assert!(
            !config.contains("google-access-sentinel"),
            "leaked: {config}"
        );
    }
}
