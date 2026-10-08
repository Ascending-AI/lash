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
    Provider::complete(
        &mut provider,
        adversarial_raw_request(),
        &lash_core::provider::NoSlotDeliveries,
        &lash_core::provider::LiveCallHorizon::fixture(),
    )
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
    Provider::complete(
        &mut provider,
        adversarial_raw_request(),
        &lash_core::provider::NoSlotDeliveries,
        &lash_core::provider::LiveCallHorizon::fixture(),
    )
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

    let error = Provider::complete(
        &mut provider,
        adversarial_raw_request(),
        &lash_core::provider::NoSlotDeliveries,
        &lash_core::provider::LiveCallHorizon::fixture(),
    )
    .await
    .expect_err("userinfo-bearing routes must fail closed");
    assert_eq!(
        error.code.as_ref().map(|code| code.to_string()),
        Some("lash:invalid_provider_endpoint".to_string())
    );
    assert_eq!(error.kind, ProviderFailureKind::Validation);
    assert!(!error.is_retryable());
    assert!(!error.to_string().contains("route-secret"));
    assert!(transport.requests.lock_recover().is_empty());
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
        &lash_core::provider::NoSlotDeliveries,
        &lash_core::provider::LiveCallHorizon::fixture(),
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
        &lash_core::provider::NoSlotDeliveries,
        &lash_core::provider::LiveCallHorizon::fixture(),
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
