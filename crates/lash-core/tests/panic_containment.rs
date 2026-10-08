#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::facade_support::{
    LlmTransportError, Provider, ProviderComponents, ProviderHandle, ProviderOptions,
};
use lash_core::{GenerationOptions, LlmRequest, LlmRequestScope, LlmResponse, ProviderRequestBody};

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

    async fn send(
        &mut self,
        _request: LlmRequest,
        _body: &ProviderRequestBody,
    ) -> Result<LlmResponse, LlmTransportError> {
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

    async fn send(
        &mut self,
        _request: LlmRequest,
        _body: &ProviderRequestBody,
    ) -> Result<LlmResponse, LlmTransportError> {
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

#[derive(Clone, Debug)]
struct AuxiliaryPanicProvider {}

#[async_trait]
impl Provider for AuxiliaryPanicProvider {
    fn kind(&self) -> &'static str {
        "panic-provider"
    }
    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }
    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }
    fn set_options(&mut self, _: ProviderOptions) {}
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
    async fn close(&self) -> Result<(), LlmTransportError> {
        panic!("auxiliary close payload")
    }
    async fn send(
        &mut self,
        _: LlmRequest,
        _: &ProviderRequestBody,
    ) -> Result<LlmResponse, LlmTransportError> {
        let auxiliary = ProviderHandle::new(ProviderComponents::new(Box::new(self.clone())));
        auxiliary.close().await?;

        panic!("the panicking callback cannot succeed")
    }
}

/// A panic in an auxiliary callback is a typed failure when quiet and
/// unwinds when loud. The turn a provider seam panic stops is the facade's
/// law (`crates/lash/src/tests/panic_containment.rs`).
#[allow(
    clippy::disallowed_methods,
    reason = "isolated test processes own the process-scoped panic mode"
)]
#[tokio::test]
async fn provider_auxiliary_panics_are_typed_in_quiet_and_loud_modes() {
    use futures_util::FutureExt as _;
    const TEST: &str = "provider_auxiliary_panics_are_typed_in_quiet_and_loud_modes";
    let Ok(case) = std::env::var("LASH_AUXILIARY_PANIC_CASE") else {
        for case in ["quiet-close", "loud-close"] {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env("LASH_AUXILIARY_PANIC_CASE", case)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("isolated panic case has a bounded lifetime")
            .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "isolated case must actually run"
            );
        }
        return;
    };
    let loud = case.starts_with("loud");
    let expected_message = "auxiliary close payload";
    lash_core::panic_containment::set_loud(loud);
    let auxiliary =
        ProviderHandle::new(ProviderComponents::new(Box::new(AuxiliaryPanicProvider {})));
    let direct = std::panic::AssertUnwindSafe(async { auxiliary.close().await })
        .catch_unwind()
        .await;
    if loud {
        let payload = direct.expect_err("loud auxiliary panic propagates");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&expected_message));
    } else {
        let error = direct
            .expect("quiet containment does not unwind")
            .expect_err("typed callback failure");
        assert_eq!(
            error.code.as_ref().unwrap().to_string(),
            "lash:provider_panicked"
        );
        assert_eq!(error.message, expected_message);
        assert!(!error.is_retryable());
    }
}

/// Which callback of [`DesugaredPanicProvider`] panics before returning its
/// future.
#[derive(Clone, Copy, Debug)]
enum DesugaredPanicCallback {
    Complete,
    Close,
}

impl DesugaredPanicCallback {
    fn payload(self) -> &'static str {
        match self {
            Self::Complete => "desugared complete construction payload",
            Self::Close => "desugared close construction payload",
        }
    }

    /// Panics with a literal payload so the loud-mode path propagates the
    /// original `&'static str` value, not a formatted `String`.
    fn panic_at_construction(self) -> ! {
        match self {
            Self::Complete => panic!("desugared complete construction payload"),
            Self::Close => panic!("desugared close construction payload"),
        }
    }
}

