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

fn refusal_code(error: &LlmTransportError) -> Option<String> {
    error.code.as_ref().map(ToString::to_string)
}

#[test]
fn stop_sequences_reach_chat_and_are_refused_by_responses_and_codex() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model = "anthropic/claude-sonnet-4.6".to_string();
    req.generation.stop_sequences = vec!["</lashlang>".to_string()];

    let (chat, _) = openrouter_provider()
        .build_chat_request_body_with_diagnostics(&req, true)
        .expect("chat body");
    assert_eq!(chat.body["stop"], json!(["</lashlang>"]));
    assert_eq!(
        chat.receipt.stop_sequences,
        lash_core::GenerationOptionOutcome::Applied
    );

    for error in [
        OpenAiProvider::new("key")
            .build_responses_request_body(&req, true)
            .expect_err("Responses has no stop field"),
        CodexProvider::build_request_body(&CodexProvider::new("token", "refresh", 0), &req, true)
            .expect_err("Codex has no stop field"),
    ] {
        assert_eq!(
            refusal_code(&error).as_deref(),
            Some("lash:unsupported_generation_option")
        );
        assert!(
            error.message.contains("stop_sequences"),
            "{}",
            error.message
        );
        assert!(!error.is_retryable());
    }
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
        attachment_acceptance: Default::default(),
        reasoning: Some(ReasoningCapability {
            efforts: vec!["high".to_string()],
            encoding: ReasoningEncoding::Effort,
            disable: false,
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
                429,
                vec![("retry-after".to_string(), "0".to_string())],
                r#"{"error":{"message":"temporarily throttled"}}"#,
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
fn responses_body_carries_temperature_and_refuses_a_seed() {
    let mut req = sampled_request();
    req.model = "gpt-5.4".to_string();

    let error = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .expect_err("the Responses endpoint has no seed field");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:unsupported_generation_option")
    );
    assert!(error.message.contains("seed"), "{}", error.message);

    req.generation.seed = None;
    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .unwrap();
    assert_eq!(body["temperature"], json!(0.0));
}

#[test]
fn codex_refuses_every_sampling_control_and_the_cap() {
    let mut cases: Vec<(&str, LlmRequest, ProviderOptions)> = Vec::new();
    let mut temperature = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    temperature.generation.temperature = Some(NonNegativeFiniteF64::new(0.0).expect("finite"));
    cases.push(("temperature", temperature, ProviderOptions::default()));
    let mut seed = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    seed.generation.seed = Some(1);
    cases.push(("seed", seed, ProviderOptions::default()));
    let mut cap = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    cap.generation.output_token_cap = NonZeroUsize::new(1_024);
    cases.push(("output_token_cap", cap, ProviderOptions::default()));
    cases.push((
        "output_token_cap",
        request(vec![LlmMessage::text(LlmRole::User, "hello")]),
        ProviderOptions {
            max_output_tokens: Some(1_024),
            ..ProviderOptions::default()
        },
    ));
    for (setting, req, options) in cases {
        let error = CodexProvider::new("access", "refresh", 0)
            .with_options(options)
            .build_request_body(&req, false)
            .expect_err(setting);
        assert_eq!(
            refusal_code(&error).as_deref(),
            Some("lash:unsupported_generation_option"),
            "{setting}"
        );
        assert!(error.message.contains(setting), "{}", error.message);
    }
}

/// A transport that fails the test if a refused call reaches it.
#[derive(Debug, Default)]
struct CountingTransport {
    sends: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmHttpTransport for CountingTransport {
    async fn send(
        &self,
        _request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(LlmTransportError::new(
            "a refused call reached the transport",
        ))
    }
}

fn tool(name: &str) -> LlmToolSpec {
    LlmToolSpec {
        name: name.to_string(),
        description: name.to_string(),
        input_schema: json!({ "type": "object", "properties": {} }).into(),
        output_schema: json!({}).into(),
    }
}

/// Every setting an OpenAI-compatible wire cannot carry is refused with a
/// typed, non-retryable failure before the transport sees a byte.
#[tokio::test]
async fn unsupported_settings_are_refused_before_any_transport_call() {
    fn with(edit: impl FnOnce(&mut LlmRequest)) -> LlmRequest {
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        edit(&mut req);
        req
    }
    let pinned = |req: &mut LlmRequest| {
        req.generation.temperature = Some(NonNegativeFiniteF64::new(0.2).expect("finite"));
        req.model_capability.sampling = lash_core::SamplingCapability::Pinned;
    };
    // (label, endpoint is Responses, compat, options, request, expected code)
    let cases: Vec<(&str, bool, OpenAiCompat, ProviderOptions, LlmRequest, &str)> = vec![
        (
            "chat pinned temperature",
            false,
            OpenAiCompat::openrouter(),
            ProviderOptions::default(),
            with(pinned),
            "lash:unsupported_generation_option",
        ),
        (
            "responses pinned temperature",
            true,
            OpenAiCompat::default(),
            ProviderOptions::default(),
            with(pinned),
            "lash:unsupported_generation_option",
        ),
        (
            "chat parallel_tool_calls without tools",
            false,
            OpenAiCompat::openrouter(),
            ProviderOptions::default(),
            with(|req| req.generation.parallel_tool_calls = Some(false)),
            "lash:unsupported_generation_option",
        ),
        (
            "local parallel_tool_calls",
            true,
            OpenAiCompat::local(),
            ProviderOptions::default(),
            with(|req| {
                req.tools = Arc::new(vec![tool("lookup")]);
                req.generation.parallel_tool_calls = Some(false);
            }),
            "lash:unsupported_generation_option",
        ),
        (
            "cap on an endpoint without a cap field",
            false,
            OpenAiCompat {
                max_tokens_field: Some(OpenAiCompatMaxTokensField::Omit),
                ..OpenAiCompat::default()
            },
            ProviderOptions::default(),
            with(|req| req.generation.output_token_cap = NonZeroUsize::new(64)),
            "lash:unsupported_generation_option",
        ),
        (
            "responses seed",
            true,
            OpenAiCompat::default(),
            ProviderOptions::default(),
            with(|req| req.generation.seed = Some(3)),
            "lash:unsupported_generation_option",
        ),
        (
            "responses stop sequences",
            true,
            OpenAiCompat::default(),
            ProviderOptions::default(),
            with(|req| req.generation.stop_sequences = vec!["END".to_string()]),
            "lash:unsupported_generation_option",
        ),
        (
            "no dialect for an explicit effort",
            false,
            OpenAiCompat::default(),
            ProviderOptions::default(),
            with(|req| {
                req.model_capability = reasoning_capability();
                req.model_variant =
                    lash_core::provider::ReasoningSelection::Effort("high".to_string());
            }),
            "lash:reasoning_encoding_unrepresentable",
        ),
        (
            "inexact effort",
            false,
            OpenAiCompat::openrouter(),
            ProviderOptions::default(),
            with(|req| {
                req.model_capability = reasoning_capability();
                req.model_variant =
                    lash_core::provider::ReasoningSelection::Effort("High".to_string());
            }),
            "lash:unsupported_effort",
        ),
    ];
    for (label, responses, compat, options, req, code) in cases {
        let transport = Arc::new(CountingTransport::default());
        let provider = OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
            .with_compat(compat)
            .with_options(options)
            .with_transport(transport.clone());
        let error = if responses {
            let mut provider = OpenAiProvider { inner: provider };
            provider.complete(req).await
        } else {
            let mut provider = provider;
            provider.complete(req).await
        }
        .expect_err(label);
        assert_eq!(refusal_code(&error).as_deref(), Some(code), "{label}");
        assert!(!error.is_retryable(), "{label}");
        assert_eq!(
            transport.sends.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{label} reached the transport"
        );
    }
}

#[test]
fn no_cap_verbosity_or_parallel_tool_calls_are_sent_unless_the_host_sets_them() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.tools = Arc::new(vec![tool("lookup")]);

    let chat = openrouter_provider()
        .build_chat_request_body(&req, true)
        .unwrap();
    let responses = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .unwrap();
    let codex = CodexProvider::new("access", "refresh", 0)
        .build_request_body(&req, true)
        .unwrap();
    for (label, body) in [
        ("chat", &chat),
        ("responses", &responses),
        ("codex", &codex),
    ] {
        for field in [
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
            "parallel_tool_calls",
            "text",
            "temperature",
        ] {
            assert!(body.get(field).is_none(), "{label} sent `{field}`");
        }
    }
    // Replay mechanics stay.
    assert_eq!(responses["store"], json!(false));
    assert_eq!(responses["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(codex["store"], json!(false));
    assert_eq!(codex["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn parallel_tool_calls_is_sent_as_the_host_set_it() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.tools = Arc::new(vec![tool("lookup")]);
    req.generation.parallel_tool_calls = Some(false);

    let (chat, _) = openrouter_provider()
        .build_chat_request_body_with_diagnostics(&req, true)
        .unwrap();
    assert_eq!(chat.body["parallel_tool_calls"], json!(false));
    assert_eq!(
        chat.receipt.parallel_tool_calls,
        lash_core::GenerationOptionOutcome::Applied
    );
    let responses = OpenAiProvider::new("key")
        .build_responses_request(&req, true)
        .unwrap();
    assert_eq!(responses.body["parallel_tool_calls"], json!(false));
    let codex = CodexProvider::new("access", "refresh", 0)
        .build_request(&req, true)
        .unwrap();
    assert_eq!(codex.body["parallel_tool_calls"], json!(false));
    assert_eq!(
        codex.receipt.parallel_tool_calls,
        lash_core::GenerationOptionOutcome::Applied
    );
}

#[test]
fn expose_thinking_requests_a_summary_on_responses_and_codex_even_without_effort() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let options = ProviderOptions {
        expose_thinking: true,
        ..ProviderOptions::default()
    };
    let responses = OpenAiProvider::new("key")
        .with_options(options.clone())
        .build_responses_request(&req, true)
        .unwrap();
    let codex = CodexProvider::new("access", "refresh", 0)
        .with_options(options)
        .build_request(&req, true)
        .unwrap();
    for built in [responses, codex] {
        assert_eq!(built.body["reasoning"], json!({ "summary": "auto" }));
        assert_eq!(
            built.receipt.thinking_summary,
            lash_core::GenerationOptionOutcome::Applied
        );
        assert_eq!(
            built.receipt.thinking_visibility,
            lash_core::GenerationOptionOutcome::Applied
        );
        assert_eq!(
            built.receipt.reasoning,
            lash_core::GenerationOptionOutcome::NotRequested
        );
    }
}

#[test]
fn expose_thinking_on_chat_is_local_visibility_only() {
    let req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    let (chat, _) = openrouter_provider()
        .with_options(ProviderOptions {
            expose_thinking: true,
            ..ProviderOptions::default()
        })
        .build_chat_request_body_with_diagnostics(&req, true)
        .expect("expose_thinking is not refused on Chat");
    assert!(chat.body.get("reasoning").is_none());
    assert!(chat.body.get("reasoning_effort").is_none());
    assert_eq!(
        chat.receipt.thinking_summary,
        lash_core::GenerationOptionOutcome::NotRequested,
        "Chat has no summary flag to request"
    );
    assert_eq!(
        chat.receipt.thinking_visibility,
        lash_core::GenerationOptionOutcome::Applied
    );
    assert!(chat.receipt.fully_honored());
}

#[test]
fn codex_speaks_responses_in_the_openai_reasoning_dialect() {
    let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    req.model_capability = reasoning_capability();
    let codex = CodexProvider::new("access", "refresh", 0);

    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    let body = codex.build_request_body(&req, true).unwrap();
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));

    req.model_variant = lash_core::provider::ReasoningSelection::Disabled;
    let body = codex.build_request_body(&req, true).unwrap();
    assert_eq!(body["reasoning"], json!({ "effort": "none" }));

    req.model_capability = budget_reasoning_capability();
    req.model_variant = lash_core::provider::ReasoningSelection::Effort("high".to_string());
    let error = codex
        .build_request_body(&req, true)
        .expect_err("Codex has no reasoning token-budget field");
    assert_eq!(
        refusal_code(&error).as_deref(),
        Some("lash:reasoning_encoding_unrepresentable")
    );
}
