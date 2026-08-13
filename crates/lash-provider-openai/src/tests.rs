use crate::support::*;
use crate::{OpenAiCompatibleProviderFactory, OpenAiProviderFactory};
use lash_core::llm::transport::ProviderFailureKind;
use lash_core::llm::types::{LlmJsonSchema, LlmMessage, LlmToolChoice, LlmToolSpec};
use lash_core::provider::{
    CacheControlDialect, CacheRetention, ModelCapability, ProviderHandle, ProviderReliability,
    ReasoningCapability, ReasoningEncoding,
};
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
mod attachment_tests;
#[cfg(feature = "testing")]
mod conformance;
mod generation_tests;
mod output_started_tests;

type ScriptedHttpResponse = (u16, Vec<(String, String)>, &'static str);

#[derive(Debug)]
struct ScriptedHttpTransport {
    responses: std::sync::Mutex<VecDeque<ScriptedHttpResponse>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedHttpTransport {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmHttpTransport for ScriptedHttpTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (status, headers, body) = self
            .responses
            .lock_recover()
            .pop_front()
            .expect("scripted response");
        Ok(lash_llm_transport::LlmHttpResponse {
            status,
            headers,
            body: LlmHttpBody::buffered(body),
        })
    }
}

const DEFAULT_RECORDING_RESPONSE_BODY: &str = r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]}"#;

#[derive(Debug)]
struct RecordingHttpTransport {
    requests: std::sync::Mutex<Vec<LlmHttpRequest>>,
    response_headers: Vec<(String, String)>,
    response_body: &'static str,
}

impl Default for RecordingHttpTransport {
    fn default() -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
            response_headers: Vec::new(),
            response_body: DEFAULT_RECORDING_RESPONSE_BODY,
        }
    }
}

impl RecordingHttpTransport {
    fn responding_with(
        response_headers: Vec<(String, String)>,
        response_body: &'static str,
    ) -> Self {
        Self {
            requests: std::sync::Mutex::new(Vec::new()),
            response_headers,
            response_body,
        }
    }
}

fn openrouter_provider() -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
        .with_compat(OpenAiCompat::openrouter())
}

#[test]
fn openrouter_preset_requires_terminal_evidence() {
    assert_eq!(
        OpenAiCompat::openrouter().stream_termination,
        Some(StreamTermination::RequireTerminalEvidence)
    );
    assert_eq!(
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
            .resolved_compat(CompletionEndpoint::ChatCompletions)
            .stream_termination,
        StreamTermination::RequireTerminalEvidence
    );
}

#[test]
fn local_compat_preset_has_exact_local_field_set() {
    assert_eq!(
        OpenAiCompat::local(),
        OpenAiCompat {
            request_fields: Some(false),
            store: Some(false),
            streaming_usage: Some(false),
            ..OpenAiCompat::default()
        }
    );
}

#[test]
fn developer_role_compat_field_is_rejected() {
    let error = serde_json::from_value::<OpenAiCompat>(json!({ "developer_role": true }))
        .expect_err("developer_role must be rejected as an unknown field");

    assert!(error.to_string().contains("unknown field `developer_role`"));
}

fn enable_cache_control(req: &mut LlmRequest, dialect: CacheControlDialect) {
    req.model_capability.cache_control = Some(dialect);
}

#[async_trait]
impl LlmHttpTransport for RecordingHttpTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: self.response_headers.clone(),
            body: LlmHttpBody::buffered(self.response_body),
        })
    }
}

fn reasoning_capability() -> ModelCapability {
    ModelCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["medium".to_string(), "high".to_string()],
            default_effort: Some("medium".to_string()),
            disable: Some(lash_core::provider::ReasoningDisableEncoding::Effort(
                "none".to_string(),
            )),
            ..ReasoningCapability::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
    }
}

fn budget_reasoning_capability() -> ModelCapability {
    ModelCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["medium".to_string(), "high".to_string()],
            default_effort: Some("medium".to_string()),
            encoding: ReasoningEncoding::Budget(BTreeMap::from([
                ("medium".to_string(), 4096),
                ("high".to_string(), 16384),
            ])),
            ..ReasoningCapability::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
    }
}

fn toggle_false_reasoning_capability() -> ModelCapability {
    ModelCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["medium".to_string()],
            default_effort: Some("medium".to_string()),
            disable: Some(lash_core::provider::ReasoningDisableEncoding::ToggleFalse),
            ..ReasoningCapability::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
    }
}

fn request(messages: Vec<LlmMessage>) -> LlmRequest {
    LlmRequest {
        model: "openai/gpt-5.4".to_string(),
        messages,
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::<LlmToolSpec>::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: ModelCapability::default(),
        scope: LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: None,
        stream_events: None,
        generation: lash_core::GenerationOptions::default(),
        provider_trace: None,
    }
}

fn count_object_key(value: &Value, key: &str) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key(key))
                + object
                    .values()
                    .map(|value| count_object_key(value, key))
                    .sum::<usize>()
        }
        Value::Array(array) => array.iter().map(|value| count_object_key(value, key)).sum(),
        _ => 0,
    }
}

#[tokio::test]
async fn host_enabled_session_affinity_works_through_a_custom_proxy_url() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiCompatibleProvider::new("key", "https://router-proxy.example/v1")
        .with_compat(OpenAiCompat {
            cache_session_affinity: Some(true),
            ..OpenAiCompat::default()
        })
        .with_transport(transport.clone());
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let session_id = format!("{}étrailing", "s".repeat(255));
    req.scope.session_id = session_id.clone();

    provider.complete(req).await.expect("request succeeds");

    let requests = transport.requests.lock_recover();
    let wire_request = requests.first().expect("captured request");
    let body: Value = serde_json::from_slice(&wire_request.body).expect("request body");
    let expected = session_id.chars().take(256).collect::<String>();
    assert_eq!(body["session_id"], expected);
    assert_eq!(
        body["session_id"]
            .as_str()
            .expect("session id")
            .chars()
            .count(),
        256
    );
    assert!(
        wire_request
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("session_id"))
    );
    assert!(wire_request.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("x-client-request-id") && value == "session-1:request:test"
    }));
}

#[tokio::test]
async fn session_affinity_is_disabled_without_endpoint_capability() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider =
        OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL).with_transport(transport.clone());
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    provider.complete(req).await.expect("request succeeds");

    let requests = transport.requests.lock_recover();
    let wire_request = requests.first().expect("captured request");
    let body: Value = serde_json::from_slice(&wire_request.body).expect("request body");
    assert!(body.get("session_id").is_none());
}

#[tokio::test]
async fn direct_openai_prompt_cache_key_does_not_enable_body_session_affinity() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiProvider::new("key").with_transport(transport.clone());
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    provider.complete(req).await.expect("request succeeds");

    let requests = transport.requests.lock_recover();
    let wire_request = requests.first().expect("captured request");
    let body: Value = serde_json::from_slice(&wire_request.body).expect("request body");
    assert_eq!(body["prompt_cache_key"], "session-1::session-1:frame:test");
    assert!(body.get("session_id").is_none());
    assert!(
        wire_request
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("x-client-request-id"))
    );
}

#[tokio::test]
async fn response_metadata_captures_only_allowlisted_headers() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![
            ("x-opper-cost".to_string(), "0.000008".to_string()),
            ("set-cookie".to_string(), "secret=value".to_string()),
        ],
        DEFAULT_RECORDING_RESPONSE_BODY,
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(ProviderOptions {
            response_metadata_headers: vec!["X-Opper-Cost".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert_eq!(
        response.response_metadata["header:x-opper-cost"],
        json!("0.000008")
    );
    assert!(!response.response_metadata.contains_key("header:set-cookie"));
}

#[tokio::test]
async fn response_metadata_is_empty_without_explicit_allowlists() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("x-opper-cost".to_string(), "0.000008".to_string())],
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"cost":0.000063}"#,
    ));
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://proxy.example/v1").with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert!(response.response_metadata.is_empty());
}

