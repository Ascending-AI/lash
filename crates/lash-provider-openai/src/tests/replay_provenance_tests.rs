use super::*;

fn adversarial_raw_request() -> LlmRequest {
    request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![
            LlmContentBlock::Text {
                text: "portable answer".into(),
                response_meta: Some(lash_core::llm::types::ResponseTextMeta {
                    id: Some("unstamped-openai-wire-id".to_string()),
                    provider_payload: Some("unstamped-openai-wire-payload".to_string()),
                    ..Default::default()
                }),
                cache_breakpoint: false,
            },
            LlmContentBlock::Reasoning {
                text: "portable summary".to_string(),
                replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                    encrypted_content: Some("foreign-openai-wire-reasoning".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        "anthropic",
                        "https://api.anthropic.com",
                        "claude-sonnet-4-6",
                    )),
                    ..Default::default()
                }),
            },
            LlmContentBlock::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: Some(ProviderReplayMeta {
                    item_id: Some("foreign-openai-wire-tool-id".to_string()),
                    opaque: Some("foreign-openai-wire-tool-opaque".to_string()),
                    origin: Some(ProviderRouteIdentity::for_endpoint(
                        "google_oauth",
                        "https://cloudcode-pa.googleapis.com/v1internal",
                        "gemini-2.5-pro",
                    )),
                }),
            },
        ],
    )])
}

fn assert_adversarial_replay_absent(wire: &str) {
    assert!(wire.contains("portable answer"));
    assert!(!wire.contains("unstamped-openai-wire-id"));
    assert!(!wire.contains("unstamped-openai-wire-payload"));
    assert!(!wire.contains("foreign-openai-wire-reasoning"));
    assert!(!wire.contains("foreign-openai-wire-tool-id"));
    assert!(!wire.contains("foreign-openai-wire-tool-opaque"));
}

#[tokio::test]
async fn raw_provider_complete_filters_chat_wire_capture() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = openrouter_provider().with_transport(transport.clone());
    Provider::complete(&mut provider, adversarial_raw_request())
        .await
        .expect("raw Chat completion");
    let requests = transport.requests.lock_recover();
    assert_eq!(requests.len(), 1);
    assert_adversarial_replay_absent(&String::from_utf8_lossy(&requests[0].body));
}

#[tokio::test]
async fn raw_provider_complete_filters_responses_wire_capture() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider = OpenAiProvider::new("key").with_transport(transport.clone());
    Provider::complete(&mut provider, adversarial_raw_request())
        .await
        .expect("raw Responses completion");
    let requests = transport.requests.lock_recover();
    assert_eq!(requests.len(), 1);
    assert_adversarial_replay_absent(&String::from_utf8_lossy(&requests[0].body));
}

#[tokio::test]
async fn raw_provider_complete_rejects_endpoint_userinfo_before_transport() {
    let transport = Arc::new(RecordingHttpTransport::default());
    let mut provider =
        OpenAiCompatibleProvider::new("key", "https://route-user:route-secret@gateway.example/v1")
            .with_transport(transport.clone());

    let error = Provider::complete(&mut provider, adversarial_raw_request())
        .await
        .expect_err("userinfo-bearing routes must fail closed");
    assert_eq!(error.code.as_deref(), Some("invalid_provider_endpoint"));
    assert_eq!(error.kind, ProviderFailureKind::Validation);
    assert!(!error.retryable);
    assert!(!error.to_string().contains("route-secret"));
    assert!(transport.requests.lock_recover().is_empty());
}

#[test]
fn chat_body_replays_openrouter_reasoning_details_on_tool_calls() {
    let provider = openrouter_provider();
    let native_route = provider.route_identity("openai/gpt-5.4");
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
                origin: Some(native_route),
            }),
        }],
    )]);

    let body = provider.build_chat_request_body(&req, false).unwrap();

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
fn chat_body_keeps_tool_call_but_drops_foreign_opaque_replay() {
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
                        "data": "foreign"
                    })
                    .to_string(),
                ),
                origin: Some(ProviderRouteIdentity::for_endpoint(
                    "google_oauth",
                    "https://cloudcode-pa.googleapis.com/v1internal",
                    "gemini-2.5-pro",
                )),
            }),
        }],
    )]);

    let body = openrouter_provider()
        .build_chat_request_body(&req, false)
        .unwrap();

    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert!(body["messages"][0].get("reasoning_details").is_none());
}

#[test]
fn raw_chat_builder_drops_unstamped_opaque_replay() {
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call_1".to_string(),
            tool_name: "lookup".to_string(),
            input_json: "{}".to_string(),
            replay: Some(ProviderReplayMeta {
                item_id: None,
                opaque: Some("unstamped-chat-opaque".to_string()),
                origin: None,
            }),
        }],
    )]);

    let body = openrouter_provider()
        .build_chat_request_body(&req, false)
        .expect("chat body");
    assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert!(!body.to_string().contains("unstamped-chat-opaque"));
}

