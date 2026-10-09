use lash_core::facade_support::LlmTransportError;
use lash_core::provider::{Provider, ProviderComponents, ProviderHandle, ProviderOptions};
use lash_core::{LlmRequest, LlmResponse, ProviderRouteIdentity, RecordedRequestTemplate};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug)]
struct PendingTransport {
    entered: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for PendingTransport {
    fn kind(&self) -> &'static str {
        "host-admission-fixture"
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), "host-route", model)
    }
    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }
    fn set_options(&mut self, _: ProviderOptions) {}
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({"endpoint": "host-route"})
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
    async fn send(
        &mut self,
        _body: &lash_sansio::llm::types::LiveRequestBody,
        _: lash_sansio::llm::types::ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        self.entered.notify_one();
        std::future::pending().await
    }
    async fn close(&self) -> Result<(), LlmTransportError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct HostAdmission {
    inner: Box<dyn Provider>,
    permits: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl Provider for HostAdmission {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        self.inner.route_identity(model)
    }
    fn options(&self) -> ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> serde_json::Value {
        self.inner.serialize_config()
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
            permits: self.permits.clone(),
        })
    }
    async fn close(&self) -> Result<(), LlmTransportError> {
        self.inner.close().await
    }
    async fn lower(
        &mut self,
        request: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        self.inner.lower(request).await
    }
    async fn send(
        &mut self,
        body: &lash_sansio::llm::types::LiveRequestBody,
        context: lash_sansio::llm::types::ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("fixture keeps admission open");
        self.inner.send(body, context).await
    }
}

#[tokio::test]
async fn host_admission_permit_releases_on_cancellation_and_forwards_close() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let closed = Arc::new(AtomicUsize::new(0));
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let make_handle = || {
        ProviderHandle::new(
            ProviderComponents::new(Box::new(PendingTransport {
                entered: entered.clone(),
                closed: closed.clone(),
            }))
            .map_provider({
                let permits = permits.clone();
                move |inner| Box::new(HostAdmission { inner, permits })
            }),
        )
    };
    let mut first = make_handle();
    let mut second = make_handle();
    let request = LlmRequest {
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("request-fixture"),
                lash_sansio::llm_profile::LlmProfileMetadata::builder("fixture-model")
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(lash_sansio::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        scope: lash_core::LlmRequestScope::new("tenant", "frame", "request"),
        instructions: None,
        messages: vec![],

        tools: Arc::new(vec![]),
        tool_choice: lash_core::llm::types::LlmToolChoice::None,
        attachment_acceptance: Default::default(),
        generation: Default::default(),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    };
    let mut first_call = Box::pin(first.complete(
        request.clone(),
        lash_core::ChargeSafetyPolicy::RequireGuarantee,
        lash_core::ExecutionBudgets::recommended(),
        &lash_core::provider::NoSlotDeliveries,
    ));
    tokio::select! {
        _ = entered.notified() => {},
        _ = &mut first_call => panic!("fixture transport must remain pending"),
    }
    assert_eq!(permits.available_permits(), 0);
    let mut second_call = Box::pin(second.complete(
        request.clone(),
        lash_core::ChargeSafetyPolicy::RequireGuarantee,
        lash_core::ExecutionBudgets::recommended(),
        &lash_core::provider::NoSlotDeliveries,
    ));
    assert!(futures_util::poll!(&mut second_call).is_pending());
    assert_eq!(closed.load(Ordering::SeqCst), 0);
    drop(second_call);
    assert_eq!(
        permits.available_permits(),
        0,
        "cancelling a queued call owns no permit"
    );
    drop(first_call);
    assert_eq!(
        permits.available_permits(),
        1,
        "cancelling inside transport releases its permit"
    );
    let mut next_request = request;
    next_request.scope.request_id = "next-request".into();
    let mut next_call = Box::pin(second.complete(
        next_request,
        lash_core::ChargeSafetyPolicy::RequireGuarantee,
        lash_core::ExecutionBudgets::recommended(),
        &lash_core::provider::NoSlotDeliveries,
    ));
    tokio::select! {
        _ = entered.notified() => {},
        _ = &mut next_call => panic!("fixture transport must remain pending"),
    }
    assert_eq!(permits.available_permits(), 0);
    drop(next_call);
    assert_eq!(permits.available_permits(), 1);
    first.close().await.unwrap();
    second.close().await.unwrap();
    assert_eq!(
        closed.load(Ordering::SeqCst),
        2,
        "each explicit close reaches the inner provider"
    );
}