#[tokio::test]
async fn response_metadata_captures_buffered_body_json_pointers() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        Vec::new(),
        r#"{"id":"gen-123","model":"test-model","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"cost":0.000063}"#,
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(ProviderOptions {
            response_metadata_body_paths: vec!["/cost".to_string(), "/missing".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("request succeeds");

    assert_eq!(response.response_metadata["body:/cost"], json!(0.000063));
    assert!(!response.response_metadata.contains_key("body:/missing"));
}

#[tokio::test]
async fn response_metadata_captures_buffered_responses_endpoint_observations() {
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("x-opper-cost".to_string(), "0.000008".to_string())],
        r#"{"id":"resp-123","model":"test-model","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}],"cost":0.000063}"#,
    ));
    let mut provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            response_metadata_headers: vec!["X-Opper-Cost".to_string()],
            response_metadata_body_paths: vec!["/cost".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("Responses request succeeds");

    assert_eq!(
        response.response_metadata["header:x-opper-cost"],
        json!("0.000008")
    );
    assert_eq!(response.response_metadata["body:/cost"], json!(0.000063));
}

#[test]
fn chat_image_attachment_serializes_as_data_url() {
    let provider = openrouter_provider();
    let png_bytes = vec![0x89, 0x50, 0x4E, 0x47];
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![
            LlmContentBlock::Text {
                text: "look".into(),
                response_meta: None,
                cache_breakpoint: false,
            },
            LlmContentBlock::Attachment { attachment_idx: 0 },
        ],
    )]);
    req.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse("image/png").unwrap(),
        png_bytes.clone(),
    )];
    let body = provider.build_chat_request_body(&req, false).unwrap();
    let messages = body["messages"].as_array().expect("messages");
    let user_msg = messages.last().expect("user message");
    let content = user_msg["content"].as_array().expect("content array");
    let image_part = content
        .iter()
        .find(|part| part["type"] == "image_url")
        .expect("image_url part");
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    assert_eq!(
        image_part["image_url"]["url"],
        format!("data:image/png;base64,{expected_b64}")
    );
}

#[test]
fn chat_unsupported_image_mime_is_rejected_at_request_boundary() {
    let provider = openrouter_provider();
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
    )]);
    req.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse("image/bmp").unwrap(),
        vec![0x42, 0x4D],
    )];

    let err = provider
        .build_chat_request_body(&req, false)
        .expect_err("bmp should be rejected before wire");
    assert_eq!(
        err.code.as_deref(),
        Some("unsupported_attachment_capability")
    );
    assert_eq!(
        err.message,
        "OpenAI Chat Completions cannot materialize attachment MIME `image/bmp` from source `inline`; providers accepting this MIME/source: none"
    );
}

#[test]
fn responses_image_attachment_serializes_as_input_image_data_url() {
    let provider = OpenAiProvider::new("key");
    let png_bytes = vec![0x89, 0x50, 0x4E, 0x47];
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![
            LlmContentBlock::Text {
                text: "look".into(),
                response_meta: None,
                cache_breakpoint: false,
            },
            LlmContentBlock::Attachment { attachment_idx: 0 },
        ],
    )]);
    req.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse("image/png").unwrap(),
        png_bytes.clone(),
    )];
    let body = provider.build_responses_request_body(&req, false).unwrap();
    let input = body["input"].as_array().expect("input array");
    let user_msg = input.last().expect("user message");
    let content = user_msg["content"].as_array().expect("content array");
    let image_part = content
        .iter()
        .find(|part| part["type"] == "input_image")
        .expect("input_image part");
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    assert_eq!(
        image_part["image_url"],
        format!("data:image/png;base64,{expected_b64}")
    );
}

#[test]
fn responses_unsupported_image_mime_is_rejected_at_request_boundary() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Attachment { attachment_idx: 0 }],
    )]);
    req.attachments = vec![AttachmentSource::inline(
        lash_core::MediaType::parse("image/bmp").unwrap(),
        vec![0x42, 0x4D],
    )];

    let err = provider
        .build_responses_request_body(&req, false)
        .expect_err("bmp should be rejected before wire");

    assert_eq!(
        err.code.as_deref(),
        Some("unsupported_attachment_capability")
    );
    assert!(err.message.contains("OpenAI"));
}

#[test]
fn builds_responses_body_with_instructions_and_input() {
    let provider = OpenAiProvider::new("key");
    let req = request(vec![
        LlmMessage::text(LlmRole::System, "system prompt"),
        LlmMessage::text(LlmRole::User, "hello"),
    ]);
    let body = provider.build_responses_request_body(&req, true).unwrap();
    assert_eq!(body["instructions"], "system prompt");
    assert_eq!(body["stream"], true);
    assert!(body.get("messages").is_none());
    assert!(body.get("cache_control").is_none());
    assert_eq!(body["prompt_cache_key"], "session-1::session-1:frame:test");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
}

#[test]
fn responses_body_emits_reasoning_from_capability_variant() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "custom-direct-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn responses_body_rejects_numeric_reasoning_from_budget_encoding() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = budget_reasoning_capability();

    let error = provider
        .build_responses_request_body(&req, true)
        .expect_err("OpenAI Responses cannot encode a budget");

    assert_eq!(
        error.code.as_deref(),
        Some("reasoning_encoding_unrepresentable")
    );
    assert!(!error.retryable);
    assert!(error.message.contains("openai"));
    assert!(error.message.contains("Responses"));
    assert!(error.message.contains("Budget(16384)"));
}

#[test]
fn responses_body_emits_none_effort_for_disabled_selection() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    req.model_capability = reasoning_capability();

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert_eq!(body["reasoning"], json!({ "effort": "none" }));
}

#[test]
fn responses_body_omits_reasoning_without_capability() {
    let provider = OpenAiProvider::new("key");
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "custom-direct-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert!(body.get("reasoning").is_none());
}

#[test]
fn responses_body_requests_reasoning_summaries_when_provider_exposes_thinking() {
    let provider = OpenAiProvider::new("key").with_options(ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    });
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
    req.model_capability = reasoning_capability();

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert_eq!(body["reasoning"]["summary"], "auto");
}

#[test]
fn providers_serialize_distinct_config_shapes() {
    let direct = OpenAiProvider::new("key");
    let direct_config = direct.serialize_config();
    assert_eq!(direct.kind(), "openai");
    assert_eq!(direct_config["api_key"], "key");
    assert!(direct_config.get("base_url").is_none());
    assert!(direct_config.get("wire_api").is_none());

    let compatible = openrouter_provider();
    let compatible_config = compatible.serialize_config();
    assert_eq!(compatible.kind(), "openai-compatible");
    assert_eq!(compatible_config["api_key"], "key");
    assert_eq!(compatible_config["base_url"], OPENROUTER_BASE_URL);
    assert!(compatible_config.get("wire_api").is_none());
}

#[test]
fn provider_factories_reject_stale_wire_api_configs() {
    let direct_err = OpenAiProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "wire_api": "responses"
        }))
        .expect_err("direct OpenAI rejects wire_api");
    assert!(direct_err.contains("wire_api"));

    let compatible_err = OpenAiCompatibleProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "base_url": OPENROUTER_BASE_URL,
            "wire_api": "chat-completions"
        }))
        .expect_err("compatible OpenAI rejects wire_api");
    assert!(compatible_err.contains("wire_api"));
}

