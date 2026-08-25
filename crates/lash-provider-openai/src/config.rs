use crate::support::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCompatMaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
    Omit,
}

/// Endpoint-owned HTTP authentication and static request query parameters.
///
/// The default preserves the conventional OpenAI wire shape:
/// `Authorization: Bearer <api key>` with no added query parameters.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiWireConfig {
    pub auth_header_name: String,
    pub auth_value_prefix: String,
    pub query_params: Vec<(String, String)>,
}

impl Default for OpenAiWireConfig {
    fn default() -> Self {
        Self {
            auth_header_name: "Authorization".to_string(),
            auth_value_prefix: "Bearer ".to_string(),
            query_params: Vec::new(),
        }
    }
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
    pub developer_role: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_capabilities: Option<ProviderSchemaCapabilities>,
    /// Response header names (case-insensitive) to capture into
    /// `LlmResponse.response_metadata` as `header:<lowercased-name>` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata_headers: Option<Vec<String>>,
    /// JSON pointers probed against response bodies (buffered: final body;
    /// streaming: every SSE event, last seen value wins), captured as
    /// `body:<pointer>` entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_metadata_body_paths: Option<Vec<String>>,
}

impl OpenAiCompat {
    /// Explicit endpoint capabilities for OpenRouter and compatible proxies.
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
    pub(crate) developer_role: bool,
    pub(crate) schema_capabilities: ProviderSchemaCapabilities,
    pub(crate) response_metadata_headers: Vec<String>,
    pub(crate) response_metadata_body_paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OpenAiCompatibleProvider {
    pub api_key: String,
    pub base_url: String,
    pub options: ProviderOptions,
    pub compat: OpenAiCompat,
    pub wire: OpenAiWireConfig,
    pub(crate) transport: std::sync::Arc<dyn LlmHttpTransport>,
}

#[derive(Clone, Debug)]
pub struct OpenAiProvider {
    pub(crate) inner: OpenAiCompatibleProvider,
}

impl OpenAiCompatibleProvider {
    pub(crate) fn local_style_base_url(base_url: &str) -> bool {
        let normalized = base_url.trim().to_ascii_lowercase();
        normalized.contains("localhost")
            || normalized.contains("127.0.0.1")
            || normalized.contains("0.0.0.0")
            || normalized.contains("ollama")
    }

    pub(crate) fn resolved_compat(&self, endpoint: CompletionEndpoint) -> OpenAiResolvedCompat {
        let local = Self::local_style_base_url(&self.base_url);
        let direct_openai = self.base_url.trim_end_matches('/') == OPENAI_BASE_URL;

        let request_fields = !local;
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
            request_fields,
            max_tokens_field,
            reasoning_format,
            cache_session_affinity: false,
            prompt_cache_key: false,
            prompt_cache_retention: false,
            strict_tools: false,
            store: !local,
            streaming_usage: !local,
            developer_role: direct_openai,
            schema_capabilities: ProviderSchemaCapabilities::openai(false),
            response_metadata_headers: Vec::new(),
            response_metadata_body_paths: Vec::new(),
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
            developer_role: self
                .compat
                .developer_role
                .unwrap_or(defaults.developer_role),
            schema_capabilities: self
                .compat
                .schema_capabilities
                .clone()
                .unwrap_or_else(|| ProviderSchemaCapabilities::openai(strict_tools)),
            response_metadata_headers: self
                .compat
                .response_metadata_headers
                .clone()
                .unwrap_or(defaults.response_metadata_headers),
            response_metadata_body_paths: self
                .compat
                .response_metadata_body_paths
                .clone()
                .unwrap_or(defaults.response_metadata_body_paths),
        }
    }
}
