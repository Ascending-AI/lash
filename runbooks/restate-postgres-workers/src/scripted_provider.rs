use lash::provider::{
    LlmRequest, LlmResponse, LlmTransportError, Provider, ProviderComponents, ProviderHandle,
    ProviderOptions,
};
use std::{future::Future, pin::Pin, sync::Arc};

type CompletionFuture =
    Pin<Box<dyn Future<Output = Result<LlmResponse, LlmTransportError>> + Send>>;
type CompletionFn = dyn Fn(LlmRequest) -> CompletionFuture + Send + Sync;
type SerializeConfigFn = dyn Fn() -> serde_json::Value + Send + Sync;

fn empty_provider_config() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

/// Scripted provider used by the distributed worker scenarios.
#[derive(Clone)]
pub struct ScriptedProvider {
    kind: &'static str,
    requires_streaming: bool,
    generation_retry_guarantee: lash_core::provider::GenerationRetryGuarantee,
    options: ProviderOptions,
    serialize_config: Arc<SerializeConfigFn>,
    complete: Arc<CompletionFn>,
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedProvider")
            .field("kind", &self.kind)
            .field("requires_streaming", &self.requires_streaming)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Default for ScriptedProvider {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ScriptedProvider {
    pub fn builder() -> ScriptedProviderBuilder {
        ScriptedProviderBuilder::new()
    }

    pub fn into_handle(self) -> ProviderHandle {
        ProviderHandle::new(ProviderComponents::new(Box::new(self)))
    }
}

pub struct ScriptedProviderBuilder {
    provider: ScriptedProvider,
}

impl ScriptedProviderBuilder {
    pub fn new() -> Self {
        Self {
            provider: ScriptedProvider {
                kind: "test",
                requires_streaming: false,
                generation_retry_guarantee: lash_core::provider::GenerationRetryGuarantee::None,
                options: ProviderOptions::default(),
                serialize_config: Arc::new(empty_provider_config),
                complete: Arc::new(|_request| {
                    Box::pin(async {
                        Err(LlmTransportError::new(
                            "ScriptedProvider::complete was called without a test completion handler",
                        ))
                    })
                }),
            },
        }
    }

    pub fn kind(mut self, kind: &'static str) -> Self {
        self.provider.kind = kind;
        self
    }

    pub fn requires_streaming(mut self, requires_streaming: bool) -> Self {
        self.provider.requires_streaming = requires_streaming;
        self
    }

    pub fn options(mut self, options: ProviderOptions) -> Self {
        self.provider.options = options;
        self
    }

    pub fn serialize_config<F>(mut self, serialize_config: F) -> Self
    where
        F: Fn() -> serde_json::Value + Send + Sync + 'static,
    {
        self.provider.serialize_config = Arc::new(serialize_config);
        self
    }

    pub fn complete<F, Fut>(mut self, complete: F) -> Self
    where
        F: Fn(LlmRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<LlmResponse, LlmTransportError>> + Send + 'static,
    {
        self.provider.complete = Arc::new(move |request| Box::pin(complete(request)));
        self
    }

    pub fn complete_error(mut self, message: impl Into<String>) -> Self {
        let message = Arc::new(message.into());
        self.provider.complete = Arc::new(move |_request| {
            let message = Arc::clone(&message);
            Box::pin(async move { Err(LlmTransportError::new(message.as_str())) })
        });
        self
    }

    pub fn build(self) -> ScriptedProvider {
        self.provider
    }
}

impl Default for ScriptedProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        (self.serialize_config)()
    }

    async fn complete(&mut self, request: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        (self.complete)(request).await
    }

    fn generation_retry_guarantee(
        &self,
        _request: &LlmRequest,
    ) -> lash_core::provider::GenerationRetryGuarantee {
        self.generation_retry_guarantee
    }

    fn requires_streaming(&self) -> bool {
        self.requires_streaming
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