#[test]
fn provider_factories_materialize_distinct_kinds() {
    let direct = OpenAiProviderFactory
        .deserialize(json!({ "api_key": "key" }))
        .expect("direct config");
    assert_eq!(direct.provider.kind(), "openai");

    let compatible = OpenAiCompatibleProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "base_url": OPENROUTER_BASE_URL
        }))
        .expect("compatible config");
    assert_eq!(compatible.provider.kind(), "openai-compatible");
}

#[test]
fn chat_body_uses_messages_and_not_responses_input() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "system prompt"),
        LlmMessage::text(LlmRole::User, "hello"),
    ]);
    req.model = "anthropic/claude-sonnet-4.6".to_string();
    req.output_spec = Some(LlmOutputSpec::JsonObject);

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert!(body.get("input").is_none());
    assert!(body.get("instructions").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));
}

#[test]
fn chat_body_emits_reasoning_from_capability_variant() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "openrouter/custom-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn chat_body_openai_format_emits_top_level_reasoning_effort_only() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();
    let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_reasoning_format(ReasoningWireFormat::openai());

    let body = provider.build_chat_request_body(&req, true).unwrap();

    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn chat_body_emits_numeric_reasoning_from_budget_encoding() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "openrouter/custom-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = budget_reasoning_capability();

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(body["reasoning"], json!({ "max_tokens": 16384 }));
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn chat_body_openai_format_rejects_budget() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = budget_reasoning_capability();
    let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_reasoning_format(ReasoningWireFormat::openai());

    let error = provider
        .build_chat_request_body(&req, true)
        .expect_err("OpenAI Chat Completions cannot encode a budget");

    assert_eq!(
        error.code.as_deref(),
        Some("reasoning_encoding_unrepresentable")
    );
    assert!(!error.retryable);
    assert!(error.message.contains("openai"));
    assert!(error.message.contains("ChatCompletions"));
    assert!(error.message.contains("Budget(16384)"));
}

#[test]
fn chat_body_emits_none_effort_for_disabled_selection() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    req.model_capability = reasoning_capability();

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(body["reasoning"], json!({ "effort": "none" }));
}

#[test]
fn chat_body_reasoning_toggle_false_respects_dialect() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    req.model_capability = toggle_false_reasoning_capability();

    let openai = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_reasoning_format(ReasoningWireFormat::openai());
    let error = openai
        .build_chat_request_body(&req, true)
        .expect_err("OpenAI Chat Completions cannot encode enabled:false");
    assert_eq!(
        error.code.as_deref(),
        Some("reasoning_encoding_unrepresentable")
    );
    assert!(!error.retryable);
    assert!(error.message.contains("ToggleFalse"));

    let openrouter = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();
    assert_eq!(openrouter["reasoning"], json!({ "enabled": false }));
    assert!(openrouter.get("reasoning_effort").is_none());
}

#[test]
fn chat_body_omits_reasoning_without_capability() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "openrouter/custom-model".to_string();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert!(body.get("reasoning").is_none());
}

#[test]
fn chat_body_none_format_omits_resolved_reasoning_intent() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = reasoning_capability();
    let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1");

    let body = provider.build_chat_request_body(&req, true).unwrap();

    assert!(body.get("reasoning").is_none());
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn anthropic_cache_dialect_marks_canonical_breakpoints_for_an_arbitrary_model() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::text(LlmRole::User, "dynamic tail"),
    ]);
    req.model = "custom/model-v1".to_string();
    enable_cache_control(&mut req, CacheControlDialect::Anthropic);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "search".to_string(),
        description: "Search".to_string(),
        input_schema: json!({"type": "object"}).into(),
        output_schema: json!({}).into(),
    }]);

    let body = OpenAiCompatibleProvider::new("key", "https://router-proxy.example/v1")
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        body["tools"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
}

#[test]
fn gemini_cache_dialect_emits_one_ephemeral_explicit_breakpoint() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::new(
            LlmRole::User,
            vec![
                LlmContentBlock::Text {
                    text: "older stable history".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                },
                LlmContentBlock::Text {
                    text: "latest stable history".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                },
                LlmContentBlock::Text {
                    text: "dynamic current iteration".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
            ],
        ),
    ]);
    req.model = "custom/model-v1".to_string();
    enable_cache_control(&mut req, CacheControlDialect::Gemini);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "search".to_string(),
        description: "Search".to_string(),
        input_schema: json!({"type": "object"}).into(),
        output_schema: json!({}).into(),
    }]);

    let body = openrouter_provider()
        .with_options(ProviderOptions {
            cache_retention: CacheRetention::Long,
            ..ProviderOptions::default()
        })
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(count_object_key(&body, "cache_control"), 1);
    assert_eq!(
        body["messages"][1]["content"][1]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(
        body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert!(body["messages"][0]["content"].is_array());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(count_object_key(&body, "ttl"), 0);
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}

#[test]
fn gemini_cache_dialect_falls_back_to_last_message_text() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::text(LlmRole::User, "last stable text"),
    ]);
    req.model = "custom/model-v1".to_string();
    enable_cache_control(&mut req, CacheControlDialect::Gemini);

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(count_object_key(&body, "cache_control"), 1);
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(body["messages"][0]["content"].is_array());
}

#[test]
fn absent_cache_dialect_strips_breakpoint_even_on_the_openrouter_url() {
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: "stable history".into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    )]);
    req.model = "google/gemini-2.5-pro".to_string();

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(count_object_key(&body, "cache_control"), 0);
    assert_eq!(count_object_key(&body, "__lash_cache_breakpoint"), 0);
}

#[test]
fn chat_tools_use_projected_openai_schema_and_preserve_override() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "anthropic/claude-sonnet-4.6".to_string();
    req.tools = Arc::new(vec![
        LlmToolSpec {
            name: "empty".to_string(),
            description: "Empty".to_string(),
            input_schema: json!({"type": "object"}).into(),
            output_schema: json!({}).into(),
        },
        LlmToolSpec {
            name: "override".to_string(),
            description: "Override".to_string(),
            input_schema: lash_core::SchemaContract::new(json!({
                "type": "object",
                "properties": {"raw": {"const": "x"}}
            }))
            .with_override(
                lash_core::facade_support::SchemaDialect::OPENAI_TOOL_PARAMETERS,
                json!({
                    "type": "object",
                    "properties": { "raw": { "type": "string", "enum": ["x"] } }
                }),
            ),
            output_schema: json!({}).into(),
        },
        LlmToolSpec {
            name: "schemars".to_string(),
            description: "Schemars".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "description": "Maximum number of results.",
                        "allOf": [
                            {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 100
                            }
                        ]
                    }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        },
    ]);

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"],
        json!({})
    );
    assert_eq!(
        body["tools"][1]["function"]["parameters"]["properties"]["raw"],
        json!({ "type": "string", "enum": ["x"] })
    );
    assert_eq!(
        body["tools"][2]["function"]["parameters"]["properties"]["limit"],
        json!({
            "description": "Maximum number of results.",
            "type": "integer",
            "minimum": 1,
            "maximum": 100
        })
    );
}

#[test]
fn structured_output_schema_is_projected_or_rejected_locally() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "result".to_string(),
        schema: json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } }
        })
        .into(),
        strict: true,
    }));

    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, false)
        .unwrap();
    assert_eq!(
        body["text"]["format"]["schema"]["required"],
        json!(["summary"])
    );
    assert_eq!(
        body["text"]["format"]["schema"]["additionalProperties"],
        false
    );

    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "bad".to_string(),
        schema: json!({"type": "object", "allOf": []}).into(),
        strict: true,
    }));
    let err = OpenAiProvider::new("key")
        .build_responses_request_body(&req, false)
        .unwrap_err();
    assert_eq!(err.kind, ProviderFailureKind::Validation);
    assert!(err.message.contains("allOf"));
}

