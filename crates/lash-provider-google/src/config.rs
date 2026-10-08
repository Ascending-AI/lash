//! Provider construction: the [`GoogleOAuthProvider`] struct, its builders,
//! endpoint-URL helpers, and the uploaded-attachment cache types.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use crate::support::*;
#[cfg(test)]
use lash_core::provider::ProviderToken;

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

/// Gemini Files deletes uploads server-side after 48 hours; entries expire
/// well before that so a dead URI is never served from this cache.
const UPLOADED_ATTACHMENT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Process-wide bound on distinct (provider, credential, project, MIME,
/// content) upload entries retained; oldest insertion is evicted past it.
const UPLOADED_ATTACHMENT_CACHE_CAPACITY: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UploadedAttachmentRef {
    pub(crate) uri: String,
    pub(crate) uploaded_at: Instant,
}

/// Bounded, expiring cache of uploaded Gemini Files URIs. Entries expire after
/// [`UPLOADED_ATTACHMENT_TTL`] and are also removed explicitly when a request
/// proves a cached URI dead (the provider's inline-retry path).
#[derive(Debug, Default)]
pub(crate) struct UploadedAttachmentCache {
    entries: HashMap<UploadedAttachmentCacheKey, UploadedAttachmentRef>,
}

impl UploadedAttachmentCache {
    pub(crate) fn get(
        &mut self,
        key: &UploadedAttachmentCacheKey,
        now: Instant,
    ) -> Option<UploadedAttachmentRef> {
        match self.entries.get(key) {
            Some(entry) if now.duration_since(entry.uploaded_at) < UPLOADED_ATTACHMENT_TTL => {
                Some(entry.clone())
            }
            Some(_) => {
                self.entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub(crate) fn insert(
        &mut self,
        key: UploadedAttachmentCacheKey,
        entry: UploadedAttachmentRef,
        now: Instant,
    ) {
        self.entries.retain(|_, existing| {
            now.duration_since(existing.uploaded_at) < UPLOADED_ATTACHMENT_TTL
        });
        if self.entries.len() >= UPLOADED_ATTACHMENT_CACHE_CAPACITY
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, existing)| existing.uploaded_at)
                .map(|(oldest, _)| oldest.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, entry);
    }

    /// Forget every upload whose URI is one of `uris`: the API rejected a
    /// body that named them.
    pub(crate) fn remove_uris(&mut self, uris: &[String]) {
        self.entries.retain(|_, entry| !uris.contains(&entry.uri));
    }
}

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
    pub extra_headers: lash_llm_transport::ExtraHeaders,
    pub stream_termination: StreamTermination,
    pub(crate) transport: Arc<dyn LlmHttpTransport>,
}

impl GoogleOAuthProvider {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(Arc::new(ProviderToken::new("access")))
    }

    pub(crate) fn uploaded_attachment_cache() -> &'static tokio::sync::Mutex<UploadedAttachmentCache>
    {
        static CACHE: OnceLock<tokio::sync::Mutex<UploadedAttachmentCache>> = OnceLock::new();
        CACHE.get_or_init(|| tokio::sync::Mutex::new(UploadedAttachmentCache::default()))
    }

    /// A provider that asks `tokens` for an access token before every attempt.
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self {
            tokens: Arc::new(TokenGate::new(tokens, Self::PROVIDER_KIND)),
            endpoint: CODE_ASSIST_ENDPOINT.to_string(),
            api_version: CODE_ASSIST_API_VERSION.to_string(),
            project_id: None,
            resolved_project_id: Arc::new(OnceLock::new()),
            options: ProviderOptions::default(),
            extra_headers: Default::default(),
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