#[test]
fn responses_body_demotes_foreign_reasoning_to_neutral_text() {
    let provider = OpenAiProvider::new("key");
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "neutral summary".to_string(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                item_id: Some("foreign-reasoning".to_string()),
                encrypted_content: Some("foreign-encrypted".to_string()),
                origin: Some(ProviderRouteIdentity::for_endpoint(
                    "anthropic",
                    "https://api.anthropic.com",
                    "claude-sonnet-4-6",
                )),
                ..Default::default()
            }),
        }],
    )]);

    let body = provider.build_responses_request_body(&req, false).unwrap();

    assert_eq!(body["input"][0]["type"], "message");
    assert_eq!(body["input"][0]["content"][0]["text"], "neutral summary");
    assert!(!body.to_string().contains("foreign-encrypted"));
}

#[test]
fn raw_responses_builder_drops_unstamped_response_item_identity_and_phase() {
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Text {
            text: "portable answer".into(),
            response_meta: Some(lash_core::llm::types::ResponseTextMeta {
                id: Some("foreign-response-item".to_string()),
                status: Some("in_progress".to_string()),
                phase: Some("analysis".to_string()),
                provider_payload: Some("foreign-response-payload".to_string()),
                origin: None,
                ..Default::default()
            }),
            cache_breakpoint: false,
        }],
    )]);

    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, false)
        .expect("responses body");
    let wire = body.to_string();
    assert!(wire.contains("portable answer"));
    assert!(!wire.contains("foreign-response-item"));
    assert!(!wire.contains("in_progress"));
    assert!(!wire.contains("analysis"));
    assert!(!wire.contains("foreign-response-payload"));
}

#[test]
fn responses_body_replays_same_route_reasoning_natively() {
    let provider = OpenAiProvider::new("key");
    let native_route = provider.route_identity("openai/gpt-5.4");
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::Reasoning {
            text: "native summary".to_string(),
            replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                item_id: Some("reasoning-1".to_string()),
                encrypted_content: Some("native-encrypted".to_string()),
                summary: vec!["native summary".to_string()],
                origin: Some(native_route),
                ..Default::default()
            }),
        }],
    )]);

    let body = provider.build_responses_request_body(&req, false).unwrap();

    assert_eq!(body["input"][0]["type"], "reasoning");
    assert_eq!(body["input"][0]["encrypted_content"], "native-encrypted");
}

#[tokio::test]
async fn openai_chat_and_responses_stamp_fresh_replay_with_the_minting_route() {
    const CHAT_BODY: &str = r#"{
        "model":"served-chat",
        "choices":[{
            "message":{
                "role":"assistant",
                "tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{}"}}],
                "reasoning_details":[{"type":"reasoning.encrypted","id":"call-1","data":"opaque"}]
            },
            "finish_reason":"tool_calls"
        }]
    }"#;
    let mut chat = OpenAiCompatibleProvider::new("key", "https://example.invalid/v1")
        .with_transport(Arc::new(RecordingHttpTransport::responding_with(
            Vec::new(),
            CHAT_BODY,
        )));
    let chat_response = Provider::complete(
        &mut chat,
        request(vec![LlmMessage::text(LlmRole::User, "go")]),
    )
    .await
    .expect("chat response parses");
    let chat_replay = chat_response
        .parts
        .iter()
        .find_map(|part| match part {
            LlmOutputPart::ToolCall { replay, .. } => replay.as_ref(),
            _ => None,
        })
        .expect("chat tool replay");
    assert_eq!(
        chat_replay.origin.as_ref(),
        Some(&chat.route_identity("openai/gpt-5.4"))
    );

    const RESPONSES_BODY: &str = r#"{
        "id":"resp-1",
        "status":"completed",
        "output":[{
            "type":"reasoning",
            "id":"reasoning-1",
            "summary":[{"type":"summary_text","text":"summary"}],
            "encrypted_content":"encrypted"
        }]
    }"#;
    let mut responses = OpenAiProvider::new("key").with_transport(Arc::new(
        RecordingHttpTransport::responding_with(Vec::new(), RESPONSES_BODY),
    ));
    let responses_response = Provider::complete(
        &mut responses,
        request(vec![LlmMessage::text(LlmRole::User, "go")]),
    )
    .await
    .expect("Responses response parses");
    let responses_replay = responses_response
        .parts
        .iter()
        .find_map(|part| match part {
            LlmOutputPart::Reasoning { replay, .. } => replay.as_ref(),
            _ => None,
        })
        .expect("Responses reasoning replay");
    assert_eq!(
        responses_replay.origin.as_ref(),
        Some(&responses.route_identity("openai/gpt-5.4"))
    );
}