#[test]
fn openrouter_can_be_configured_for_bedrock_safe_schema_dialect() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "rank")]);
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "rank_result".to_string(),
        schema: json!({
            "type": "object",
            "required": ["ranked"],
            "properties": {
                "ranked": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": { "type": "string" }
                }
            }
        })
        .into(),
        strict: true,
    }));

    let body = openrouter_provider()
        .with_schema_capabilities(
            lash_core::facade_support::ProviderSchemaCapabilities::bedrock_claude(),
        )
        .build_chat_request_body(&req, true)
        .unwrap();
    let ranked = &body["response_format"]["json_schema"]["schema"]["properties"]["ranked"];

    assert!(ranked.get("minItems").is_none());
    assert!(ranked.get("maxItems").is_none());
    assert!(
        ranked["description"]
            .as_str()
            .is_some_and(|description| description.contains("minItems=2"))
    );
}

#[test]
fn anthropic_cache_dialect_prefers_explicit_text_breakpoint() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::new(
            LlmRole::User,
            vec![
                LlmContentBlock::Text {
                    text: "stable history".into(),
                    response_meta: None,
                    cache_breakpoint: true,
                },
                LlmContentBlock::Text {
                    text: "dynamic current iteration".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
            ],
        ),
    ]);
    req.model = "custom/model-v1".to_string();
    enable_cache_control(&mut req, CacheControlDialect::Anthropic);

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(
        body["messages"][1]["content"][1]
            .get("cache_control")
            .is_none()
    );
    assert!(
        body["messages"][1]["content"][0]
            .get("__lash_cache_breakpoint")
            .is_none()
    );
}

fn without_cache_control(mut value: Value) -> Value {
    if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
        for part in parts {
            part.as_object_mut()
                .map(|object| object.remove("cache_control"));
        }
    }
    value
}

#[test]
fn cache_dialect_chat_history_shape_is_stable_when_breakpoint_moves() {
    for (model, dialect) in [
        ("custom/model-a", CacheControlDialect::Anthropic),
        ("custom/model-b", CacheControlDialect::Gemini),
    ] {
        let history = |text: &'static str, cache_breakpoint| {
            LlmMessage::new(
                LlmRole::User,
                vec![LlmContentBlock::Text {
                    text: text.into(),
                    response_meta: None,
                    cache_breakpoint,
                }],
            )
        };
        let mut previous = request(vec![
            LlmMessage::text(LlmRole::System, "stable system prompt"),
            history("stable history one", true),
            LlmMessage::text(LlmRole::User, "dynamic iteration one"),
        ]);
        previous.model = model.to_string();
        enable_cache_control(&mut previous, dialect);
        let mut next = request(vec![
            LlmMessage::text(LlmRole::System, "stable system prompt"),
            history("stable history one", false),
            history("stable history two", true),
            LlmMessage::text(LlmRole::User, "dynamic iteration two"),
        ]);
        next.model = model.to_string();
        enable_cache_control(&mut next, dialect);

        let provider = openrouter_provider();
        let previous_body = provider.build_chat_request_body(&previous, true).unwrap();
        let next_body = provider.build_chat_request_body(&next, true).unwrap();
        let previous_history = without_cache_control(previous_body["messages"][1].clone());
        let next_history = without_cache_control(next_body["messages"][1].clone());

        assert_eq!(
            serde_json::to_vec(&previous_history).unwrap(),
            serde_json::to_vec(&next_history).unwrap(),
            "earlier history wire shape changed for {model}"
        );
    }
}

#[test]
fn cache_retention_none_removes_chat_cache_markers() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::text(LlmRole::User, "dynamic tail"),
    ]);
    enable_cache_control(&mut req, CacheControlDialect::Anthropic);

    let body = openrouter_provider()
        .with_options(ProviderOptions {
            cache_retention: CacheRetention::None,
            ..ProviderOptions::default()
        })
        .build_chat_request_body(&req, true)
        .unwrap();

    assert!(body["messages"][0]["content"].is_array());
    assert!(body["messages"][1]["content"].is_array());
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .all(|part| part.get("__lash_cache_breakpoint").is_none())
    );
    assert!(body.get("tools").is_none());
}

#[test]
fn cache_retention_long_uses_anthropic_ttl_on_chat_cache_markers() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::System, "stable system prompt"),
        LlmMessage::text(LlmRole::User, "dynamic tail"),
    ]);
    enable_cache_control(&mut req, CacheControlDialect::Anthropic);

    let body = openrouter_provider()
        .with_options(ProviderOptions {
            cache_retention: CacheRetention::Long,
            ..ProviderOptions::default()
        })
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({ "type": "ephemeral", "ttl": "1h" })
    );
}

#[test]
fn responses_long_cache_retention_emits_openai_retention() {
    let provider = OpenAiProvider::new("key").with_options(ProviderOptions {
        cache_retention: CacheRetention::Long,
        ..ProviderOptions::default()
    });
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert_eq!(body["prompt_cache_key"], "session-1::session-1:frame:test");
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert!(body.get("cache_control").is_none());
}

#[test]
fn responses_none_cache_retention_omits_prompt_cache_fields() {
    let provider = OpenAiProvider::new("key").with_options(ProviderOptions {
        cache_retention: CacheRetention::None,
        ..ProviderOptions::default()
    });
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let body = provider.build_responses_request_body(&req, true).unwrap();

    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn openai_compat_config_serializes_when_non_default() {
    let provider = openrouter_provider()
        .with_compat(OpenAiCompat {
            max_tokens_field: Some(OpenAiCompatMaxTokensField::MaxCompletionTokens),
            streaming_usage: Some(false),
            provider_routing: Some(ProviderRoutingPrefs {
                require_parameters: true,
            }),
            ..OpenAiCompat::default()
        })
        .with_options(ProviderOptions {
            response_metadata_headers: vec!["X-Opper-Cost".to_string()],
            response_metadata_body_paths: vec!["/cost".to_string()],
            ..ProviderOptions::default()
        });

    let config = provider.serialize_config();

    assert_eq!(
        config["compat"]["max_tokens_field"],
        json!("max_completion_tokens")
    );
    assert_eq!(config["compat"]["streaming_usage"], false);
    assert_eq!(
        config["compat"]["provider_routing"],
        json!({ "require_parameters": true })
    );
    assert_eq!(
        config["options"]["response_metadata_headers"],
        json!(["X-Opper-Cost"])
    );
    assert_eq!(
        config["options"]["response_metadata_body_paths"],
        json!(["/cost"])
    );

    let round_trip = OpenAiCompatibleProviderFactory
        .deserialize(config)
        .expect("response metadata allowlists deserialize");
    let serialized = round_trip.provider.serialize_config();
    assert_eq!(
        serialized["options"]["response_metadata_headers"],
        json!(["X-Opper-Cost"])
    );
    assert_eq!(
        serialized["compat"]["provider_routing"],
        json!({ "require_parameters": true })
    );
    assert_eq!(
        serialized["options"]["response_metadata_body_paths"],
        json!(["/cost"])
    );
}

#[test]
fn provider_routing_config_rejects_unknown_sibling_keys() {
    let nested_error = OpenAiCompatibleProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "base_url": "https://proxy.example/v1",
            "compat": {
                "provider_routing": {
                    "require_parameters": true,
                    "unknown_preference": true
                }
            }
        }))
        .expect_err("unknown provider routing preference must be rejected");
    assert!(nested_error.contains("unknown_preference"));

    let compat_error = OpenAiCompatibleProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "base_url": "https://proxy.example/v1",
            "compat": {
                "provider_routing": { "require_parameters": true },
                "unknown_compat_policy": true
            }
        }))
        .expect_err("unknown compat policy must still be rejected");
    assert!(compat_error.contains("unknown_compat_policy"));
}

