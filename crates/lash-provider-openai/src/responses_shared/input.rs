use serde_json::{Value, json};

use super::*;

// ---------------------------------------------------------------------------
// Request input assembly
// ---------------------------------------------------------------------------

/// The handful of genuine deltas between the direct-OpenAI and Codex flavours
/// of the Responses `input` array. Everything else — the per-message block
/// loop, the reasoning-replay item shape, function_call/function_call_output
/// emission, runtime feedback projection — is identical.
#[derive(Clone, Copy, Debug)]
pub struct ResponsesInputOptions {
    /// Responses assistant history is emitted as `message` items with stable
    /// ids (`msg_lash_{message}_{part}` when the request carries none),
    /// status/phase from [`ResponseTextMeta`], and `output_text` annotations.
    pub assistant_message_metadata: bool,
    /// Codex folds sibling user `input_image` parts that follow a
    /// `function_call_output` into that output's `output` array so the image
    /// reads as the tool's result. OpenAI keeps them as a standalone user turn.
    pub fold_tool_result_images: bool,
}

impl ResponsesInputOptions {
    /// Direct OpenAI Responses: synthetic assistant ids + phase/annotations,
    /// no tool-result image folding.
    pub const OPENAI: Self = Self {
        assistant_message_metadata: true,
        fold_tool_result_images: false,
    };

    /// Codex Responses: same assistant message metadata as OpenAI, with
    /// tool-result images folded into the preceding `function_call_output`.
    pub const CODEX: Self = Self {
        assistant_message_metadata: true,
        fold_tool_result_images: true,
    };
}

#[allow(clippy::too_many_arguments)]
fn flush_pending_content(
    pending: &mut Vec<Value>,
    input: &mut Vec<Value>,
    role: &'static str,
    is_user: bool,
    opts: &ResponsesInputOptions,
    response_meta: Option<ResponseTextMeta>,
    message_index: usize,
    part_index: usize,
) {
    if pending.is_empty() {
        return;
    }
    let content = std::mem::take(pending);
    if opts.assistant_message_metadata && role == "assistant" {
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
        item["content"] = Value::Array(content);
        if let Some(phase) = meta.phase.as_ref() {
            item["phase"] = json!(phase.as_str());
        }
        input.push(item);
        return;
    }
    if is_user
        && let Some(prev) = input.last_mut()
        && prev.get("role").and_then(|v| v.as_str()) == Some("user")
        && let Some(existing) = prev.get_mut("content").and_then(Value::as_array_mut)
    {
        existing.extend(content);
    } else {
        let mut item = json!({"role": role});
        item["content"] = Value::Array(content);
        input.push(item);
    }
}

/// Walk backwards from the last input item: if the final entry is a user
/// `content` message whose parts are all `input_image`, and the entry before
/// it is a `function_call_output`, promote the image parts into the `output`
/// of that function_call_output so the server sees the image as the tool's
/// result rather than as a standalone user turn.
fn fold_tool_result_images(input: &mut Vec<Value>) {
    if input.len() < 2 {
        return;
    }
    let last_idx = input.len() - 1;
    let is_user_image_msg = input[last_idx].get("role").and_then(|v| v.as_str()) == Some("user")
        && input[last_idx]
            .get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|parts| {
                parts
                    .iter()
                    .all(|p| p.get("type").and_then(|t| t.as_str()) == Some("input_image"))
            });
    if !is_user_image_msg {
        return;
    }
    let prev_is_call_output =
        input[last_idx - 1].get("type").and_then(|v| v.as_str()) == Some("function_call_output");
    if !prev_is_call_output {
        return;
    }
    let last = input.remove(last_idx);
    let image_parts = last
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(prev) = input.last_mut() else { return };
    // An array keeps its parts, a non-empty string becomes the text part it stood for.
    let mut output = match prev["output"].take() {
        Value::Array(parts) => parts,
        Value::String(t) if !t.is_empty() => vec![json!({"type": "input_text", "text": t})],
        _ => Vec::new(),
    };
    output.extend(image_parts);
    prev["output"] = Value::Array(output);
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
    items: &mut Vec<Value>,
    item: Value,
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

