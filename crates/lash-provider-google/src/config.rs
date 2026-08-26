//! Provider construction: the [`GoogleOAuthProvider`] struct, its builders,
//! endpoint-URL helpers, and the uploaded-attachment cache types.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash_provider_auth::{
    Credential, CredentialManager, CredentialRefresher, RefreshCause, classify_oauth_refresh_error,
};

use crate::support::*;

pub(crate) const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const CODE_ASSIST_API_VERSION: &str = "v1internal";

pub(crate) static DEFAULT_HTTP_TRANSPORT: LazyLock<Arc<dyn LlmHttpTransport>> =
    LazyLock::new(|| Arc::new(ReqwestLlmHttpTransport::new()));

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct UploadedAttachmentCacheKey {
    pub(crate) provider: &'static str,
    pub(crate) credential_scope: String,
    pub(crate) mime: String,
    pub(crate) hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UploadedAttachmentRef {
    pub(crate) uri: String,
}

#[derive(Clone)]
pub(crate) struct GoogleCredential {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_at: u64,
}

impl std::fmt::Debug for GoogleCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleCredential")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl std::fmt::Display for GoogleCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GoogleCredential([REDACTED])")
    }
}

impl Credential for GoogleCredential {
    fn expires_at(&self) -> Option<SystemTime> {
        (self.expires_at != 0)
            .then(|| UNIX_EPOCH.checked_add(Duration::from_secs(self.expires_at)))
            .flatten()
    }
}

/// OAuth application credentials registered by the host with Google.
///
/// Construct this with named fields so the client ID and secret cannot be
/// confused with the provider's access and refresh tokens.
#[derive(Clone, Deserialize)]
pub struct GoogleOAuthClient {
    #[serde(rename = "oauth_client_id")]
    /// OAuth client ID issued for the host's Google application.
    pub id: String,
    #[serde(rename = "oauth_client_secret")]
    /// OAuth client secret issued for the host's Google application.
    pub secret: String,
}

impl std::fmt::Debug for GoogleOAuthClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleOAuthClient")
            .field("id", &self.id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug)]
struct GoogleCredentialRefresher {
    oauth_client: GoogleOAuthClient,
}