#[test]
fn reasoning_wire_format_equality_uses_name_identity() {
    assert_eq!(
        ReasoningWireFormat::openrouter(),
        ReasoningWireFormat::openrouter()
    );
    assert_ne!(
        ReasoningWireFormat::openai(),
        ReasoningWireFormat::openrouter()
    );
}

#[test]
fn reasoning_formats_round_trip_through_compatible_factory() {
    for format in [
        ReasoningWireFormat::openrouter(),
        ReasoningWireFormat::openai(),
    ] {
        let name = format.name().to_string();
        let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
            .with_reasoning_format(format);
        let config = provider.serialize_config();
        assert_eq!(config["compat"]["reasoning_format"], name);

        let round_trip = OpenAiCompatibleProviderFactory
            .deserialize(config)
            .expect("built-in reasoning format round trip");
        assert_eq!(
            round_trip.provider.serialize_config()["compat"]["reasoning_format"],
            name
        );
    }
}

#[test]
fn compatible_factory_rejects_unknown_reasoning_format() {
    let error = OpenAiCompatibleProviderFactory
        .deserialize(json!({
            "api_key": "key",
            "base_url": "https://proxy.example/v1",
            "compat": { "reasoning_format": "qwen3" }
        }))
        .expect_err("custom reasoning formats are programmatic only");

    assert!(error.contains("unknown reasoning format `qwen3`"));
    assert!(error.contains("none/openai/openrouter"));
}

#[derive(Debug)]
struct DeepSeekTestReasoningEncoder;

impl ReasoningWireEncoder for DeepSeekTestReasoningEncoder {
    fn name(&self) -> &str {
        "deepseek-test"
    }

    fn encode(
        &self,
        endpoint: CompletionEndpoint,
        intent: &ReasoningWireIntent,
        body: &mut Value,
    ) -> Result<(), ReasoningEncodeError> {
        match (endpoint, intent) {
            (CompletionEndpoint::ChatCompletions, ReasoningWireIntent::Effort(_)) => {
                body["thinking"] = json!({ "type": "enabled" });
                Ok(())
            }
            _ => Err(ReasoningEncodeError {
                dialect: self.name().to_string(),
                detail: "test dialect only supports Chat Completions effort".to_string(),
            }),
        }
    }
}

#[test]
fn custom_reasoning_encoder_is_attached_programmatically() {
    let format = ReasoningWireFormat::custom(Arc::new(DeepSeekTestReasoningEncoder));
    let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_reasoning_format(format);
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
    req.model_capability = reasoning_capability();

    let body = provider.build_chat_request_body(&req, false).unwrap();

    assert_eq!(body["thinking"], json!({ "type": "enabled" }));
}

#[test]
fn custom_reasoning_format_serializes_but_cannot_be_deserialized() {
    let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_reasoning_format(ReasoningWireFormat::custom(Arc::new(
            DeepSeekTestReasoningEncoder,
        )));

    let config = provider.serialize_config();
    assert_eq!(config["compat"]["reasoning_format"], json!("deepseek-test"));
    let error = OpenAiCompatibleProviderFactory
        .deserialize(config)
        .expect_err("custom reasoning format cannot round trip through serde");
    assert!(error.contains("unknown reasoning format `deepseek-test`"));
    assert!(error.contains("none/openai/openrouter"));
}

#[test]
fn openai_compat_resolver_covers_openrouter_and_session_affinity() {
    let openrouter = openrouter_provider();
    let openrouter_caps = openrouter.resolved_compat(CompletionEndpoint::ChatCompletions);
    assert_eq!(
        openrouter_caps.reasoning_format,
        ReasoningWireFormat::openrouter()
    );
    assert!(openrouter_caps.streaming_usage);
    assert!(openrouter_caps.cache_session_affinity);
    // Endpoint facts only: restricted routing and response-metadata capture
    // are host decisions, so the preset resolves neither.
    assert_eq!(openrouter_caps.provider_routing, None);
    assert!(openrouter.options.response_metadata_body_paths.is_empty());
}

#[test]
fn localhost_without_local_preset_keeps_optional_request_fields() {
    let compat = OpenAiCompatibleProvider::new("key", "http://localhost:11434/v1")
        .resolved_compat(CompletionEndpoint::ChatCompletions);

    assert!(compat.request_fields);
    assert!(compat.store);
    assert!(compat.streaming_usage);
}

#[test]
fn non_local_url_with_local_preset_omits_optional_request_fields() {
    let compat = OpenAiCompatibleProvider::new("key", "https://gpu-box.example/v1")
        .with_compat(OpenAiCompat::local())
        .resolved_compat(CompletionEndpoint::ChatCompletions);

    assert!(!compat.request_fields);
    assert!(!compat.store);
    assert!(!compat.streaming_usage);
}

#[test]
fn chat_body_honors_compat_max_token_field_streaming_usage_and_strict_tools() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "lookup".to_string(),
        description: "Lookup".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "q": { "type": "string" } }
        })
        .into(),
        output_schema: json!({}).into(),
    }]);

    let body = openrouter_provider()
        .with_compat(OpenAiCompat {
            max_tokens_field: Some(OpenAiCompatMaxTokensField::MaxCompletionTokens),
            streaming_usage: Some(false),
            strict_tools: Some(true),
            ..OpenAiCompat::default()
        })
        .build_chat_request_body(&req, true)
        .unwrap();

    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["max_completion_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["tools"][0]["function"]["strict"], true);
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["additionalProperties"],
        false
    );
}

#[test]
fn local_preset_suppresses_optional_openai_fields() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("medium".to_string());
    req.model_capability = reasoning_capability();
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "lookup".to_string(),
        description: "Lookup".to_string(),
        input_schema: json!({"type": "object"}).into(),
        output_schema: json!({}).into(),
    }]);

    let local = OpenAiCompatibleProvider::new("key", "https://gpu-box.example/v1")
        .with_compat(OpenAiCompat::local());
    let chat = local.build_chat_request_body(&req, true).unwrap();
    assert!(chat.get("stream_options").is_none());
    assert!(chat.get("parallel_tool_calls").is_none());
    assert!(chat.get("reasoning").is_none());

    let responses = local.build_responses_request_body(&req, true).unwrap();
    assert!(responses.get("include").is_none());
    assert!(responses.get("store").is_none());
    assert!(responses.get("text").is_none());
    assert!(responses.get("prompt_cache_key").is_none());
}

#[test]
fn assistant_text_preserves_response_meta() {
    let provider = OpenAiProvider::new("key");
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Text {
            text: "done".into(),
            response_meta: Some(ResponseTextMeta {
                id: Some("msg_1".to_string()),
                status: Some("completed".to_string()),
                phase: Some("final_answer".to_string()),
                ..ResponseTextMeta::default()
            }),
            cache_breakpoint: false,
        }],
    )]);
    let body = provider.build_responses_request_body(&req, false).unwrap();
    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["id"], "msg_1");
    assert_eq!(body["input"][0]["phase"], "final_answer");
}

#[test]
fn legacy_assistant_text_gets_deterministic_id() {
    let provider = OpenAiProvider::new("key");
    let req = request(vec![LlmMessage::text(LlmRole::Assistant, "legacy")]);
    let body = provider.build_responses_request_body(&req, false).unwrap();
    assert_eq!(body["input"][0]["id"], "msg_lash_0_0");
    assert_eq!(body["input"][0]["status"], "completed");
    assert!(body["input"][0].get("phase").is_none());
}

