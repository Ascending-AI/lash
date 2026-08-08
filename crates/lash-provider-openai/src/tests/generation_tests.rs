use super::*;
use crate::CodexProvider;
use lash_core::NonNegativeFiniteF64;
use lash_sansio::sync::MutexExt;

#[test]
fn output_token_cap_maps_to_wire_fields() {
    let options = ProviderOptions {
        max_output_tokens: Some(9999),
        ..ProviderOptions::default()
    };
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.output_token_cap = NonZeroUsize::new(2048);

    let responses_body = OpenAiProvider::new("key")
        .with_options(options.clone())
        .build_responses_request_body(&req, true)
        .unwrap();
    assert_eq!(responses_body["max_output_tokens"], 2048);
    let provider_limited_responses_body = OpenAiProvider::new("key")
        .with_options(options.clone())
        .build_responses_request_body(
            &request(vec![LlmMessage::text(LlmRole::User, "hello")]),
            true,
        )
        .unwrap();
    assert_eq!(provider_limited_responses_body["max_output_tokens"], 9999);

    let mut chat_req = req;
    chat_req.model = "anthropic/claude-sonnet-4.6".to_string();
    let chat_body = openrouter_provider()
        .with_options(options.clone())
        .build_chat_request_body(&chat_req, true)
        .unwrap();
    assert_eq!(chat_body["max_tokens"], 2048);
    let mut provider_limited_chat_req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    provider_limited_chat_req.model = "anthropic/claude-sonnet-4.6".to_string();
    let provider_limited_chat_body = openrouter_provider()
        .with_options(options)
        .build_chat_request_body(&provider_limited_chat_req, true)
        .unwrap();
    assert_eq!(provider_limited_chat_body["max_tokens"], 9999);
}

#[test]
fn stop_sequences_reach_chat_but_are_omitted_by_responses_and_codex() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "anthropic/claude-sonnet-4.6".to_string();
    req.generation.stop_sequences = vec!["</lashlang>".to_string()];

    let chat = openrouter_provider()
        .build_chat_request_body(&req, true)
        .expect("chat body");
    assert_eq!(chat["stop"], json!(["</lashlang>"]));
    assert_eq!(
        crate::common::generation_disposition(&req, &chat).stop_sequences,
        lash_core::GenerationOptionDisposition::Applied
    );

    let responses = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .expect("responses body");
    assert!(responses.get("stop").is_none());
    assert_eq!(
        crate::common::generation_disposition(&req, &responses).stop_sequences,
        lash_core::GenerationOptionDisposition::OmittedUnsupported
    );

    let codex =
        CodexProvider::build_request_body(&CodexProvider::new("token", "refresh", 0), &req, true)
            .expect("codex body");
    assert!(codex.get("stop").is_none());
}

/// Records every outgoing HTTP body while replaying a scripted status
/// sequence, so a retried call can be inspected attempt by attempt.
#[derive(Debug)]
struct RecordingScriptedTransport {
    bodies: std::sync::Mutex<Vec<String>>,
    responses: std::sync::Mutex<VecDeque<ScriptedHttpResponse>>,
}

#[async_trait]
impl LlmHttpTransport for RecordingScriptedTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.bodies
            .lock_recover()
            .push(String::from_utf8(request.body.to_vec()).expect("utf-8 body"));
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

fn sampled_request() -> LlmRequest {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.generation.temperature = Some(NonNegativeFiniteF64::new(0.0).expect("finite temperature"));
    req.generation.seed = Some(-42);
    req
}

#[test]
fn chat_body_carries_temperature_and_seed_on_both_buffered_and_streaming_paths() {
    let provider = openrouter_provider();
    let req = sampled_request();

    for stream in [false, true] {
        let body = provider.build_chat_request_body(&req, stream).unwrap();
        assert_eq!(body["temperature"], json!(0.0));
        assert_eq!(body["seed"], json!(-42));
    }
}

#[test]
fn chat_body_omits_temperature_and_seed_when_the_caller_sets_neither() {
    let provider = openrouter_provider();
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);

    let body = provider.build_chat_request_body(&req, true).unwrap();
    assert!(body.get("temperature").is_none());
    assert!(body.get("seed").is_none());
}

#[test]
fn sampling_controls_do_not_disturb_the_rest_of_the_chat_body() {
    let mut req = sampled_request();
    req.model = "anthropic/claude-sonnet-4.6".to_string();
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "answer".to_string(),
        schema: json!({ "type": "object", "properties": {} }).into(),
        strict: true,
    }));
    req.tools = Arc::new(vec![LlmToolSpec {
        name: "lookup".to_string(),
        description: "look something up".to_string(),
        input_schema: json!({ "type": "object", "properties": {} }).into(),
        output_schema: json!({}).into(),
    }]);
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    req.model_capability = ModelCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["high".to_string()],
            default_effort: None,
            aliases: BTreeMap::new(),
            encoding: ReasoningEncoding::Effort,
            disable: None,
            mandatory: false,
        }),
        ..ModelCapability::default()
    };

    let body = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();

    assert_eq!(body["temperature"], json!(0.0));
    assert_eq!(body["seed"], json!(-42));
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "answer");
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
    assert_eq!(body["stream_options"], json!({ "include_usage": true }));
    assert_eq!(body["tools"][0]["function"]["name"], "lookup");
    assert_eq!(body["tool_choice"], "auto");
}

#[tokio::test]
async fn every_retry_attempt_reapplies_the_sampling_controls() {
    let transport = Arc::new(RecordingScriptedTransport {
        bodies: std::sync::Mutex::new(Vec::new()),
        responses: std::sync::Mutex::new(VecDeque::from([
            (
                503,
                Vec::new(),
                r#"{"error":{"message":"temporarily unavailable"}}"#,
            ),
            (
                200,
                Vec::new(),
                r#"{"id":"gen-1","model":"m","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#,
            ),
        ])),
    });
    let provider = openrouter_provider()
        .with_options(ProviderOptions {
            reliability: ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..ProviderOptions::default()
        })
        .with_transport(transport.clone());
    let mut handle = ProviderHandle::new(provider.into_components());

    let completion = handle
        .complete(sampled_request())
        .await
        .expect("retry succeeds");

    assert_eq!(completion.call_record.attempts.len(), 2);
    let bodies = transport.bodies.lock_recover().clone();
    assert_eq!(bodies.len(), 2);
    for body in bodies {
        let value: Value = serde_json::from_str(&body).expect("request body json");
        assert_eq!(value["temperature"], json!(0.0));
        assert_eq!(value["seed"], json!(-42));
    }
}

#[test]
fn responses_body_carries_temperature_but_never_a_seed() {
    let mut req = sampled_request();
    req.model = "gpt-5.4".to_string();

    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .unwrap();

    assert_eq!(body["temperature"], json!(0.0));
    // The Responses endpoint has no seed field.
    assert!(body.get("seed").is_none());
}

#[test]
fn codex_request_omits_both_sampling_controls() {
    // Neither control is emitted on the Codex dialect, the same treatment it
    // gives token caps. Whether this backend would accept `temperature` is
    // unverified — no live probe has been run against it — so omission is the
    // conservative side of emit-or-omit: a dropped temperature is visible in
    // the request-body receipt, while an unsupported field would 400 every
    // Codex call. Revisit with an authenticated request, not with docs.
    let body = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&sampled_request(), false)
        .unwrap();

    assert!(body.get("temperature").is_none());
    assert!(body.get("seed").is_none());
}
