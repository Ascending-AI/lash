use super::*;

pub(super) fn request_with_instructions(
    instructions: &str,
    messages: Vec<LlmMessage>,
) -> LlmRequest {
    let mut req = request(messages);
    req.instructions = Some(Arc::from(instructions));
    req
}

#[test]
fn builds_responses_body_with_instructions_and_input() {
    let provider = OpenAiProvider::new("key");
    let req = request_with_instructions(
        "system prompt",
        vec![LlmMessage::text(LlmRole::User, "hello")],
    );
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
fn runtime_feedback_leading_never_becomes_responses_instructions() {
    let req = request(vec![
        LlmMessage::text(LlmRole::System, "feedback"),
        LlmMessage::text(LlmRole::User, "hello"),
    ]);
    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, true)
        .unwrap();
    assert!(
        body.get("instructions").is_none(),
        "feedback must not become initial instructions: {body}"
    );
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][0]["content"][0]["text"], "feedback");
}

#[test]
fn runtime_feedback_chat_cache_distinguishes_instructions_and_explicit_fences() {
    let provider = OpenAiCompatibleProvider::new("key", "https://provider.test").with_options(
        ProviderOptions {
            cache_retention: CacheRetention::Short,
            ..Default::default()
        },
    );
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "U"),
        LlmMessage::text(LlmRole::System, "F"),
        LlmMessage::text(LlmRole::User, "tail"),
    ]);
    req.model_capability.cache_control = Some(CacheControlDialect::Anthropic);
    let body = provider.build_chat_request_body(&req, false).unwrap();
    assert!(
        body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        body["messages"][2]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    req.messages[1] = LlmMessage::new(
        LlmRole::System,
        vec![LlmContentBlock::Text {
            text: "F".into(),
            response_meta: None,
            cache_breakpoint: true,
        }],
    );
    let (body, diagnostic) = provider
        .build_chat_request_body_with_diagnostics(&req, false)
        .unwrap();
    assert_eq!(diagnostic.requested, 1);
    assert_eq!(diagnostic.emitted, 1);
    assert_eq!(diagnostic.dropped, 0);
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert!(
        body["messages"][2]["content"][0]
            .get("cache_control")
            .is_none()
    );
}