/// A `Provider` written the way `#[async_trait]` desugars the trait: each
/// method's body runs when the handle invokes it (`Complete` is the
/// attempt's `send`), so this panic happens while
/// the call's boxed future is being constructed — before any future exists
/// for the unwind catcher's poll to cover.
#[derive(Clone, Debug)]
struct DesugaredPanicProvider {
    callback: DesugaredPanicCallback,
}

impl Provider for DesugaredPanicProvider {
    fn kind(&self) -> &'static str {
        "desugared-panic-provider"
    }
    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }
    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }
    fn set_options(&mut self, _: ProviderOptions) {}
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
    fn send<'life0, 'life1, 'async_trait>(
        &'life0 mut self,
        _: LlmRequest,
        _: &'life1 ProviderRequestBody,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<LlmResponse, LlmTransportError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let callback = self.callback;
        if matches!(callback, DesugaredPanicCallback::Complete) {
            callback.panic_at_construction();
        }
        Box::pin(async move {
            let auxiliary =
                ProviderHandle::new(ProviderComponents::new(Box::new(DesugaredPanicProvider {
                    callback,
                })));
            match callback {
                DesugaredPanicCallback::Close => auxiliary.close().await?,
                DesugaredPanicCallback::Complete => {
                    unreachable!("the complete arm panics before boxing")
                }
            }
            panic!("the panicking callback cannot succeed")
        })
    }
    fn close<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), LlmTransportError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        if matches!(self.callback, DesugaredPanicCallback::Close) {
            self.callback.panic_at_construction();
        }
        Box::pin(async { Ok(()) })
    }
}

/// A panic while a desugared callback builds its future is the same typed,
/// non-retryable failure when quiet and unwinds when loud.
#[allow(
    clippy::disallowed_methods,
    reason = "isolated test processes own the process-scoped panic mode"
)]
#[tokio::test]
async fn provider_desugared_construction_panics_are_typed_in_quiet_and_loud_modes() {
    use futures_util::FutureExt as _;
    const TEST: &str = "provider_desugared_construction_panics_are_typed_in_quiet_and_loud_modes";
    let Ok(case) = std::env::var("LASH_DESUGARED_PANIC_CASE") else {
        for case in [
            "quiet-complete",
            "loud-complete",
            "quiet-close",
            "loud-close",
        ] {
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
                    .env("LASH_DESUGARED_PANIC_CASE", case)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("isolated panic case has a bounded lifetime")
            .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "isolated case must actually run"
            );
        }
        return;
    };
    let loud = case.starts_with("loud");
    let callback = match case
        .strip_prefix("quiet-")
        .or_else(|| case.strip_prefix("loud-"))
    {
        Some("complete") => DesugaredPanicCallback::Complete,
        Some("close") => DesugaredPanicCallback::Close,
        other => panic!("unknown desugared panic case {other:?}"),
    };
    let expected_message = callback.payload();
    lash_core::panic_containment::set_loud(loud);
    let mut auxiliary =
        ProviderHandle::new(ProviderComponents::new(Box::new(DesugaredPanicProvider {
            callback,
        })));
    let mut complete_call_record = None;
    let direct = std::panic::AssertUnwindSafe(async {
        match callback {
            DesugaredPanicCallback::Complete => auxiliary
                .complete(request())
                .await
                .map(|_| ())
                .map_err(|failure| {
                    complete_call_record = Some(failure.call_record);
                    failure.error
                }),
            DesugaredPanicCallback::Close => auxiliary.close().await,
        }
    })
    .catch_unwind()
    .await;
    if loud {
        let payload = direct.expect_err("loud construction panic propagates");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&expected_message));
    } else {
        let error = direct
            .expect("quiet containment does not unwind")
            .expect_err("typed callback failure");
        assert_eq!(
            error.code.as_ref().unwrap().to_string(),
            "lash:provider_panicked"
        );
        assert_eq!(error.message, expected_message);
        assert!(!error.is_retryable());
        if let Some(record) = complete_call_record {
            assert_eq!(
                record.attempts.len(),
                1,
                "a construction panic cannot retry"
            );
            assert_eq!(
                record.attempts[0]
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.decline_cause()),
                Some(lash_sansio::llm::types::RetryDeclineCause::NotRetryable)
            );
        }
    }
}
