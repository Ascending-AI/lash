use lash_sansio::llm::types::TemplateJson;
use serde_json::{Value, json};

use super::*;

// ---------------------------------------------------------------------------
// Request input assembly
// ---------------------------------------------------------------------------

fn flush_pending_content(
    pending: &mut Vec<TemplateJson>,
    input: &mut Vec<TemplateJson>,
    role: &'static str,
    is_user: bool,
    response_meta: Option<ResponseTextMeta>,
    message_index: usize,
    part_index: usize,
) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    if role == "assistant" {
        let meta = response_meta.unwrap_or(ResponseTextMeta {
            id: Some(format!("msg_lash_{message_index}_{part_index}")),
            status: Some("completed".to_string()),
            phase: None,
            ..ResponseTextMeta::default()
        });
        let mut item = json!({
            "type": "message",
            "role": "assistant",
            "id": meta.id.unwrap_or_else(|| format!("msg_lash_{message_index}_{part_index}")),
            "status": meta.status.unwrap_or_else(|| "completed".to_string()),
        });
        if let Some(phase) = meta.phase.as_ref() {
            item["phase"] = json!(phase.as_str());
        }
        let mut item = TemplateJson::from(item);
        item.set("content", content);
        input.push(item);
        return;
    }
    if is_user
        && let Some(prev) = input.last_mut()
        && prev.str_field("role") == Some("user")
        && let Some(existing) = prev
            .field_mut("content")
            .and_then(TemplateJson::as_array_mut)
    {
        existing.extend(content);
    } else {
        input.push(TemplateJson::object([
            ("role", json!(role).into()),
            ("content", content.into()),
        ]));
    }
}

fn reasoning_replay_item(text: &str, replay: Option<&ProviderReasoningReplay>) -> Option<Value> {
    // Only replay reasoning items that actually carry an encrypted blob.
    // Display-only summaries (no blob) must not be fed back — the server will
    // either ignore them or reject the turn.
    let blob = replay.and_then(|meta| meta.encrypted_content.as_deref())?;
    let summary = replay
        .map(|meta| meta.summary.as_slice())
        .unwrap_or_default();
    let summary_items: Vec<Value> = if summary.is_empty() {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type": "summary_text", "text": text})]
        }
    } else {
        summary
            .iter()
            .map(|entry| json!({"type": "summary_text", "text": entry}))
            .collect()
    };
    let mut item = json!({
        "type": "reasoning",
        "summary": summary_items,
        "encrypted_content": blob,
    });
    if let Some(id) = replay.and_then(|meta| meta.item_id.as_deref())
        && !id.is_empty()
    {
        item["id"] = json!(id);
    }
    Some(item)
}

// Only attachment-bearing feedback loses native instruction authority. Keep its
// complete text in one tag and carry every attachment in the same user item.
pub(crate) fn attachment_feedback(msg: &LlmMessage) -> Option<LlmMessage> {
    if !matches!(msg.role, LlmRole::System)
        || !msg
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::Attachment { .. }))
    {
        return None;
    }
    let mut blocks = vec![LlmContentBlock::Text {
        text: format!(
            "<runtime_feedback>{}</runtime_feedback>",
            msg.blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<String>()
        )
        .into(),
        response_meta: None,
        cache_breakpoint: msg.blocks.iter().any(|block| {
            matches!(
                block,
                LlmContentBlock::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        }),
    }];
    blocks.extend(
        msg.blocks
            .iter()
            .filter(|block| !matches!(block, LlmContentBlock::Text { .. }))
            .cloned(),
    );
    Some(LlmMessage::new(LlmRole::User, blocks))
}

// Tool outputs are separate wire items, but belong before the feedback in
// their user turn. Preserve the order of both outputs and feedback items.
pub(crate) fn push_tool_output(
    items: &mut Vec<TemplateJson>,
    item: TemplateJson,
    feedback_start: &mut Option<usize>,
) {
    if let Some(index) = feedback_start {
        items.insert(*index, item);
        *index += 1;
    } else {
        items.push(item);
    }
}

pub(crate) fn feedback_boundary(msg: &LlmMessage, item_count: usize, start: &mut Option<usize>) {
    match msg.role {
        LlmRole::System => {
            start.get_or_insert(item_count);
        }
        LlmRole::User
            if msg
                .blocks
                .iter()
                .any(|block| matches!(block, LlmContentBlock::ToolResult { .. })) => {}
        _ => *start = None,
    }
}

