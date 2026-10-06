#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::facade_support::{
    LlmTransportError, Provider, ProviderComponents, ProviderHandle, ProviderOptions,
};
use lash_core::{GenerationOptions, LlmRequest, LlmRequestScope, LlmResponse};

static PANIC_MODE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug)]
struct PanicProvider;

#[async_trait]
impl Provider for PanicProvider {
    fn kind(&self) -> &'static str {
        "panic-provider"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        panic!("provider payload only")
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct ClassifierKeywordPanicProvider;

#[async_trait]
impl Provider for ClassifierKeywordPanicProvider {
    fn kind(&self) -> &'static str {
        "classifier-keyword-panic"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }

    fn set_options(&mut self, _options: ProviderOptions) {}

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn complete(&mut self, _request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        panic!("safety context length does not exist")
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder(
                    "panic-test-model".to_string(),
                )
                .context_window_tokens(128_000)
                .capability(Default::default())
                .extra_body(Default::default())
                .request_defaults(Default::default())
                .build()
                .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: Default::default(),
        attachment_acceptance: Default::default(),
        generation: GenerationOptions::default(),
        scope: LlmRequestScope::new("panic-test", "panic-test:frame", "panic-test:request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

#[tokio::test]
async fn provider_panic_is_typed_and_non_retryable() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let mut provider = ProviderHandle::new(ProviderComponents::new(Box::new(PanicProvider)));
    let failure = provider
        .complete(request())
        .await
        .expect_err("typed failure");

    assert_eq!(
        failure.error.code.as_ref().map(|code| code.to_string()),
        Some("lash:provider_panicked".to_string())
    );
    assert_eq!(failure.error.message, "provider payload only");
    assert!(!failure.error.is_retryable());
    assert_eq!(failure.call_record.attempts.len(), 1);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.decline_cause()),
        Some(lash_sansio::llm::types::RetryDeclineCause::NotRetryable)
    );
}

#[tokio::test]
async fn manufactured_provider_panic_bypasses_text_classification() {
    let _mode = PANIC_MODE.lock().await;
    lash_core::panic_containment::set_loud(false);
    let mut provider = ProviderHandle::new(ProviderComponents::new(Box::new(
        ClassifierKeywordPanicProvider,
    )));
    let failure = provider
        .complete(request())
        .await
        .expect_err("typed failure");

    assert_eq!(
        failure.error.code.as_ref().map(|code| code.to_string()),
        Some("lash:provider_panicked".to_string())
    );
    assert_eq!(failure.error.kind, lash_core::ProviderFailureKind::Unknown);
    assert!(!failure.error.is_retryable());
}