/// Build ordered Responses input shared by the direct provider and Codex.
pub fn build_responses_input(req: &LlmRequest, opts: ResponsesInputOptions) -> Vec<Value> {
    let mut input: Vec<Value> = Vec::new();
    let mut feedback_start = None;
    for (message_index, msg) in req.messages.iter().enumerate() {
        feedback_boundary(msg, input.len(), &mut feedback_start);
        let fallback = attachment_feedback(msg);
        let msg = fallback.as_ref().unwrap_or(msg);
        let role = if matches!(msg.role, LlmRole::System) {
            req.model_capability.instruction_role.as_str()
        } else {
            role_name(&msg.role)
        };
        let is_user = matches!(msg.role, LlmRole::User);
        let mut pending_content: Vec<Value> = Vec::new();
        let mut pending_meta: Option<ResponseTextMeta> = None;
        let mut pending_part_index = 0usize;

        // Codex folds the image/placeholder blocks that follow a ToolResult
        // into that tool's `output`. One scan yields both the folded image
        // parts (keyed by ToolResult block index) and the sibling indices to
        // skip in the main loop so they aren't double-emitted.
        let (tool_result_image_folds, consumed_after_tool_result) = if opts.fold_tool_result_images
        {
            collect_tool_result_image_folds(req, msg)
        } else {
            Default::default()
        };

        for (part_index, block) in msg.blocks.iter().enumerate() {
            if consumed_after_tool_result.contains(&part_index) {
                continue;
            }
            match block {
                LlmContentBlock::Text {
                    text,
                    response_meta,
                    ..
                } => {
                    if text.is_empty() {
                        continue;
                    }
                    if opts.assistant_message_metadata
                        && matches!(msg.role, LlmRole::Assistant)
                        && (!pending_content.is_empty() || response_meta.is_some())
                    {
                        flush_pending_content(
                            &mut pending_content,
                            &mut input,
                            role,
                            false,
                            &opts,
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
                    if opts.assistant_message_metadata && part_type == "output_text" {
                        pending_content.push(json!({
                            "type": part_type,
                            "text": text,
                            "annotations": [],
                        }));
                    } else {
                        pending_content.push(json!({
                            "type": part_type,
                            "text": text,
                        }));
                    }
                }
                LlmContentBlock::Attachment { source } => {
                    if is_user {
                        pending_content.push(input_attachment_part(req, source));
                    }
                }
                LlmContentBlock::Reasoning { text, replay, .. } => {
                    flush_pending_content(
                        &mut pending_content,
                        &mut input,
                        role,
                        is_user && fallback.is_none(),
                        &opts,
                        pending_meta.take(),
                        message_index,
                        pending_part_index,
                    );
                    if let Some(item) = reasoning_replay_item(text, replay.as_ref()) {
                        input.push(item);
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
                        &opts,
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
                    input.push(item);
                }
                LlmContentBlock::ToolResult {
                    call_id, content, ..
                } => {
                    flush_pending_content(
                        &mut pending_content,
                        &mut input,
                        role,
                        is_user && fallback.is_none(),
                        &opts,
                        pending_meta.take(),
                        message_index,
                        pending_part_index,
                    );
                    let image_parts = tool_result_image_folds
                        .get(&part_index)
                        .cloned()
                        .unwrap_or_default();
                    if image_parts.is_empty() {
                        push_tool_output(
                            &mut input,
                            json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": content,
                            }),
                            &mut feedback_start,
                        );
                    } else {
                        let mut parts: Vec<Value> = Vec::new();
                        if !content.is_empty() {
                            parts.push(json!({
                                "type": "input_text",
                                "text": content,
                            }));
                        }
                        parts.extend(image_parts);
                        push_tool_output(
                            &mut input,
                            json!({
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": parts,
                            }),
                            &mut feedback_start,
                        );
                    }
                }
            }
        }
        flush_pending_content(
            &mut pending_content,
            &mut input,
            role,
            is_user && fallback.is_none(),
            &opts,
            pending_meta.take(),
            message_index,
            pending_part_index,
        );

        if opts.fold_tool_result_images && is_user && fallback.is_none() {
            fold_tool_result_images(&mut input);
        }
    }

    input
}

/// For each `ToolResult` block in `msg`, the Codex-folded image parts (its
/// trailing sibling `Image` / `[Tool image: …]` blocks) keyed by the
/// ToolResult's block index, plus the set of all sibling indices consumed this
/// way so the main loop skips them. One scan replaces the former skip-set
/// pre-pass plus a separate per-ToolResult re-scan.
fn collect_tool_result_image_folds(
    req: &LlmRequest,
    msg: &lash_core::llm::types::LlmMessage,
) -> (
    std::collections::HashMap<usize, Vec<Value>>,
    std::collections::HashSet<usize>,
) {
    let mut folds: std::collections::HashMap<usize, Vec<Value>> = std::collections::HashMap::new();
    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, block) in msg.blocks.iter().enumerate() {
        if !matches!(block, LlmContentBlock::ToolResult { .. }) {
            continue;
        }
        let mut parts: Vec<Value> = Vec::new();
        for (j, sibling) in msg.blocks.iter().enumerate().skip(idx + 1) {
            match sibling {
                LlmContentBlock::Attachment { source } => {
                    parts.push(input_attachment_part(req, source));
                    consumed.insert(j);
                }
                LlmContentBlock::Text { text: t, .. } if t.starts_with("[Tool image:") => {
                    consumed.insert(j);
                }
                _ => break,
            }
        }
        if !parts.is_empty() {
            folds.insert(idx, parts);
        }
    }
    (folds, consumed)
}
