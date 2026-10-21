use lash_core::provider::attachment_wire::UnitWriter;
use lash_protocol_types::llm::types::TemplateJson;
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
fn function_call_output(content: &[ModelToolReturnPart], is_error: bool) -> TemplateJson {
    if content.iter().all(|block| block.attachment().is_none()) {
        let text = tool_result_text(content);
        return Value::String(if is_error {
            format!("Error: {text}")
        } else {
            text.into_owned()
        })
        .into();
    }
    let marker = is_error.then(|| json!({"type": "input_text", "text": "Error: "}).into());
    TemplateJson::Array(
        marker
            .into_iter()
            .chain(content.iter().filter_map(|block| match block {
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
                    lash_protocol_types::llm::attachment_delivery::AttachmentPosition::ToolResult,
                )),
            }))
            .collect(),
    )
}

/// Whether an assistant message writes an input item: a message item for
/// its text, or a call, an output or a replayed reasoning item.
fn assistant_writes_item(msg: &LlmMessage) -> bool {
    matches!(msg.role, LlmRole::Assistant)
        && msg.blocks.iter().any(|block| match block {
            LlmContentBlock::Text { text, .. } => !text.is_empty(),
            LlmContentBlock::ToolCall { .. } | LlmContentBlock::ToolResult { .. } => true,
            LlmContentBlock::Reasoning { replay, .. } => replay
                .as_ref()
                .is_some_and(|meta| meta.encrypted_content.is_some()),
            LlmContentBlock::Attachment { .. } => false,
        })
}

/// Build ordered Responses input shared by the direct provider and Codex,
/// each group of messages written by `units`.
///
/// A group starts at an assistant message that writes an item. Its last
/// item is the assistant's, so no later user content merges into an
/// earlier group's item, and it ends the feedback a tool output is placed
/// before: a group's items are the same alone as within the whole input,
/// and a group lowered before is reused whole.
///
/// # Errors
///
/// What pinning an attachment slot refuses with.
pub fn build_responses_input(
    req: &LlmRequest,
    units: &mut UnitWriter<'_>,
) -> Result<Vec<TemplateJson>, LlmTransportError> {
    let mut input = Vec::new();
    let source_offset = units.source_offset();
    let mut start = 0;
    for end in 1..=req.messages.len() {
        if end < req.messages.len() && !assistant_writes_item(&req.messages[end]) {
            continue;
        }
        let group = start..end;
        input.extend(units.unit_at(
            &req.messages,
            group.clone(),
            |key| {
                req.messages[group.clone()]
                    .iter()
                    .fold(key.u64((group.start + source_offset) as u64), |key, msg| {
                        key.message(msg)
                    })
            },
            || Ok(build_input_group(req, group.clone(), source_offset)),
        )?);
        start = end;
    }
    Ok(input)
}

/// The input items of the messages in `group`.
fn build_input_group(
    req: &LlmRequest,
    group: std::ops::Range<usize>,
    source_offset: usize,
) -> Vec<TemplateJson> {
    let mut input: Vec<TemplateJson> = Vec::new();
    let mut feedback_start = None;
    for (message_index, msg) in req
        .messages
        .iter()
        .enumerate()
        .skip(group.start)
        .take(group.len())
    {
        let message_index = message_index + source_offset;
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
                            lash_protocol_types::llm::attachment_delivery::AttachmentPosition::Message,
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
                    call_id,
                    content,
                    status,
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
                    push_tool_output(
                        &mut input,
                        TemplateJson::object([
                            ("type", json!("function_call_output").into()),
                            ("call_id", json!(call_id).into()),
                            ("output", function_call_output(content, status.is_error())),
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

pub(crate) fn retained_group_start(
    messages: &lash_protocol_types::llm::types::RequestMessages,
) -> usize {
    let mut starts = 0;
    for index in (0..messages.len()).rev() {
        if messages.get(index).is_some_and(assistant_writes_item) {
            starts += 1;
            if starts == 2 {
                return index;
            }
        }
    }
    0
}
