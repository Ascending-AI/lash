//! FIG-3515: a tool result mixing text and an attachment reaches the
//! Anthropic request as exactly one `tool_result` for its `tool_use_id`,
//! with its text and image blocks in the tool value's order.
use super::*;
use lash_sansio::session_model::message::render_prompt;
use lash_sansio::{MediaType, Message, MessageRole, ModelToolReturnPart, Part, shared_parts};

fn png() -> AttachmentSource {
    AttachmentSource::inline(MediaType::parse("image/png").unwrap(), vec![1, 2, 3, 4])
}

fn transcript(content: Vec<ModelToolReturnPart>) -> Vec<Message> {
    vec![
        Message {
            id: "m1".into(),
            role: MessageRole::Assistant,
            parts: shared_parts(vec![Part::tool_call(
                "m1.p0".into(),
                "{}".into(),
                "call_1".into(),
                "shot".into(),
                None,
            )]),
            origin: None,
        },
        Message {
            id: "m2".into(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::tool_result(
                "m2.p0".into(),
                content,
                "call_1".into(),
                "shot".into(),
            )]),
            origin: None,
        },
    ]
}

fn user_content(content: Vec<ModelToolReturnPart>) -> Value {
    let rendered = render_prompt(&transcript(content));
    let body = AnthropicProvider::new("key")
        .build_request_body(&request(rendered.messages))
        .unwrap();
    body["messages"][1]["content"].clone()
}

#[test]
fn text_image_text_tool_result_is_one_anthropic_tool_result() {
    let content = user_content(vec![
        ModelToolReturnPart::text("[\"before\","),
        ModelToolReturnPart::Attachment(png()),
        ModelToolReturnPart::text(",\"after\"]"),
    ]);
    assert_eq!(
        content,
        json!([{
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": [
                {"type": "text", "text": "[\"before\","},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQIDBA=="}},
                {"type": "text", "text": ",\"after\"]"},
            ],
            "cache_control": {"type": "ephemeral"},
        }])
    );
}

#[test]
fn text_only_tool_result_keeps_the_plain_string_form() {
    let content = user_content(vec![ModelToolReturnPart::text("ok")]);
    assert_eq!(
        content,
        json!([{
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "ok",
            "cache_control": {"type": "ephemeral"},
        }])
    );
}
