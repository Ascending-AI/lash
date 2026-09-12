use super::*;

#[test]
fn fig1123_remote_llm_request_json_round_trips() {
    let mut model_intent = RemoteModelIntent::new("gpt-test");
    model_intent.capability.reasoning_retention = RemoteReasoningRetentionPolicy {
        capability: Some(RemoteReasoningRetentionCapability::OpenAiContext {
            supported: vec![RemoteOpenAiReasoningContext::CurrentTurn],
        }),
        selection: RemoteReasoningRetentionSelection::OpenAiContext {
            context: RemoteOpenAiReasoningContext::CurrentTurn,
        },
    };
    let request = RemoteLlmRequest {
        instructions: None,
        request_id: "request-1".to_string(),
        scope: RemoteLlmRequestScope::new("session", "session:frame:test", "request-1"),
        model_intent,
        messages: vec![RemoteLlmMessage {
            role: RemoteLlmRole::User,
            content: vec![RemoteLlmContentBlock::Text {
                text: "hello".to_string(),
                response_meta: None,
                cache_breakpoint: false,
            }],
            starts_user_segment: true,
        }],
        tools: Vec::new(),
        tool_choice: RemoteLlmToolChoice::Auto,
        output_spec: Some(RemoteLlmOutputSpec::JsonObject),
        generation: RemoteGenerationOptions {
            output_token_cap: Some(128),
            temperature: Some(serde_json::Number::from_f64(0.25).expect("finite")),
            seed: Some(-9),
            stop_sequences: Vec::new(),
        },
        metadata: HashMap::new(),
    };

    request.validate().expect("valid request");
    let wire = request.encode_json().expect("serialize envelope");
    let decoded = RemoteLlmRequest::decode_json(&wire).expect("version-first decode");
    assert_eq!(decoded.request_id, request.request_id);
    assert_eq!(decoded.scope, request.scope);
    assert_eq!(decoded.messages, request.messages);
}

#[test]
fn current_llm_envelope_rejects_userinfo_in_replay_route_without_echoing_it() {
    let request = RemoteLlmRequest {
        instructions: None,
        request_id: "request-userinfo".to_string(),
        scope: RemoteLlmRequestScope::new("session", "session:frame:test", "request-userinfo"),
        model_intent: RemoteModelIntent::new("gpt-test"),
        messages: vec![RemoteLlmMessage {
            role: RemoteLlmRole::Assistant,
            content: vec![RemoteLlmContentBlock::Text {
                text: "portable answer".to_string(),
                response_meta: Some(RemoteResponseTextMeta {
                    origin: Some(RemoteProviderRouteIdentity {
                        provider: "openai-compatible".to_string(),
                        endpoint: "https://route-user:route-secret@gateway.example/v1".to_string(),
                        model: "gpt-test".to_string(),
                    }),
                    ..Default::default()
                }),
                cache_breakpoint: false,
            }],
            starts_user_segment: false,
        }],
        tools: Vec::new(),
        tool_choice: RemoteLlmToolChoice::Auto,
        output_spec: None,
        generation: RemoteGenerationOptions::default(),
        metadata: HashMap::new(),
    };
    let wire = request
        .encode_json()
        .expect("serialize adversarial request envelope");

    let error = RemoteLlmRequest::decode_json(&wire)
        .expect_err("userinfo-bearing replay routes must fail closed");
    assert!(matches!(error, RemoteProtocolError::InvalidEnvelope { .. }));
    assert!(!error.to_string().contains("route-secret"));
}