/// A tool result's `function_call_output.output`: the plain string when the
/// result is text only, otherwise the content-item array with its text and
/// attachments interleaved in the result's order, so an image stays the
/// tool's output rather than a separate user turn.
fn function_call_output(content: &[ModelToolReturnPart]) -> TemplateJson {
    if content.iter().all(|block| block.attachment().is_none()) {
        return Value::String(tool_result_text(content).into_owned()).into();
    }
    TemplateJson::Array(
        content
            .iter()
            .filter_map(|block| match block {
                ModelToolReturnPart::Text { text } if text.is_empty() => None,
                ModelToolReturnPart::Text { text } => {
                    Some(json!({"type": "input_text", "text": text}).into())
                }
                // Retained output is sent as its witness; its reference is
                // never materialized.
                ModelToolReturnPart::Retained(retained) => {
                    Some(json!({"type": "input_text", "text": retained.witness}).into())
                }
                ModelToolReturnPart::Attachment(source) => Some(input_attachment_part(
                    source,
                    lash_sansio::llm::attachment_delivery::AttachmentPosition::ToolResult,
                )),
            })
            .collect(),
    )
}

/// Build ordered Responses input shared by the direct provider and Codex.
pub fn build_responses_input(req: &LlmRequest) -> Vec<TemplateJson> {
    let mut input: Vec<TemplateJson> = Vec::new();
    let mut feedback_start = None;
    for (message_index, msg) in req.messages.iter().enumerate() {
        feedback_boundary(msg, input.len(), &mut feedback_start);
        let fallback = attachment_feedback(msg);
        let msg = fallback.as_ref().unwrap_or(msg);
        let role = if matches!(msg.role, LlmRole::System) {
            req.model.metadata().capability.instruction_role.as_str()
        } else {
            role_name(&msg.role)
        };
        let is_user = matches!(msg.role, LlmRole::User);
        let mut pending_content: Vec<TemplateJson> = Vec::new();
        let mut pending_meta: Option<ResponseTextMeta> = None;
        let mut pending_part_index = 0usize;

        for (part_index, block) in msg.blocks.iter().enumerate() {
            match block {
                LlmContentBlock::Text {
                    text,
                    response_meta,
                    ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    if matches!(msg.role, LlmRole::Assistant)
                        && (!pending_content.is_empty() || response_meta.is_some())
                    {
                        flush_pending_content(
                            &mut pending_content,
                            &mut input,
                            role,
                            false,
                            pending_meta.take(),
                            message_index,
                            pending_part_index,
                        );
                        pending_part_index = part_index;
                        pending_meta = response_meta.clone();
                    }
                    let part_type = if matches!(msg.role, LlmRole::Assistant) {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    if part_type == "output_text" {
                        pending_content.push(
                            json!({
                                "type": part_type,
                                "text": text,
                                "annotations": [],
                            })
                            .into(),
                        );
                    } else {
                        pending_content.push(
                            json!({
                                "type": part_type,
                                "text": text,
                            })
                            .into(),
                        );
                    }
                }
                LlmContentBlock::Attachment { reference } => {
                    if is_user {
                        pending_content.push(input_attachment_part(
                            reference,
                            lash_sansio::llm::attachment_delivery::AttachmentPosition::Message,
                        ));
                    }
                }
                LlmContentBlock::Reasoning { text, replay, .. } => {
                    flush_pending_content(
                        &mut pending_content,
                        &mut input,
                        role,
                        is_user && fallback.is_none(),
                        pending_meta.take(),
                        message_index,
                        pending_part_index,
                    );
                    if let Some(item) = reasoning_replay_item(text, replay.as_ref()) {
                        input.push(item.into());
                    }
                }
                LlmContentBlock::ToolCall {
                    call_id,
                    tool_name,
                    input_json,
                    replay,
                    ..
                } => {
                    flush_pending_content(
                        &mut pending_content,
                        &mut input,
                        role,
                        is_user && fallback.is_none(),
                        pending_meta.take(),
                        message_index,
                        pending_part_index,
                    );
                    let mut item = json!({
                        "type": "function_call",
                        "call_id": call_id,
                        "name": tool_name,
                        "arguments": tool_call_input_replay_string(input_json),
                    });
                    // `id` (e.g. `fc_...`) pairs a function_call with its
                    // sibling reasoning item across turns; omit when absent.
                    if let Some(id) = replay.as_ref().and_then(|meta| meta.item_id.as_deref()) {
                        item["id"] = json!(id);
                    }
                    input.push(item.into());
                }
                LlmContentBlock::ToolResult {
                    call_id, content, ..
                } => {
                    flush_pending_content(
                        &mut pending_content,
                        &mut input,
                        role,
                        is_user && fallback.is_none(),
                        pending_meta.take(),
                        message_index,
                        pending_part_index,
                    );
                    push_tool_output(
                        &mut input,
                        TemplateJson::object([
                            ("type", json!("function_call_output").into()),
                            ("call_id", json!(call_id).into()),
                            ("output", function_call_output(content)),
                        ]),
                        &mut feedback_start,
                    );
                }
            }
        }
        flush_pending_content(
            &mut pending_content,
            &mut input,
            role,
            is_user && fallback.is_none(),
            pending_meta.take(),
            message_index,
            pending_part_index,
        );
    }

    input
}
