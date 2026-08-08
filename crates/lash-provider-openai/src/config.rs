use crate::support::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCompatMaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
    Omit,
}

/// Gateway provider-routing preferences, emitted as the request's top-level
/// `provider` object. Endpoints that do not implement provider routing must
/// leave this unset.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderRoutingPrefs {
    /// Route only to upstream backends that support every parameter in the
    /// request. Gateways otherwise silently drop parameters an upstream does
    /// not implement — a dropped `response_format` yields free-written JSON
    /// under a nominal `finish_reason: "stop"`, so the structured-output
    /// contract fails only at parse time.
    pub require_parameters: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiCompat {
    /// Clean-EOF policy for this endpoint. Model capability data, when set on
    /// a request, takes precedence over this endpoint default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_termination: Option<StreamTermination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fields: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<OpenAiCompatMaxTokensField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_format: Option<ReasoningWireFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_session_affinity: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming_usage: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_capabilities: Option<ProviderSchemaCapabilities>,
    /// Provider-routing preferences for gateway endpoints. Emitted whenever
    /// set, not only for schema'd requests: any parameter this adapter sends
    /// is one the caller relies on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_routing: Option<ProviderRoutingPrefs>,
}

impl OpenAiCompat {
    /// Explicit endpoint capabilities for local OpenAI-compatible servers.
    pub fn local() -> Self {
        Self {
            request_fields: Some(false),
            store: Some(false),
            streaming_usage: Some(false),
            ..Self::default()
        }
    }

    /// Explicit endpoint capabilities for OpenRouter and compatible proxies.
    ///
    /// Carries facts about the endpoint's wire dialect, not preferences about
    /// how to use it. `provider_routing` is deliberately absent: restricting
    /// the routing pool trades cost, latency and availability against contract
    /// enforcement, and that trade belongs to the host.
    pub fn openrouter() -> Self {
        Self {
            reasoning_format: Some(ReasoningWireFormat::openrouter()),
            cache_session_affinity: Some(true),
            stream_termination: Some(StreamTermination::RequireTerminalEvidence),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenAiResolvedCompat {
    pub(crate) stream_termination: StreamTermination,
    pub(crate) request_fields: bool,
    pub(crate) max_tokens_field: OpenAiCompatMaxTokensField,
    pub(crate) reasoning_format: ReasoningWireFormat,
    pub(crate) cache_session_affinity: bool,
    pub(crate) prompt_cache_key: bool,
    pub(crate) prompt_cache_retention: bool,
    pub(crate) strict_tools: bool,
    pub(crate) store: bool,
    pub(crate) streaming_usage: bool,
    pub(crate) schema_capabilities: ProviderSchemaCapabilities,
    pub(crate) provider_routing: Option<ProviderRoutingPrefs>,
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleProvider {
    pub api_key: String,
    pub base_url: String,
    pub options: ProviderOptions,
    pub compat: OpenAiCompat,
    pub(crate) transport: std::sync::Arc<dyn LlmHttpTransport>,
}

#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    pub(crate) inner: OpenAiCompatibleProvider,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn resolved_compat(&self, endpoint: CompletionEndpoint) -> OpenAiResolvedCompat {
        // ADR 0037 ratifies this exact direct-OpenAI equality as the sole
        // URL-derived compatibility choice.
        let direct_openai = self.base_url.trim_end_matches('/') == OPENAI_BASE_URL;

        let max_tokens_field = match endpoint {
            CompletionEndpoint::Responses => OpenAiCompatMaxTokensField::MaxOutputTokens,
            CompletionEndpoint::ChatCompletions => OpenAiCompatMaxTokensField::MaxTokens,
        };
        let reasoning_format = match endpoint {
            CompletionEndpoint::Responses if direct_openai => ReasoningWireFormat::openai(),
            _ => ReasoningWireFormat::none(),
        };
        let defaults = OpenAiResolvedCompat {
            stream_termination: StreamTermination::RequireTerminalEvidence,
            request_fields: true,
            max_tokens_field,
            reasoning_format,
            cache_session_affinity: false,
            prompt_cache_key: false,
            prompt_cache_retention: false,
            strict_tools: false,
            store: true,
            streaming_usage: true,
            schema_capabilities: ProviderSchemaCapabilities::openai(false),
            provider_routing: None,
        };
        let strict_tools = self.compat.strict_tools.unwrap_or(defaults.strict_tools);
        OpenAiResolvedCompat {
            stream_termination: self
                .compat
                .stream_termination
                .unwrap_or(defaults.stream_termination),
            request_fields: self
                .compat
                .request_fields
                .unwrap_or(defaults.request_fields),
            max_tokens_field: self
                .compat
                .max_tokens_field
                .unwrap_or(defaults.max_tokens_field),
            reasoning_format: self
                .compat
                .reasoning_format
                .clone()
                .unwrap_or(defaults.reasoning_format),
            cache_session_affinity: self
                .compat
                .cache_session_affinity
                .unwrap_or(defaults.cache_session_affinity),
            prompt_cache_key: self
                .compat
                .prompt_cache_key
                .unwrap_or(defaults.prompt_cache_key),
            prompt_cache_retention: self
                .compat
                .prompt_cache_retention
                .unwrap_or(defaults.prompt_cache_retention),
            strict_tools,
            store: self.compat.store.unwrap_or(defaults.store),
            streaming_usage: self
                .compat
                .streaming_usage
                .unwrap_or(defaults.streaming_usage),
            schema_capabilities: self
                .compat
                .schema_capabilities
                .clone()
                .unwrap_or_else(|| ProviderSchemaCapabilities::openai(strict_tools)),
            provider_routing: self
                .compat
                .provider_routing
                .clone()
                .or(defaults.provider_routing),
        }
    }
}
