use crate::support::*;

impl OpenAiCompatibleProvider {
    /// A provider that sends the fixed `api_key`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::with_token_source(Arc::new(ProviderToken::new(api_key.into())), base_url)
    }

    /// A provider that asks `tokens` for a token before every attempt.
    /// Uses [`ProviderOptions::standard`] and [`lash_llm_transport::TokenPolicy::standard`];
    /// both policies can be configured before use.
    pub fn with_token_source(tokens: Arc<dyn TokenSource>, base_url: impl Into<String>) -> Self {
        Self::with_gate(tokens, "openai-compatible", base_url)
    }

    /// `kind` names the provider to the token source, as `Provider::kind` does.
    fn with_gate(
        tokens: Arc<dyn TokenSource>,
        kind: &'static str,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            tokens: Arc::new(TokenGate::new(tokens, kind)),
            base_url: base_url.into(),
            options: ProviderOptions::standard(),
            request_work: crate::RequestWorkPolicy::standard(),
            attachment_credential_scope: None,
            compat: OpenAiCompat::default(),
            wire: OpenAiWireConfig::default(),
            transport: DEFAULT_HTTP_TRANSPORT.clone(),
            responses_resume: None,
        }
    }

    /// Configure proactive renewal before this provider is used.
    pub fn with_token_policy(mut self, policy: lash_llm_transport::TokenPolicy) -> Self {
        self.tokens = Arc::new(self.tokens.configured(policy));
        self
    }

    pub fn with_request_work_policy(mut self, policy: crate::RequestWorkPolicy) -> Self {
        self.request_work = policy;
        self
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

    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.wire.extra_headers = headers.into();
        self
    }

    pub fn with_reasoning_dialect(mut self, dialect: OpenAiReasoningDialect) -> Self {
        self.compat.reasoning = Some(dialect);
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
    /// A provider that sends the fixed `api_key`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_token_source(Arc::new(ProviderToken::new(api_key.into())))
    }

    /// A provider that asks `tokens` for a token before every attempt.
    /// Uses [`ProviderOptions::standard`] and [`lash_llm_transport::TokenPolicy::standard`];
    /// both policies can be configured before use.
    pub fn with_token_source(tokens: Arc<dyn TokenSource>) -> Self {
        let compat = OpenAiCompat {
            reasoning: Some(OpenAiReasoningDialect::OpenAi),
            prompt_cache_key: Some(true),
            prompt_cache_retention: Some(true),
            ..OpenAiCompat::default()
        };
        Self {
            inner: OpenAiCompatibleProvider::with_gate(tokens, "openai", OPENAI_BASE_URL)
                .with_compat(compat),
        }
    }

    /// Bind the Files namespace to a host-owned, non-secret credential identity.
    pub fn with_attachment_credential_scope(mut self, scope: impl Into<String>) -> Self {
        self.inner.attachment_credential_scope = Some(scope.into());
        self
    }

    /// Configure proactive renewal before this provider is used.
    pub fn with_token_policy(mut self, policy: lash_llm_transport::TokenPolicy) -> Self {
        self.inner = self.inner.with_token_policy(policy);
        self
    }

    pub fn with_request_work_policy(mut self, policy: crate::RequestWorkPolicy) -> Self {
        self.inner.request_work = policy;
        self
    }

    pub fn with_options(mut self, options: ProviderOptions) -> Self {
        self.inner.options = options;
        self
    }

    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.inner.wire.extra_headers = headers.into();
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
        self.build_responses_request(req, stream)
            .map(|built| built.body)
    }

    #[cfg(test)]
    pub(crate) fn build_responses_request(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<BuiltRequest, LlmTransportError> {
        self.inner.build_responses_request_for_route(
            req,
            stream,
            &self.route_identity(req.model.wire_model()),
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

    fn attachment_accepts(
        &self,
        _model: &str,
        mime: &lash_sansio::MediaType,
        position: AttachmentPosition,
    ) -> ProviderAccepts {
        if position != AttachmentPosition::Message || !crate::attachment_delivery::image(mime) {
            return ProviderAccepts::NONE;
        }
        ProviderAccepts {
            bytes: true,
            url: true,
            provider_file: None,
        }
    }
    fn encode_slot(
        &self,
        slot: &AttachmentSlot,
        delivery: &Delivery,
    ) -> Result<TransientJson, LlmTransportError> {
        crate::attachment_delivery::encode(
            slot,
            delivery,
            crate::attachment_delivery::CHAT_CODEC,
            None,
        )
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
        let mut durable_wire = self.wire.clone();
        durable_wire.extra_headers = Default::default();
        if durable_wire != OpenAiWireConfig::default() {
            map.insert(
                "wire".to_string(),
                serde_json::to_value(&durable_wire).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(map)
    }

    async fn lower(
        &mut self,
        req: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        lower(self, req, CompletionEndpoint::ChatCompletions).await
    }

    async fn send(
        &mut self,
        body: &LiveRequestBody,
        context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        send(self, body, context, CompletionEndpoint::ChatCompletions).await
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

    fn attachment_file_scope(&self) -> Option<ProviderFileScope> {
        self.inner
            .attachment_credential_scope
            .as_ref()
            .map(|scope| ProviderFileScope {
                provider: self.kind().into(),
                endpoint: self.route_identity("").endpoint.into(),
                credential_scope: scope.clone(),
            })
    }
    fn attachment_accepts(
        &self,
        _model: &str,
        mime: &lash_sansio::MediaType,
        _position: AttachmentPosition,
    ) -> ProviderAccepts {
        crate::attachment_delivery::responses_accepts(mime, self.attachment_file_scope())
    }
    fn encode_slot(
        &self,
        slot: &AttachmentSlot,
        delivery: &Delivery,
    ) -> Result<TransientJson, LlmTransportError> {
        crate::attachment_delivery::encode(
            slot,
            delivery,
            crate::attachment_delivery::RESPONSES_CODEC,
            self.attachment_file_scope(),
        )
    }
    fn options(&self) -> ProviderOptions {
        self.inner.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.inner.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if !self.inner.options.is_default() {
            map.insert(
                "options".to_string(),
                serde_json::to_value(&self.inner.options).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(map)
    }

    async fn lower(
        &mut self,
        req: &LlmRequest,
    ) -> Result<RecordedRequestTemplate, LlmTransportError> {
        lower(&self.inner, req, CompletionEndpoint::Responses).await
    }

    async fn send(
        &mut self,
        body: &LiveRequestBody,
        context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        send(
            &mut self.inner,
            body,
            context,
            CompletionEndpoint::Responses,
        )
        .await
    }

    fn generation_retry_guarantee(
        &self,
        context: &ResponseContext,
        body: &RecordedRequestTemplate,
    ) -> GenerationRetryGuarantee {
        if body.slots().next().is_some() {
            return GenerationRetryGuarantee::None;
        }
        self.inner
            .responses_resume
            .as_ref()
            .filter(|resume| {
                resume.request_key.request_id == context.scope.request_id
                    && responses_request_fingerprint(body) == resume.request_key.fingerprint
            })
            .map_or(GenerationRetryGuarantee::None, |_| {
                GenerationRetryGuarantee::Resumable
            })
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn openai_api_keys_are_redacted_from_debug_output() {
        let compatible =
            OpenAiCompatibleProvider::new("sk-oai-secret-sentinel", "https://example.test");
        let debug = format!("{compatible:?}");
        assert!(!debug.contains("sk-oai-secret-sentinel"), "leaked: {debug}");
        assert!(debug.contains("[redacted]"));

        assert!(
            !compatible
                .serialize_config()
                .to_string()
                .contains("sk-oai-secret-sentinel")
        );

        let openai = OpenAiProvider::new("sk-oai-secret-sentinel");
        let debug = format!("{openai:?}");
        assert!(!debug.contains("sk-oai-secret-sentinel"), "leaked: {debug}");
        assert!(
            !openai
                .serialize_config()
                .to_string()
                .contains("sk-oai-secret-sentinel")
        );
    }
}
