//! Provider construction: the [`GoogleOAuthProvider`] struct, its builders,
//! endpoint-URL helpers, the uploaded-attachment cache types, and the
//! [`GoogleOAuthProviderFactory`].

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

#[derive(Debug)]
struct GoogleCredentialRefresher;

#[async_trait]
impl CredentialRefresher<GoogleCredential> for GoogleCredentialRefresher {
    async fn refresh(
        &self,
        current: &GoogleCredential,
        _cause: RefreshCause,
    ) -> Result<GoogleCredential, CredentialError> {
        let tokens = crate::oauth::refresh_tokens(&current.refresh_token)
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
    LlmTransportError::new(error.to_string())
        .with_kind(lash_core::ProviderFailureKind::Auth)
        .with_code(code)
        .retryable(error.retryable)
}

/// Google OAuth (Gemini via Code Assist) provider.
#[derive(Clone, Debug)]
pub struct GoogleOAuthProvider {
    pub(crate) credentials: Arc<CredentialManager<GoogleCredential>>,
    pub(crate) attempt_credential: Option<Lease<GoogleCredential>>,
    pub project_id: Option<String>,
    pub options: ProviderOptions,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl GoogleOAuthProvider {
    pub(crate) fn uploaded_attachment_cache()
    -> &'static tokio::sync::Mutex<HashMap<UploadedAttachmentCacheKey, UploadedAttachmentRef>> {
        static CACHE: OnceLock<
            tokio::sync::Mutex<HashMap<UploadedAttachmentCacheKey, UploadedAttachmentRef>>,
        > = OnceLock::new();
        CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
    }

    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: u64,
    ) -> Self {
        let credential = GoogleCredential {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at,
        };
        Self {
            credentials: Arc::new(CredentialManager::new(
                credential,
                Arc::new(GoogleCredentialRefresher),
            )),
            attempt_credential: None,
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

    pub(crate) fn endpoint_base_url() -> String {
        let endpoint = std::env::var("CODE_ASSIST_ENDPOINT")
            .unwrap_or_else(|_| CODE_ASSIST_ENDPOINT.to_string());
        let version = std::env::var("CODE_ASSIST_API_VERSION")
            .unwrap_or_else(|_| CODE_ASSIST_API_VERSION.to_string());
        format!("{endpoint}/{version}")
    }

    pub(crate) fn method_url(method: &str) -> String {
        format!("{}:{}", Self::endpoint_base_url(), method)
    }

    pub(crate) fn route_identity_for_model(model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(Self::PROVIDER_KIND, &Self::endpoint_base_url(), model)
    }

    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

#[derive(Deserialize)]
struct GoogleProviderConfig {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    options: ProviderOptions,
    #[serde(default = "default_stream_termination")]
    stream_termination: StreamTermination,
}

fn default_stream_termination() -> StreamTermination {
    StreamTermination::EofTolerated
}

pub struct GoogleOAuthProviderFactory;

impl ProviderFactory for GoogleOAuthProviderFactory {
    fn kind(&self) -> &'static str {
        "google_oauth"
    }
    fn deserialize(&self, config: serde_json::Value) -> Result<ProviderComponents, String> {
        let cfg: GoogleProviderConfig =
            serde_json::from_value(config).map_err(|err| err.to_string())?;
        Ok(GoogleOAuthProvider {
            project_id: cfg.project_id,
            options: cfg.options,
            stream_termination: cfg.stream_termination,
            ..GoogleOAuthProvider::new(cfg.access_token, cfg.refresh_token, cfg.expires_at)
        }
        .into_components())
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn invalid_grant_maps_to_visible_non_retryable_auth_transport_error() {
        let error = credential_transport_error(CredentialError::invalid_grant());
        assert_eq!(error.kind, lash_core::ProviderFailureKind::Auth);
        assert_eq!(error.code.as_deref(), Some("credential_invalid_grant"));
        assert!(!error.retryable);
        assert!(error.message.contains("sign in again"));
    }
}