#[test]
fn chat_body_replays_openrouter_reasoning_details_on_tool_calls() {
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call_1".to_string(),
            tool_name: "lookup".to_string(),
            input_json: "{\"q\":\"x\"}".to_string(),
            replay: Some(ProviderReplayMeta {
                item_id: None,
                opaque: Some(
                    json!({
                        "type": "reasoning.encrypted",
                        "id": "call_1",
                        "data": "encrypted"
                    })
                    .to_string(),
                ),
            }),
        }],
    )]);

    let body = openrouter_provider()
        .build_chat_request_body(&req, false)
        .unwrap();

    assert_eq!(
        body["messages"][0]["reasoning_details"][0],
        json!({
            "type": "reasoning.encrypted",
            "id": "call_1",
            "data": "encrypted"
        })
    );
}

#[test]
fn non_streaming_chat_parser_captures_text_tool_and_usage() {
    let value = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "reasoning_content": "think",
                "content": "hello",
                "reasoning_details": [{
                    "type": "reasoning.encrypted",
                    "id": "call_1",
                    "data": "encrypted"
                }],
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"q\":\"x\"}"
                    }
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 13,
            "completion_tokens": 17,
            "prompt_tokens_details": { "cached_tokens": 2 }
        }
    });

    let parts = OpenAiCompatibleProvider::chat_response_parts_from_value(&value);
    let usage = lash_llm_transport::openai_usage_from_response_value(&value);

    assert!(matches!(&parts[0], LlmOutputPart::Reasoning { text, .. } if text == "think"));
    assert!(matches!(&parts[1], LlmOutputPart::Text { text, .. } if text == "hello"));
    assert!(matches!(
        &parts[2],
        LlmOutputPart::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
            ..
        } if call_id == "call_1"
            && tool_name == "lookup"
            && input_json == "{\"q\":\"x\"}"
            && replay
                .as_ref()
                .and_then(|meta| meta.opaque.as_deref())
                .is_some_and(|value| value.contains("encrypted"))
    ));
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 17);
    assert_eq!(usage.cache_read_input_tokens, 2);
    assert_eq!(usage.cache_write_input_tokens, 0);
}

#[test]
fn chat_usage_payload_maps_canonical_token_buckets() {
    let value = json!({
        "usage": {
            "prompt_tokens": 21,
            "completion_tokens": 13,
            "prompt_tokens_details": {
                "cached_tokens": 5,
                "cache_write_tokens": 4
            },
            "completion_tokens_details": {
                "reasoning_tokens": 3
            }
        }
    });

    let usage = lash_llm_transport::openai_usage_from_response_value(&value);

    assert_eq!(
        usage,
        lash_core::llm::types::LlmUsage {
            input_tokens: 12,
            output_tokens: 13,
            cache_read_input_tokens: 5,
            cache_write_input_tokens: 4,
            reasoning_output_tokens: 3,
        }
    );
}

#[test]
fn openrouter_gemini_production_usage_payload_maps_zero_cache_buckets() {
    let usage = lash_llm_transport::openai_usage_from_usage_value(&json!({
        "prompt_tokens": 6370,
        "completion_tokens": 7,
        "prompt_tokens_details": {
            "audio_tokens": 0,
            "cache_write_tokens": 0,
            "cached_tokens": 0,
            "video_tokens": 0
        },
        "cost": 0.003206
    }));

    assert_eq!(
        usage,
        lash_core::llm::types::LlmUsage {
            input_tokens: 6370,
            output_tokens: 7,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        }
    );
}

#[test]
fn chat_stream_parser_captures_text_tool_done_and_usage() {
    let mut state = ChatStreamState::default();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{"reasoning_content":"Think"}}]}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","id":"call_1","data":"encrypted"}]}}],"usage":{"prompt_tokens":9,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"choices":[{"usage":{"prompt_tokens":11,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":2}}}]}"#,
        &mut state,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_chat_sse_event("[DONE]", &mut state).unwrap();

    assert_eq!(state.full_text, "Hi");
    assert_eq!(state.reasoning_text, "Think");
    let parts = state.parts();
    assert!(matches!(&parts[0], LlmOutputPart::Reasoning { text, .. } if text == "Think"));
    assert!(matches!(&parts[1], LlmOutputPart::Text { text, .. } if text == "Hi"));
    assert!(matches!(
        &parts[2],
        LlmOutputPart::ToolCall {
            call_id,
            tool_name,
            input_json,
            replay,
            ..
        } if call_id == "call_1"
            && tool_name == "lookup"
            && input_json == "{\"q\":\"x\"}"
            && replay
                .as_ref()
                .and_then(|meta| meta.opaque.as_deref())
                .is_some_and(|value| value.contains("encrypted"))
    ));
    assert_eq!(state.usage.input_tokens, 5);
    assert_eq!(state.usage.output_tokens, 5);
    assert_eq!(state.usage.cache_read_input_tokens, 4);
    assert_eq!(state.usage.cache_write_input_tokens, 2);
}

#[test]
fn stream_parser_captures_text_reasoning_tool_and_phase() {
    let mut state = ResponsesStreamState::default();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1"}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"Think"}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Think"}],"encrypted_content":"enc"}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.done","item":{"type":"message","id":"msg_1","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Hi"}]}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":""}}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"x\":"}"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"x\":1}" }"#,
        &mut state,
        None,
    )
    .unwrap();
    OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"tool","arguments":"{\"x\":1}","status":"completed"}}"#,
        &mut state,
        None,
    )
    .unwrap();

    assert_eq!(state.full_text, "Hi");
    let parts = state.response_parts();
    assert!(matches!(
        &parts[0],
        LlmOutputPart::Reasoning { replay: Some(replay), .. }
            if replay.item_id.as_deref() == Some("rs_1")
                && replay.encrypted_content.as_deref() == Some("enc")
    ));
    assert!(matches!(
        &parts[1],
        LlmOutputPart::Text {
            response_meta: Some(ResponseTextMeta {
                phase: Some(phase),
                ..
            }),
            ..
        } if phase == "commentary"
    ));
    assert!(matches!(
        &parts[2],
        LlmOutputPart::ToolCall {
            replay: Some(replay),
            input_json,
            ..
        } if replay.item_id.as_deref() == Some("fc_1") && input_json == "{\"x\":1}"
    ));
}

#[test]
fn responses_final_answer_phase_hides_commentary_from_visible_text() {
    let mut state = ResponsesStreamState::default();
    for event in [
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_commentary","phase":"commentary"}}"#,
        r#"{"type":"response.output_text.delta","output_index":0,"item_id":"msg_commentary","delta":"Working notes."}"#,
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_commentary","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Working notes."}]}}"#,
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_final","phase":"final_answer"}}"#,
        r#"{"type":"response.output_text.delta","output_index":1,"item_id":"msg_final","delta":"Final answer."}"#,
        r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_final","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Final answer."}]}}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_commentary","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Working notes."}]},{"type":"message","id":"msg_final","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Final answer."}]}]}}"#,
    ] {
        OpenAiCompatibleProvider::process_sse_event(event, &mut state, None).unwrap();
    }

    let parts = state.response_parts();
    assert_eq!(state.full_text, "Final answer.");
    assert_eq!(
        parts
            .iter()
            .filter(|part| matches!(part, LlmOutputPart::Text { .. }))
            .count(),
        2
    );
    let response = LlmResponse {
        full_text: state.full_text.clone(),
        parts,
        response_metadata: Default::default(),
        ..LlmResponse::default()
    };
    let visible = lash_core::facade_support::normalized_response_parts(&response);
    assert_eq!(
        visible
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "Final answer."
    );
}

