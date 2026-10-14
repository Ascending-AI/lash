//! A provider feature alone must close its transport signatures through Lash.
//! Compile each feature independently; workspace feature unification is not proof.

#[cfg(any(feature = "openai", feature = "anthropic", feature = "google"))]
mod transport {
    use std::sync::Arc;
    use std::time::Duration;

    use lash::http_transport::{HttpRequest, HttpResponse, HttpTransport, LlmTransportError};

    #[derive(Debug)]
    struct HostTransport;

    #[lash::async_trait]
    impl HttpTransport for HostTransport {
        async fn send(
            &self,
            _request: HttpRequest,
            _timeout: Option<Duration>,
        ) -> Result<HttpResponse, LlmTransportError> {
            std::future::pending().await
        }
    }

    pub(super) fn host_transport() -> Arc<dyn HttpTransport> {
        Arc::new(HostTransport)
    }
}

#[cfg(feature = "openai")]
pub fn openai_transport() {
    let _ = lash::openai::OpenAiProvider::new("compile-only")
        .with_transport(transport::host_transport());
    let _ = lash::openai::OpenAiCompatibleProvider::new("compile-only", "https://provider.invalid")
        .with_transport(transport::host_transport());
}

#[cfg(feature = "anthropic")]
pub fn anthropic_transport() {
    let _ = lash::anthropic::AnthropicProvider::new("compile-only")
        .with_transport(transport::host_transport());
}

#[cfg(feature = "google")]
pub fn google_transport() {
    let tokens = std::sync::Arc::new(lash::provider::ProviderToken::new("compile-only"));
    let _ =
        lash::google::GoogleOAuthProvider::new(tokens).with_transport(transport::host_transport());
}
