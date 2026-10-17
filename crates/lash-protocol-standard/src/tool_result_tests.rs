//! FIG-3515: a tool call is answered by exactly one transcript part whose
//! blocks keep the tool value's order, and the committed transcript is one
//! the resume-safety check accepts.
use super::*;
use lash_core::{
    AttachmentId, AttachmentRef, AttachmentTypeMetadata, MediaType, ToolCallOutput, ToolValue,
    facade_support::ModelToolReturn, facade_support::ModelToolReturnPart,
};

fn attachment_source(id: &str) -> AttachmentRef {
    AttachmentRef::new(
        AttachmentId::parse(format!("{:02x}", id.bytes().fold(0u8, u8::wrapping_add)).repeat(32))
            .expect("valid attachment id"),
        MediaType::parse("image/png").unwrap(),
        4,
        Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("tiny".to_string()),
    )
}

/// FIG-3515: the transcript this protocol commits for any tool value is
/// one the resume-safety check accepts, and the prompt rendered from it
/// answers each call with exactly one tool-result block carrying the
/// model return's blocks unchanged.
#[test]
fn committed_tool_results_agree_with_the_resume_safety_check() {
    let attachment = attachment_source("att-3");
    let mut object = std::collections::BTreeMap::new();
    object.insert("caption".to_string(), ToolValue::String("shot".into()));
    object.insert(
        "image".to_string(),
        ToolValue::Attachment(attachment.clone()),
    );
    let returns = [
        ModelToolReturn::from_output(
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::Array(vec![
                ToolValue::String("before".into()),
                ToolValue::Attachment(attachment.clone()),
                ToolValue::String("after".into()),
            ])),
        ),
        ModelToolReturn::from_output(
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::Object(object)),
        ),
        ModelToolReturn::from_output(
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::String(String::new())),
        ),
        ModelToolReturn::from_output("shot".to_string(), &ToolCallOutput::success("ok")),
        ModelToolReturn::from_output(
            "shot".into(),
            &ToolCallOutput::failure(lash_core::ToolFailure::new("disk", "boom")),
        ),
        ModelToolReturn::from_output(
            "shot".into(),
            &ToolCallOutput::cancelled(lash_core::ToolCancellation::runtime("stopped")),
        ),
    ];

    let labels = [
        "call-array",
        "call-object",
        "call-empty",
        "call-text",
        "call-failed",
        "call-cancelled",
    ];
    let calls = returns
        .iter()
        .zip(labels)
        .map(|(model_return, label)| {
            Part::tool_call(
                String::new(),
                "{}".to_string(),
                lash_core::ToolCallId::fixture(label),
                label.to_string(),
                model_return.tool_name.clone(),
                None,
            )
        })
        .collect();
    let results = returns
        .iter()
        .cloned()
        .zip(labels)
        .map(|(model_return, label)| {
            tool_result_part(lash_core::ToolCallId::fixture(label), model_return)
        })
        .collect();
    let transcript = vec![
        Message {
            id: "m_calls".to_string(),
            role: MessageRole::Assistant,
            parts: shared_parts(calls),
            origin: None,
            reply_marker: None,
        },
        Message {
            id: "m_results".to_string(),
            role: MessageRole::User,
            parts: shared_parts(results),
            origin: None,
            reply_marker: None,
        },
    ];

    let transcript: Vec<Message> =
        serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
    assert!(lash_protocol_types::messages_are_prompt_resume_safe(
        &transcript
    ));
    let rendered = lash_protocol_types::session_model::message::render_prompt(&transcript);
    let result_blocks: Vec<_> = rendered.messages[1]
        .blocks
        .iter()
        .filter_map(|block| match block {
            lash_protocol_types::llm::types::LlmContentBlock::ToolResult {
                call_id,
                content,
                status,
                ..
            } => Some((call_id.clone(), content.clone(), *status)),
            _ => None,
        })
        .collect();
    assert_eq!(result_blocks.len(), returns.len());
    let expected_blocks: Vec<(&str, Vec<ModelToolReturnPart>)> = vec![
        (
            "call-array",
            vec![
                ModelToolReturnPart::text("[\"before\","),
                ModelToolReturnPart::Attachment(attachment.clone()),
                ModelToolReturnPart::text(",\"after\"]"),
            ],
        ),
        (
            "call-object",
            vec![
                ModelToolReturnPart::text("{\"caption\":\"shot\",\"image\":"),
                ModelToolReturnPart::Attachment(attachment.clone()),
                ModelToolReturnPart::text("}"),
            ],
        ),
        ("call-empty", vec![]),
        ("call-text", vec![ModelToolReturnPart::text("\"ok\"")]),
        ("call-failed", vec![]),
        ("call-cancelled", vec![]),
    ];
    for (((call_id, content, status), (expected_call_id, expected_content)), model_return) in
        result_blocks.iter().zip(&expected_blocks).zip(&returns)
    {
        assert_eq!(*status, model_return.outcome.status());
        assert_eq!(call_id, expected_call_id);
        assert_eq!(
            content, expected_content,
            "{call_id} keeps its blocks in order"
        );
    }
    assert!(
        rendered.messages[1].blocks.iter().all(|block| matches!(
            block,
            lash_protocol_types::llm::types::LlmContentBlock::ToolResult { .. }
        )),
        "no attachment escapes its result as a loose block"
    );
}