#[test]
fn response_failed_server_error_is_retryable() {
    let mut state = ResponsesStreamState::default();
    let err = OpenAiCompatibleProvider::process_sse_event(
        r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"internal stream ended unexpectedly"}}}"#,
        &mut state,
        None,
    )
    .unwrap_err();

    assert!(err.retryable);
    assert_eq!(err.message, "internal stream ended unexpectedly");
}

#[test]
fn openrouter_buffered_wire_preserves_concrete_model_and_explicit_zero_reasoning() {
    let value = json!({
        "id": "gen-123",
        "model": "anthropic/claude-sonnet-4.5",
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop",
            "native_finish_reason": "stop_sequence"
        }],
        "usage": {
            "completion_tokens_details": { "reasoning_tokens": 0 }
        }
    });
    let mut state = ChatStreamState::default();
    state.capture_response_value(&value);

    assert_eq!(
        state.execution_evidence(),
        Some(ExecutionEvidence {
            served_model: Some("anthropic/claude-sonnet-4.5".to_string()),
            provider_response_id: Some("gen-123".to_string()),
            provider_request_id: None,
            reasoning_output_tokens: Some(0),
            provider_finish_reason: Some("stop_sequence".to_string()),
        })
    );
    assert_ne!(state.served_model.as_deref(), Some("openrouter/auto"));
}

#[tokio::test]
async fn openrouter_handle_records_failed_request_id_then_served_model_evidence() {
    let transport = Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from([
            (
                429,
                vec![
                    ("x-request-id".to_string(), "req-failed".to_string()),
                    ("retry-after".to_string(), "0".to_string()),
                ],
                r#"{"error":{"message":"temporarily throttled"}}"#,
            ),
            (
                200,
                vec![("x-request-id".to_string(), "req-success".to_string())],
                r#"{"id":"gen-123","model":"anthropic/claude-sonnet-4.5","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"completion_tokens_details":{"reasoning_tokens":0}}}"#,
            ),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let provider = openrouter_provider()
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(transport);
    let mut handle = ProviderHandle::new(provider.into_components());
    let mut req = request(Vec::new());
    req.model = "openrouter/auto".to_string();

    let completion = handle.complete(req).await.expect("retry succeeds");
    assert_eq!(completion.call_record.attempts.len(), 2);
    let failed = &completion.call_record.attempts[0];
    assert_eq!(failed.outcome, lash_core::AttemptOutcome::Failed);
    assert_eq!(
        failed
            .error
            .as_ref()
            .unwrap()
            .provider_request_id
            .as_deref(),
        Some("req-failed")
    );
    assert_eq!(failed.error.as_ref().unwrap().http_status, Some(429));
    assert_eq!(
        failed
            .evidence
            .as_ref()
            .unwrap()
            .provider_request_id
            .as_deref(),
        Some("req-failed")
    );

    let evidence = completion.call_record.attempts[1]
        .evidence
        .as_ref()
        .expect("success evidence");
    assert_eq!(
        evidence.served_model.as_deref(),
        Some("anthropic/claude-sonnet-4.5")
    );
    assert_eq!(evidence.provider_request_id.as_deref(), Some("req-success"));
    assert_eq!(evidence.reasoning_output_tokens, Some(0));
    assert!(completion.call_record.attempts[1].usage.is_some());
}

#[test]
fn openrouter_buffered_wire_keeps_absent_reasoning_distinct_from_zero() {
    let mut state = ChatStreamState::default();
    state.capture_reasoning_tokens(&json!({ "completion_tokens": 3 }));
    assert_eq!(state.reasoning_output_tokens, None);

    state.capture_reasoning_tokens(&json!({
        "completion_tokens_details": { "reasoning_tokens": 0 }
    }));
    assert_eq!(state.reasoning_output_tokens, Some(0));
}

#[test]
fn openrouter_stream_wire_retains_partial_identity_when_stream_ends_early() {
    let mut state = ChatStreamState::default();
    OpenAiCompatibleProvider::process_chat_sse_event(
        r#"{"id":"gen-partial","model":"openai/gpt-5.4-mini","choices":[{"delta":{"content":"partial"}}]}"#,
        &mut state,
    )
    .expect("partial SSE chunk parses");

    assert_eq!(state.full_text, "partial");
    assert_eq!(
        state.execution_evidence(),
        Some(ExecutionEvidence {
            served_model: Some("openai/gpt-5.4-mini".to_string()),
            provider_response_id: Some("gen-partial".to_string()),
            provider_request_id: None,
            reasoning_output_tokens: None,
            provider_finish_reason: None,
        })
    );
}

#[test]
fn openrouter_stream_wire_captures_first_stable_identity_and_terminal_facts() {
    let mut state = ChatStreamState::default();
    for raw in [
        r#"{"id":"","model":"","choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"id":"gen-first","model":"provider/model-a","choices":[{"delta":{}}]}"#,
        r#"{"id":"gen-later","model":"provider/model-b","choices":[{"delta":{},"finish_reason":"stop","native_finish_reason":"stop_sequence"}],"usage":{"completion_tokens_details":{"reasoning_tokens":7}}}"#,
    ] {
        OpenAiCompatibleProvider::process_chat_sse_event(raw, &mut state)
            .expect("SSE chunk parses");
    }

    let evidence = state.execution_evidence().expect("observed evidence");
    assert_eq!(evidence.provider_response_id.as_deref(), Some("gen-first"));
    assert_eq!(evidence.served_model.as_deref(), Some("provider/model-a"));
    assert_eq!(evidence.reasoning_output_tokens, Some(7));
    assert_eq!(
        evidence.provider_finish_reason.as_deref(),
        Some("stop_sequence")
    );
}

fn streamed_request(events: Arc<std::sync::Mutex<Vec<LlmStreamEvent>>>) -> LlmRequest {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.stream_events = Some(LlmEventSender::new(move |event| {
        events.lock_recover().push(event);
    }));
    req
}

fn single_stream_transport(body: &'static str) -> Arc<ScriptedHttpTransport> {
    Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from([(
            200,
            vec![("content-type".to_string(), "text/event-stream".to_string())],
            body,
        )])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}

const TERMINAL_RESPONSES_STREAM: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"second generation\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_second\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_second\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"second generation\"}]}]}}\n\n"
);

fn two_responses_streams(first: &'static str) -> Arc<ScriptedHttpTransport> {
    Arc::new(ScriptedHttpTransport {
        responses: std::sync::Mutex::new(VecDeque::from([
            (
                200,
                vec![("content-type".to_string(), "text/event-stream".to_string())],
                first,
            ),
            (
                200,
                vec![("content-type".to_string(), "text/event-stream".to_string())],
                TERMINAL_RESPONSES_STREAM,
            ),
        ])),
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn responses_handle_does_not_retry_unfinished_tool_arguments() {
    let first = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_partial\",\"call_id\":\"call_partial\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\n",
        r#"data: {"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_partial","delta":"{\"q\":\"paid\"}"}"#,
        "\n\n"
    );
    let transport = two_responses_streams(first);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let result = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await;

    assert_eq!(
        transport.calls(),
        1,
        "paid tool output must not be re-bought"
    );
    let failure = result.expect_err("unfinished paid tool output must stop the ladder");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert!(!failure.retryable);
}

#[tokio::test]
async fn responses_handle_does_not_retry_opaque_reasoning_output() {
    let first = concat!(
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_paid\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_paid\",\"summary\":[],\"encrypted_content\":\"opaque-paid-reasoning\"}}\n\n"
    );
    let transport = two_responses_streams(first);
    let provider = OpenAiProvider::new("key")
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(Arc::clone(&transport) as _);
    let mut handle = ProviderHandle::new(provider.into_components());

    let result = handle
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await;

    assert_eq!(
        transport.calls(),
        1,
        "paid opaque reasoning must not be re-bought"
    );
    let failure = result.expect_err("opaque paid reasoning must stop the ladder");
    assert_eq!(
        failure.code.as_deref(),
        Some("unsafe_retry_after_output_started")
    );
    assert!(!failure.retryable);
}

#[tokio::test]
async fn response_metadata_streaming_body_capture_is_last_wins() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}],\"cost\":0.1}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"cost\":0.2}\n\n",
        "data: [DONE]\n\n"
    );
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![("content-type".to_string(), "text/event-stream".to_string())],
        body,
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(ProviderOptions {
            response_metadata_body_paths: vec!["/cost".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let response = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("terminal stream succeeds and [DONE] is tolerated");

    assert_eq!(response.response_metadata["body:/cost"], json!(0.2));
}

#[tokio::test]
async fn response_metadata_buffered_sse_body_capture_is_last_wins() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}],\"cost\":0.1}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"cost\":0.2}\n\n",
        "data: [DONE]\n\n"
    );
    let transport = Arc::new(RecordingHttpTransport::responding_with(Vec::new(), body));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_options(ProviderOptions {
            response_metadata_body_paths: vec!["/cost".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let response = provider
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect("buffered SSE-shaped response succeeds and [DONE] is tolerated");

    assert_eq!(response.response_metadata["body:/cost"], json!(0.2));
}

#[tokio::test]
async fn response_metadata_headers_are_preserved_on_partial_stream_responses() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    let transport = Arc::new(RecordingHttpTransport::responding_with(
        vec![
            ("content-type".to_string(), "text/event-stream".to_string()),
            ("x-opper-cost".to_string(), "0.000008".to_string()),
        ],
        body,
    ));
    let mut provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
        .with_compat(OpenAiCompat {
            stream_termination: Some(StreamTermination::RequireTerminalEvidence),
            ..OpenAiCompat::default()
        })
        .with_options(ProviderOptions {
            response_metadata_headers: vec!["X-Opper-Cost".to_string()],
            ..ProviderOptions::default()
        })
        .with_transport(transport);

    let error = provider
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("missing terminal evidence returns a partial response");
    let partial = error.partial_response.expect("partial response");

    assert_eq!(
        partial.response_metadata["header:x-opper-cost"],
        json!("0.000008")
    );
}

#[tokio::test]
async fn chat_stream_ending_without_finish_reason_is_retryable_truncation_with_partials() {
    let body = concat!(
        "data: {\"id\":\"gen-partial\",\"model\":\"provider/model\",\"choices\":[{\"delta\":{\"content\":\"partial\",\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n"
    );
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut provider = openrouter_provider()
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default().max_attempts(1),
            ..ProviderOptions::default()
        })
        .with_transport(single_stream_transport(body));

    let error = provider
        .complete(streamed_request(Arc::clone(&events)))
        .await
        .expect_err("missing finish_reason must fail");

    assert_eq!(error.kind, ProviderFailureKind::Stream);
    assert_eq!(
        error.code.as_deref(),
        Some("stream_ended_before_finish_reason")
    );
    assert!(error.retryable);
    let partial = error.partial_response.as_deref().expect("partial response");
    assert_eq!(partial.full_text, "partial");
    assert_eq!(partial.usage.input_tokens, 9);
    assert_eq!(partial.usage.output_tokens, 4);
    assert!(partial.provider_usage.is_some());
    assert!(
        partial
            .parts
            .iter()
            .any(|part| matches!(part, LlmOutputPart::ToolCall { .. }))
    );
    assert!(
        events
            .lock_recover()
            .iter()
            .any(|event| matches!(event, LlmStreamEvent::Part(LlmOutputPart::ToolCall { .. })))
    );
}

