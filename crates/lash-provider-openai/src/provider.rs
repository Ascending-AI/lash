use crate::support::*;

impl OpenAiCompatibleProvider {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            options: ProviderOptions::default(),
            compat: OpenAiCompat::default(),
            wire: OpenAiWireConfig::default(),
            transport: DEFAULT_HTTP_TRANSPORT.clone(),
            responses_resume: None,
        }
    }

    pub fn with_options(mut self, options: ProviderOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_compat(mut self, compat: OpenAiCompat) -> Self {
        self.compat = compat;
        self
    }

    pub fn with_wire_config(mut self, wire: OpenAiWireConfig) -> Self {
        self.wire = wire;
        self
    }

    pub fn with_reasoning_format(mut self, format: ReasoningWireFormat) -> Self {
        self.compat.reasoning_format = Some(format);
        self
    }

    pub fn with_schema_capabilities(mut self, capabilities: ProviderSchemaCapabilities) -> Self {
        self.compat.schema_capabilities = Some(capabilities);
        self
    }

    pub fn with_transport(mut self, transport: std::sync::Arc<dyn LlmHttpTransport>) -> Self {
        self.transport = transport;
        self
    }

    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        let compat = OpenAiCompat {
            prompt_cache_key: Some(true),
            prompt_cache_retention: Some(true),
            ..OpenAiCompat::default()
        };
        Self {
            inner: OpenAiCompatibleProvider::new(api_key, OPENAI_BASE_URL).with_compat(compat),
        }
    }

    pub fn with_options(mut self, options: ProviderOptions) -> Self {
        self.inner.options = options;
        self
    }

    pub fn with_transport(mut self, transport: std::sync::Arc<dyn LlmHttpTransport>) -> Self {
        self.inner.transport = transport;
        self
    }

    pub fn into_components(self) -> ProviderComponents {
        ProviderComponents::new(Box::new(self))
    }

    #[cfg(test)]
    pub(crate) fn build_responses_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        self.inner.build_responses_request_body_for_route(
            req,
            stream,
            &self.route_identity(&req.model),
        )
    }
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn kind(&self) -> &'static str {
        "openai-compatible"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(self.kind(), &self.base_url, model)
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "api_key".to_string(),
            serde_json::Value::String(self.api_key.clone()),
        );
        map.insert(
            "base_url".to_string(),
            serde_json::Value::String(self.base_url.clone()),
        );
        if !self.options.is_default() {
            map.insert(
                "options".to_string(),
                serde_json::to_value(&self.options).unwrap_or(serde_json::Value::Null),
            );
        }
        if self.compat != OpenAiCompat::default() {
            map.insert(
                "compat".to_string(),
                serde_json::to_value(&self.compat).unwrap_or(serde_json::Value::Null),
            );
        }
        if self.wire != OpenAiWireConfig::default() {
            map.insert(
                "wire".to_string(),
                serde_json::to_value(&self.wire).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(map)
    }

    async fn complete(&mut self, req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        complete(self, req, CompletionEndpoint::ChatCompletions).await
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn kind(&self) -> &'static str {
        "openai"
    }

    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::for_endpoint(self.kind(), &self.inner.base_url, model)
    }

    fn options(&self) -> ProviderOptions {
        self.inner.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "api_key".to_string(),
            serde_json::Value::String(self.inner.api_key.clone()),
        );
        if !self.inner.options.is_default() {
            map.insert(
                "options".to_string(),
                serde_json::to_value(&self.inner.options).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(map)
    }

    async fn complete(&mut self, req: LlmRequest) -> Result<LlmResponse, LlmTransportError> {
        complete(&mut self.inner, req, CompletionEndpoint::Responses).await
    }

    fn generation_retry_guarantee(&self, request: &LlmRequest) -> GenerationRetryGuarantee {
        self.inner
            .responses_resume
            .as_ref()
            .filter(|resume| {
                resume.request_key.request_id == request.scope.request_id
                    && responses_request_fingerprint(&self.inner, request)
                        .is_some_and(|fingerprint| fingerprint == resume.request_key.fingerprint)
            })
            .map_or(GenerationRetryGuarantee::None, |_| {
                GenerationRetryGuarantee::Resumable
            })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
