use super::*;
use lash_core::llm::transport::ProviderFailureKind;

pub(super) fn request_with_instructions(
    instructions: &str,
    messages: Vec<LlmMessage>,
) -> LlmRequest {
    let mut req = request(messages);
    req.instructions = Some(Arc::from(instructions));
    req
}

#[test]
fn structured_output_uses_native_output_config_format() {
    let provider = AnthropicProvider::new("key");
    let mut req = request_with_instructions(
        "system prompt",
        vec![LlmMessage::text(LlmRole::User, "extract")],
    );
    req.output_spec = Some(LlmOutputSpec::JsonSchema(LlmJsonSchema {
        name: "extract_result".to_string(),
        strict: true,
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["answer"],
            "properties": {
                "answer": { "type": "string" }
            }
        })
        .into(),
    }));

    let body = provider.build_request_body(&req).expect("body");

    assert_eq!(
        body["output_config"]["format"],
        json!({
            "type": "json_schema",
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["answer"],
                "properties": {
                    "answer": { "type": "string" }
                }
            }
        })
    );
    let system_text = body["system"][0]["text"].as_str().unwrap_or_default();
    assert_eq!(system_text, "system prompt");
    assert!(!system_text.contains("Respond with a single JSON object"));
}

#[test]
fn runtime_feedback_native_trailing_section_retains_conversation_cache() {
    let mut req = request_with_instructions(
        "I",
        vec![
            LlmMessage::text(LlmRole::User, "U"),
            LlmMessage::text(LlmRole::System, "F"),
        ],
    );
    req.model_capability.native_mid_conversation_system = true;
    let body = AnthropicProvider::new("key")
        .build_request_body(&req)
        .unwrap();
    assert_eq!(body["messages"][1]["role"], "system");
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({"type":"ephemeral"})
    );
}

#[test]
fn runtime_feedback_native_nontext_and_empty_messages_use_tagged_fallback() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "U"),
        LlmMessage::new(
            LlmRole::System,
            vec![
                LlmContentBlock::Text {
                    text: "F".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
                LlmContentBlock::Attachment {
                    source: Box::new(AttachmentSource::inline(
                        lash_core::MediaType::parse("image/png").unwrap(),
                        vec![1, 2, 3],
                    )),
                },
            ],
        ),
    ]);
    req.model_capability.native_mid_conversation_system = true;
    let body = AnthropicProvider::new("key")
        .build_request_body(&req)
        .unwrap();
    assert!(body.get("system").is_none());
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"][1]["text"],
        "<runtime_feedback>F</runtime_feedback>"
    );
    assert_eq!(body["messages"][0]["content"][2]["type"], "image");
    req.messages[1] = LlmMessage::text(LlmRole::System, "");
    let body = AnthropicProvider::new("key")
        .build_request_body(&req)
        .unwrap();
    assert_eq!(
        body["messages"][0]["content"][1]["text"],
        "<runtime_feedback></runtime_feedback>"
    );
}

#[test]
fn runtime_feedback_native_sections_respect_neighboring_fallback_blocks() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "U"),
        LlmMessage::text(LlmRole::System, "before"),
        LlmMessage::text(LlmRole::System, ""),
        LlmMessage::text(LlmRole::System, "after"),
        LlmMessage::text(LlmRole::Assistant, "A"),
    ]);
    req.model_capability.native_mid_conversation_system = true;
    let body = AnthropicProvider::new("key")
        .build_request_body(&req)
        .unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"][1]["text"],
        "<runtime_feedback>before</runtime_feedback>"
    );
    assert_eq!(
        body["messages"][0]["content"][2]["text"],
        "<runtime_feedback></runtime_feedback>"
    );
    assert_eq!(body["messages"][1]["role"], "system");
    assert_eq!(body["messages"][1]["content"][0]["text"], "after");
    assert_eq!(body["messages"][2]["role"], "assistant");
}

#[test]
fn runtime_feedback_native_does_not_drop_whitespace_text_blocks() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "U"),
        LlmMessage::new(
            LlmRole::System,
            ["a", " \n", "b"]
                .into_iter()
                .map(|text| LlmContentBlock::Text {
                    text: text.into(),
                    response_meta: None,
                    cache_breakpoint: false,
                })
                .collect(),
        ),
    ]);
    req.model_capability.native_mid_conversation_system = true;
    let body = AnthropicProvider::new("key")
        .build_request_body(&req)
        .unwrap();
    assert_eq!(
        body["messages"][0]["content"][1]["text"],
        "<runtime_feedback>a \nb</runtime_feedback>"
    );
}

#[test]
fn runtime_feedback_result_order_preserves_explicit_cache_marker() {
    let mut req = request(vec![
        LlmMessage::text(LlmRole::User, "U"),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::ToolCall {
                call_id: "call1".into(),
                tool_name: "lookup".into(),
                input_json: "{}".into(),
                replay: None,
            }],
        ),
        LlmMessage::new(
            LlmRole::System,
            vec![LlmContentBlock::Text {
                text: "F".into(),
                response_meta: None,
                cache_breakpoint: true,
            }],
        ),
        LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::ToolResult {
                call_id: "call1".into(),
                tool_name: Some("lookup".into()),
                content: "RESULT".into(),
            }],
        ),
    ]);
    req.model_capability.native_mid_conversation_system = true;
    let provider = AnthropicProvider::new("key").with_options(ProviderOptions {
        cache_retention: CacheRetention::Short,
        ..Default::default()
    });
    let body = provider.build_request_body(&req).unwrap();
    let parts = body["messages"][2]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "tool_result");
    assert!(parts[0].get("cache_control").is_none());
    assert_eq!(parts[1]["text"], "<runtime_feedback>F</runtime_feedback>");
    assert_eq!(parts[1]["cache_control"], json!({"type":"ephemeral"}));
}

#[test]
fn malformed_tool_call_input_json_fails_the_anthropic_request() {
    let provider = AnthropicProvider::new("key");
    let req = request(vec![LlmMessage::new(
        LlmRole::Assistant,
        vec![LlmContentBlock::ToolCall {
            call_id: "call1".into(),
            tool_name: "lookup".into(),
            input_json: "{".into(),
            replay: None,
        }],
    )]);

    let error = provider
        .build_request_body(&req)
        .expect_err("malformed tool input must not become {}");
    assert_eq!(error.kind, ProviderFailureKind::Validation);
    assert_eq!(error.code.as_deref(), Some("invalid_tool_call_input_json"));
    assert!(error.message.contains("lookup"));
    assert_eq!(error.raw.as_deref().map(String::as_str), Some("{"));
}