#[tokio::test]
async fn chat_stream_with_finish_reason_succeeds_and_eof_tolerated_preserves_compatibility() {
    let terminal_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
    );
    let mut strict = openrouter_provider().with_transport(single_stream_transport(terminal_body));
    let response = strict
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("finish_reason is terminal evidence");
    assert_eq!(response.full_text, "done");
    assert_eq!(response.terminal_reason, LlmTerminalReason::Stop);

    let eof_body = "data: {\"choices\":[{\"delta\":{\"content\":\"legacy\"}}]}\n\n";
    let compat = OpenAiCompat {
        stream_termination: Some(StreamTermination::EofTolerated),
        ..OpenAiCompat::openrouter()
    };
    let mut tolerant = OpenAiCompatibleProvider::new("key", OPENROUTER_BASE_URL)
        .with_compat(compat)
        .with_transport(single_stream_transport(eof_body));
    let response = tolerant
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("explicit EOF tolerance preserves compatibility");
    assert_eq!(response.full_text, "legacy");
}

#[tokio::test]
async fn responses_stream_requires_terminal_event_and_accepts_incomplete_terminal() {
    let partial_body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\",\"response\":{\"id\":\"resp-1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        "data: [DONE]\n\n"
    );
    let mut strict =
        OpenAiProvider::new("key").with_transport(single_stream_transport(partial_body));
    let error = strict
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect_err("Responses requires a terminal event");
    assert_eq!(
        error.code.as_deref(),
        Some("stream_ended_before_terminal_response")
    );
    assert_eq!(
        error
            .partial_response
            .as_deref()
            .map(|partial| partial.full_text.as_str()),
        Some("partial")
    );
    let partial = error.partial_response.as_deref().expect("partial response");
    assert_eq!(partial.usage.input_tokens, 5);
    assert_eq!(partial.usage.output_tokens, 2);
    assert!(partial.provider_usage.is_some());

    let terminal_body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"bounded\"}\n\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-2\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n"
    );
    let mut terminal =
        OpenAiProvider::new("key").with_transport(single_stream_transport(terminal_body));
    let response = terminal
        .complete(streamed_request(Arc::new(
            std::sync::Mutex::new(Vec::new()),
        )))
        .await
        .expect("response.incomplete is terminal evidence");
    assert_eq!(response.full_text, "bounded");
}

#[path = "provider_routing_tests.rs"]
mod provider_routing_tests;

#[test]
fn generation_disposition_reports_what_each_dialect_carried() {
    use crate::common::generation_disposition;
    use lash_core::llm::types::GenerationOptionDisposition::{Applied, OmittedUnsupported};

    let provider = openrouter_provider();
    let mut req = request(vec![LlmMessage::new(
        LlmRole::User,
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    )]);
    enable_cache_control(&mut req, CacheControlDialect::Anthropic);
    req.generation.temperature = Some(lash_core::NonNegativeFiniteF64::new(0.25).unwrap());
    req.generation.seed = Some(7);

    // Chat Completions carries both sampling controls.
    let chat = provider.build_chat_request_body(&req, false).unwrap();
    let chat = generation_disposition(&req, &chat);
    assert_eq!((chat.temperature, chat.seed), (Applied, Applied));
    assert_eq!(chat.cache, Applied);
    assert!(chat.nothing_omitted());

    // Responses has no seed field, so a repeatability request is dropped —
    // silently on the wire, but not in the report.
    let responses = provider.build_responses_request_body(&req, false).unwrap();
    let responses = generation_disposition(&req, &responses);
    assert_eq!(
        (responses.temperature, responses.seed),
        (Applied, OmittedUnsupported)
    );
    assert!(!responses.nothing_omitted());

    let direct = OpenAiProvider::new("key");
    let direct_body = direct.build_responses_request_body(&req, false).unwrap();
    assert_eq!(
        generation_disposition(&req, &direct_body).cache,
        Applied,
        "OpenAI Responses carries prompt-cache intent via prompt_cache_key"
    );
}
