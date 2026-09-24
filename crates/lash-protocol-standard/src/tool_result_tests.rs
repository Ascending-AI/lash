//! FIG-3515: a tool call is answered by exactly one transcript part whose
//! blocks keep the tool value's order, and the committed transcript is one
//! the resume-safety check accepts.
use super::*;
use lash_core::{
    AttachmentId, AttachmentSource, AttachmentTypeMetadata, MediaType, ToolCallOutput, ToolValue,
    facade_support::AttachmentRef, facade_support::ModelToolReturn,
    facade_support::ModelToolReturnPart,
};

fn attachment_source(id: &str) -> AttachmentSource {
    AttachmentSource::stored(AttachmentRef::new(
        AttachmentId::parse(id).expect("valid attachment id"),
        MediaType::parse("image/png").unwrap(),
        4,
        Some(AttachmentTypeMetadata::image(Some(1), Some(1))),
        Some("tiny".to_string()),
    ))
}

#[test]
fn tool_attachment_is_a_block_of_the_one_result() {
    let attachment = attachment_source("att-1");
    let output = ToolCallOutput::success_tool_value(ToolValue::Attachment(attachment.clone()));
    let model_return =
        ModelToolReturn::from_output("call-9".to_string(), "screenshot".to_string(), &output);

    let part = tool_result_part(model_return);

    assert!(matches!(part.kind(), PartKind::ToolResult));
    assert_eq!(part.tool_call_id(), Some("call-9"));
    assert_eq!(part.tool_name(), Some("screenshot"));
    assert_eq!(
        part.tool_result_content(),
        Some(&[ModelToolReturnPart::Attachment(attachment)][..])
    );
}

#[test]
fn tool_text_and_attachment_keep_their_order_inside_one_result() {
    let attachment = attachment_source("att-2");
    let output = ToolCallOutput::success_tool_value(ToolValue::Array(vec![
        ToolValue::String("before".into()),
        ToolValue::Attachment(attachment.clone()),
        ToolValue::String("after".into()),
    ]));
    let model_return =
        ModelToolReturn::from_output("call-10".to_string(), "snap".to_string(), &output);

    let part = tool_result_part(model_return);

    // The array projection's compact JSON text sits around the
    // attachment, in position, inside the call's single result.
    assert_eq!(
        part.tool_result_content(),
        Some(
            &[
                ModelToolReturnPart::text("[\"before\","),
                ModelToolReturnPart::Attachment(attachment),
                ModelToolReturnPart::text(",\"after\"]"),
            ][..]
        )
    );
    assert_eq!(part.content(), "[\"before\",\n[Attachment 1]\n,\"after\"]");
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
    let notice = lash_sansio::AttachmentMaterializationNotice::no_provider_accepts(&attachment);
    let notice_placeholder = notice.model_placeholder();
    let mut noticed = ModelToolReturn::from_output(
        "call-notice".to_string(),
        "shot".to_string(),
        &ToolCallOutput::success_tool_value(ToolValue::Attachment(attachment.clone())),
    );
    noticed
        .parts
        .push(ModelToolReturnPart::text(notice_placeholder.clone()));
    noticed.attachment_notices.push(notice);

    let returns = vec![
        ModelToolReturn::from_output(
            "call-array".to_string(),
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::Array(vec![
                ToolValue::String("before".into()),
                ToolValue::Attachment(attachment.clone()),
                ToolValue::String("after".into()),
            ])),
        ),
        ModelToolReturn::from_output(
            "call-object".to_string(),
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::Object(object)),
        ),
        ModelToolReturn::from_output(
            "call-empty".to_string(),
            "shot".to_string(),
            &ToolCallOutput::success_tool_value(ToolValue::String(String::new())),
        ),
        ModelToolReturn::from_output(
            "call-text".to_string(),
            "shot".to_string(),
            &ToolCallOutput::success("ok"),
        ),
        noticed,
    ];

    let calls = returns
        .iter()
        .map(|model_return| {
            Part::tool_call(
                String::new(),
                "{}".to_string(),
                model_return.call_id.clone(),
                model_return.tool_name.clone(),
                None,
            )
        })
        .collect();
    let results = returns.iter().cloned().map(tool_result_part).collect();
    let transcript = vec![
        Message {
            id: "m_calls".to_string(),
            role: MessageRole::Assistant,
            parts: shared_parts(calls),
            origin: None,
        },
        Message {
            id: "m_results".to_string(),
            role: MessageRole::User,
            parts: shared_parts(results),
            origin: None,
        },
    ];

    assert!(lash_sansio::messages_are_prompt_resume_safe(&transcript));
    let rendered = lash_sansio::session_model::message::render_prompt(&transcript);
    let result_blocks: Vec<_> = rendered.messages[1]
        .blocks
        .iter()
        .filter_map(|block| match block {
            lash_sansio::llm::types::LlmContentBlock::ToolResult {
                call_id, content, ..
            } => Some((call_id.clone(), content.clone())),
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
        (
            "call-notice",
            vec![
                ModelToolReturnPart::Attachment(attachment.clone()),
                ModelToolReturnPart::text(notice_placeholder),
            ],
        ),
    ];
    for ((call_id, content), (expected_call_id, expected_content)) in
        result_blocks.iter().zip(&expected_blocks)
    {
        assert_eq!(call_id, expected_call_id);
        assert_eq!(
            content, expected_content,
            "{call_id} keeps its blocks in order"
        );
    }
    assert!(
        rendered.messages[1].blocks.iter().all(|block| matches!(
            block,
            lash_sansio::llm::types::LlmContentBlock::ToolResult { .. }
        )),
        "no attachment escapes its result as a loose block"
    );
}