#[async_trait]
impl CredentialRefresher<GoogleCredential> for GoogleCredentialRefresher {
    async fn refresh(
        &self,
        current: &GoogleCredential,
        _cause: RefreshCause,
    ) -> Result<GoogleCredential, CredentialError> {
        let tokens = crate::oauth::refresh_tokens(&self.oauth_client, &current.refresh_token)
            .await
            .map_err(classify_oauth_refresh_error)?;
        Ok(GoogleCredential {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }
}

pub(crate) fn credential_transport_error(error: CredentialError) -> LlmTransportError {
    let code = match error.kind {
        CredentialErrorKind::InvalidGrant => "credential_invalid_grant",
        CredentialErrorKind::Transient => "credential_refresh_transient",
        CredentialErrorKind::Other => "credential_refresh_failed",
    };
    let retry_verdict = if error.retryable {
        TransportRetryVerdict::RetryableTransient
    } else {
        TransportRetryVerdict::Forbidden
    };
    let failure_kind = if error.retryable {
        lash_core::ProviderFailureKind::Transport
    } else {
        lash_core::ProviderFailureKind::Auth
    };
    LlmTransportError::new(error.to_string())
        .with_kind(failure_kind)
        .with_code(code)
        .with_retry_verdict(retry_verdict)
}

/// Google OAuth (Gemini via Code Assist) provider.
#[derive(Clone, Debug)]
pub struct GoogleOAuthProvider {
    pub(crate) credentials: Arc<CredentialManager<GoogleCredential>>,
    pub(crate) attempt_credential: Option<Lease<GoogleCredential>>,
    pub(crate) oauth_client: GoogleOAuthClient,
    pub(crate) endpoint: String,
    pub(crate) api_version: String,
    pub project_id: Option<String>,
    pub options: ProviderOptions,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl GoogleOAuthProvider {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(
            "access",
            "refresh",
            0,
            GoogleOAuthClient {
                id: "oauth-client-id".to_string(),
                secret: "oauth-client-secret".to_string(),
            },
        )
    }

    pub(crate) fn uploaded_attachment_cache()
    -> &'static tokio::sync::Mutex<HashMap<UploadedAttachmentCacheKey, UploadedAttachmentRef>> {
        static CACHE: OnceLock<
            tokio::sync::Mutex<HashMap<UploadedAttachmentCacheKey, UploadedAttachmentRef>>,
        > = OnceLock::new();
        CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
    }

    /// Construct a provider from current tokens and the host's named Google
    /// OAuth application credentials.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: u64,
        oauth_client: GoogleOAuthClient,
    ) -> Self {
        let credential = GoogleCredential {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at,
        };
        Self {
            credentials: Arc::new(CredentialManager::new(
                credential,
                Arc::new(GoogleCredentialRefresher {
                    oauth_client: oauth_client.clone(),
                }),
            )),
            attempt_credential: None,
            oauth_client,
            endpoint: CODE_ASSIST_ENDPOINT.to_string(),
            api_version: CODE_ASSIST_API_VERSION.to_string(),
            project_id: None,
            options: ProviderOptions::default(),
            stream_termination: StreamTermination::EofTolerated,
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
mod credential_tests {
    use super::*;
    use lash_core::provider::{DefaultProviderFailureClassifier, ProviderFailureClassifier};

    #[test]
    fn credential_failures_preserve_transient_and_permission_retry_verdicts() {
        let transient = DefaultProviderFailureClassifier
            .classify(credential_transport_error(CredentialError::transient()));
        assert_eq!(transient.kind, lash_core::ProviderFailureKind::Transport);
        assert_eq!(
            transient.code.as_deref(),
            Some("credential_refresh_transient")
        );
        assert_eq!(
            transient.retry_verdict,
            TransportRetryVerdict::RetryableTransient
        );

        let permission = DefaultProviderFailureClassifier
            .classify(credential_transport_error(CredentialError::invalid_grant()));
        assert_eq!(permission.kind, lash_core::ProviderFailureKind::Auth);
        assert_eq!(permission.code.as_deref(), Some("credential_invalid_grant"));
        assert_eq!(permission.retry_verdict, TransportRetryVerdict::Forbidden);
        assert!(permission.message.contains("sign in again"));
    }

    #[test]
    fn google_endpoint_and_api_version_default_and_override_explicitly() {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            0,
            GoogleOAuthClient {
                id: "oauth-client-id".to_string(),
                secret: "oauth-client-secret".to_string(),
            },
        );
        assert_eq!(
            provider.endpoint_base_url(),
            "https://cloudcode-pa.googleapis.com/v1internal"
        );

        let provider = provider
            .with_endpoint("  https://code-assist.example///  ")
            .with_api_version("  v2  ");
        assert_eq!(
            provider.method_url("generateContent"),
            "https://code-assist.example/v2:generateContent"
        );
        assert_eq!(
            provider
                .route_identity_for_model("gemini-test")
                .endpoint
                .as_ref(),
            "https://code-assist.example/v2"
        );
    }

    #[test]
    #[should_panic(expected = "Google endpoint must not be empty")]
    fn google_endpoint_rejects_empty_values() {
        GoogleOAuthProvider::for_test().with_endpoint(" / ");
    }

    #[test]
    #[should_panic(expected = "Google API version must not be empty")]
    fn google_api_version_rejects_empty_values() {
        GoogleOAuthProvider::for_test().with_api_version("   ");
    }

    #[test]
    fn google_oauth_client_secret_is_redacted_from_debug_output() {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            0,
            GoogleOAuthClient {
                id: "oauth-client-id".to_string(),
                secret: "oauth-client-secret-sentinel".to_string(),
            },
        );
        let debug = format!("{provider:?}");
        assert!(debug.contains("oauth-client-id"));
        assert!(!debug.contains("oauth-client-secret-sentinel"));
    }

    #[test]
    fn google_serialized_config_carries_explicit_oauth_and_endpoint_configuration() {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            0,
            GoogleOAuthClient {
                id: "oauth-client-id".to_string(),
                secret: "oauth-client-secret".to_string(),
            },
        )
        .with_endpoint("https://code-assist.example")
        .with_api_version("v2");

        let config = provider.serialize_config();
        assert_eq!(config["oauth_client_id"], "oauth-client-id");
        assert_eq!(config["oauth_client_secret"], "oauth-client-secret");
        assert_eq!(config["endpoint"], "https://code-assist.example");
        assert_eq!(config["api_version"], "v2");
    }
}
