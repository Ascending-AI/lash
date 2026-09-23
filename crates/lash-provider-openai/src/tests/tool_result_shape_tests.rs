//! FIG-3515: one tool call is answered by exactly one tool output on every
//! OpenAI wire, carrying the result's text and image in order where the wire
//! allows and a documented degradation where it does not.
use super::*;
use lash_core::facade_support::ModelToolReturnPart;

const PNG: &[u8] = &[1, 2, 3, 4];

fn text_image_text_request() -> LlmRequest {
    request(vec![
        LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::ToolCall {
                call_id: "call_1".into(),
                tool_name: "shot".into(),
                input_json: "{}".into(),
                replay: None,
            }],
        ),
        LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::ToolResult {
                call_id: "call_1".into(),
                tool_name: Some("shot".into()),
                content: vec![
                    ModelToolReturnPart::text("[\"before\","),
                    ModelToolReturnPart::Attachment(AttachmentSource::inline(
                        lash_core::MediaType::parse("image/png").unwrap(),
                        PNG.to_vec(),
                    )),
                    ModelToolReturnPart::text(",\"after\"]"),
                ],
            }],
        ),
    ])
}

fn data_url() -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(PNG)
    )
}

fn function_call_outputs(body: &Value) -> Vec<&Value> {
    body["input"]
        .as_array()
        .expect("input array")
        .iter()
        .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "call_1")
        .collect()
}

#[test]
fn responses_text_image_text_result_is_one_ordered_function_call_output() {
    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&text_image_text_request(), false)
        .unwrap();
    let outputs = function_call_outputs(&body);
    assert_eq!(outputs.len(), 1, "{body:#}");
    assert_eq!(
        outputs[0]["output"],
        json!([
            {"type": "input_text", "text": "[\"before\","},
            {"type": "input_image", "image_url": data_url()},
            {"type": "input_text", "text": ",\"after\"]"},
        ])
    );
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["role"] != "user"),
        "the image stays inside the tool's output: {body:#}"
    );
}

#[test]
fn codex_text_image_text_result_is_one_ordered_function_call_output() {
    let body = crate::CodexProvider::new("access", "refresh", 0)
        .build_request_body(&text_image_text_request(), false)
        .unwrap();
    let outputs = function_call_outputs(&body);
    assert_eq!(outputs.len(), 1, "{body:#}");
    assert_eq!(
        outputs[0]["output"],
        json!([
            {"type": "input_text", "text": "[\"before\","},
            {"type": "input_image", "image_url": data_url()},
            {"type": "input_text", "text": ",\"after\"]"},
        ])
    );
}

#[test]
fn responses_text_only_result_keeps_the_string_output() {
    let mut req = text_image_text_request();
    Arc::make_mut(&mut req.messages[1].blocks)[0] = LlmContentBlock::ToolResult {
        call_id: "call_1".into(),
        tool_name: Some("shot".into()),
        content: vec![
            ModelToolReturnPart::text("ok"),
            ModelToolReturnPart::text("[tool intent note]"),
        ],
    };
    let body = OpenAiProvider::new("key")
        .build_responses_request_body(&req, false)
        .unwrap();
    let outputs = function_call_outputs(&body);
    assert_eq!(outputs.len(), 1, "{body:#}");
    assert_eq!(outputs[0]["output"], json!("ok\n[tool intent note]"));
}

#[test]
fn chat_text_image_text_result_is_one_tool_message_with_the_image_after() {
    let body = OpenAiCompatibleProvider::new("key", "https://provider.example/v1")
        .build_chat_request_body(&text_image_text_request(), false)
        .unwrap();
    let messages = body["messages"].as_array().expect("messages");
    let tool_messages: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
        .collect();
    assert_eq!(tool_messages.len(), 1, "{body:#}");
    assert_eq!(
        tool_messages[0]["content"],
        json!("[\"before\",\n[Attachment 1]\n,\"after\"]")
    );
    let tool_index = messages
        .iter()
        .position(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(
        messages[tool_index + 1],
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "Attachments from tool result call_1:"},
                {"type": "image_url", "image_url": {"url": data_url()}},
            ],
        })
    );
}
