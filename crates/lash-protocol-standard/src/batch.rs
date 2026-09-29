//! `batch` is protocol sugar (ADR 0116 §2): never a tool, never executed.
//!
//! The driver expands each `batch` call of a response into slots of the
//! step's one tool group, beside the response's native calls, and folds the
//! slots' results back into one result per wrapper. Both directions are pure
//! functions of the recorded response and the turn's admitted configuration,
//! so a replay recomputes the identical plan and presentation.

use std::num::NonZeroUsize;

use lash_core::ToolDefinition;
use lash_core::facade_support::{ModelToolReturn, ModelToolReturnPart};
use lash_core::sansio::{
    CompletedToolCall, ExpandedRow, ExpandedWrapper, PendingToolCall, ToolExpansionPlan,
};
use lash_core::{ToolCallOutput, ToolFailure, ToolFailureClass};
use lash_tool_support::object_schema;
use serde_json::Value;

pub type BatchResultRow = lash_sansio::BatchResultRow;

/// The name the model calls the sugar by.
pub(crate) const BATCH_TOOL_NAME: &str = "batch";

pub(crate) fn batch_tool_definition(max_members: NonZeroUsize) -> ToolDefinition {
    ToolDefinition::raw(
        "tool:batch",
        BATCH_TOOL_NAME,
        format!(
            "Run 1-{max_members} independent tool calls concurrently. Every call starts before any finishes; results return in input order, each with a success flag and result or error. Do not nest batch calls."
        ),
        object_schema(
            serde_json::json!({
                "tool_calls": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": max_members.get(),
                    "items": {
                        "type": "object",
                        "properties": {
                            "tool": { "type": "string" },
                            "parameters": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["tool", "parameters"],
                        "additionalProperties": false
                    },
                    "description": format!("1-{max_members} objects {{ tool, parameters }}; each tool must be available and parameters must match its schema.")
                }
            }),
            &["tool_calls"],
        ),
        batch_output_schema(),
    )
}

fn batch_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "index": { "type": "integer", "minimum": 0 },
                        "tool": { "type": "string" },
                        "success": { "type": "boolean" },
                        "result": {},
                        "error": {}
                    },
                    "required": ["index", "tool", "success"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["results"],
        "additionalProperties": false
    })
}

/// A response's dispatchable calls after expansion.
#[derive(Debug, Default)]
pub(crate) struct Expansion {
    /// The flat executable slots of the step's one tool group.
    pub(crate) calls: Vec<PendingToolCall>,
    /// How the slots fold back into the response's calls.
    pub(crate) plan: ToolExpansionPlan,
    /// Wrappers refused whole: a structurally malformed, empty or oversized
    /// member list. None of their members starts.
    pub(crate) refused: Vec<(PendingToolCall, ToolCallOutput)>,
}

/// Expands every `batch` call of `calls` into consecutive slots at the
/// wrapper's position. Native calls take one slot each, in order.
pub(crate) fn expand(calls: Vec<PendingToolCall>, max_members: NonZeroUsize) -> Expansion {
    let mut expansion = Expansion::default();
    let mut source_position = 0_u32;
    for call in calls {
        if call.tool_name != BATCH_TOOL_NAME {
            expansion.calls.push(call);
            source_position += 1;
            continue;
        }
        let members = match parse_members(&call.args, max_members) {
            Ok(members) => members,
            Err(message) => {
                let output = ToolCallOutput::failure(ToolFailure::runtime(
                    ToolFailureClass::InvalidRequest,
                    "invalid_batch",
                    message,
                ));
                expansion.refused.push((call, output));
                continue;
            }
        };
        let mut rows = Vec::with_capacity(members.len());
        for (member_index, member) in members.into_iter().enumerate() {
            let member_index = index_u32(member_index);
            if member.tool == BATCH_TOOL_NAME {
                rows.push(ExpandedRow::Refused {
                    member_index,
                    tool: member.tool,
                    error: Value::String("`batch` cannot run inside `batch`".to_string()),
                });
                continue;
            }
            rows.push(ExpandedRow::Slot {
                member_index,
                tool: member.tool.clone(),
                slot: index_u32(expansion.calls.len()),
            });
            // A member is named under its wrapper by its original member
            // index, counted before refusals (ADR 0117 §2).
            expansion.calls.push(PendingToolCall {
                call_id: call.call_id.child(u64::from(member_index)),
                provider_call_id: None,
                tool_name: member.tool,
                args: member.parameters,
                replay: None,
            });
        }
        expansion.plan.wrappers.push(ExpandedWrapper {
            source_position,
            call_id: call.call_id,
            provider_call_id: call.provider_call_id,
            tool_name: call.tool_name,
            args: call.args,
            replay: call.replay,
            rows,
        });
        source_position += 1;
    }
    expansion
}

/// Slot and member counts are bounded by the group's retained-children
/// bound, far below `u32::MAX`.
fn index_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

struct Member {
    tool: String,
    parameters: Value,
}

fn parse_members(args: &Value, max_members: NonZeroUsize) -> Result<Vec<Member>, String> {
    let Some(raw_calls) = args.get("tool_calls").and_then(Value::as_array) else {
        return Err("`batch` was not run: `tool_calls` must be an array of calls.".to_string());
    };
    if raw_calls.is_empty() {
        return Err("`batch` was not run: `tool_calls` must hold at least one call.".to_string());
    }
    if raw_calls.len() > max_members.get() {
        return Err(format!(
            "`batch` was not run: it holds {} calls and accepts at most {max_members}.",
            raw_calls.len()
        ));
    }
    raw_calls
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let Some(object) = item.as_object() else {
                return Err(format!(
                    "`batch` was not run: `tool_calls[{index}]` must be an object with `tool` and `parameters`."
                ));
            };
            let Some(tool) = object
                .get("tool")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
            else {
                return Err(format!(
                    "`batch` was not run: `tool_calls[{index}].tool` must be a non-empty string."
                ));
            };
            Ok(Member {
                tool: tool.to_string(),
                parameters: object
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect()
}

/// Folds one result per slot back into one result per response call, in
/// response order: a native call answers with its slot, a wrapper with one
/// result whose rows are its members in member order.
///
/// A slot the plan does not account for, or a plan naming a slot the host did
/// not answer, is a defect of the host's answer; the fold then returns the
/// slots it was given so the step still answers every call it can.
pub(crate) fn fold(
    plan: &ToolExpansionPlan,
    completed: Vec<CompletedToolCall>,
) -> Vec<CompletedToolCall> {
    let wrapper_slots = plan
        .wrappers
        .iter()
        .flat_map(|wrapper| wrapper.rows.iter())
        .filter(|row| matches!(row, ExpandedRow::Slot { .. }))
        .count();
    let Some(native_calls) = completed.len().checked_sub(wrapper_slots) else {
        return completed;
    };
    let outer_calls = native_calls + plan.wrappers.len();
    let mut slots = completed.into_iter().map(Some).collect::<Vec<_>>();
    let mut wrappers = plan.wrappers.iter().peekable();
    let mut cursor = 0_usize;
    let mut folded = Vec::with_capacity(outer_calls);
    for position in 0..outer_calls {
        match wrappers.next_if(|wrapper| wrapper.source_position as usize == position) {
            Some(wrapper) => {
                let taken = wrapper
                    .rows
                    .iter()
                    .filter(|row| matches!(row, ExpandedRow::Slot { .. }))
                    .count();
                folded.push(fold_wrapper(wrapper, &mut slots));
                cursor += taken;
            }
            None => {
                if let Some(native) = slots.get_mut(cursor).and_then(Option::take) {
                    folded.push(native);
                }
                cursor += 1;
            }
        }
    }
    folded
}

fn fold_wrapper(
    wrapper: &ExpandedWrapper,
    slots: &mut [Option<CompletedToolCall>],
) -> CompletedToolCall {
    let mut rows = Vec::with_capacity(wrapper.rows.len());
    let mut model_rows = Vec::with_capacity(wrapper.rows.len());
    let mut attachments = Vec::new();
    let mut attachment_notices = Vec::new();
    let mut intent_outcomes = Vec::new();
    for row in &wrapper.rows {
        match row {
            ExpandedRow::Refused {
                member_index,
                tool,
                error,
            } => {
                let row = BatchResultRow::failure(*member_index as usize, tool, error.clone());
                model_rows.push(row.clone());
                rows.push(row);
            }
            ExpandedRow::Slot {
                member_index,
                tool,
                slot,
            } => {
                let index = *member_index as usize;
                let Some(member) = slots.get_mut(*slot as usize).and_then(Option::take) else {
                    let missing = Value::String(format!(
                        "the host returned no result for batch member {index}"
                    ));
                    let row = BatchResultRow::failure(index, tool, missing);
                    model_rows.push(row.clone());
                    rows.push(row);
                    continue;
                };
                let success = member.output.is_success();
                let value = member.output.value_for_projection();
                let (presented, member_attachments) = presented_member(member.model_return.parts);
                attachments.extend(member_attachments);
                attachment_notices.extend(member.model_return.attachment_notices);
                intent_outcomes.extend(member.intent_outcomes);
                if success {
                    rows.push(BatchResultRow::success(index, tool, value));
                    model_rows.push(BatchResultRow::success(index, tool, presented));
                } else {
                    rows.push(BatchResultRow::failure(index, tool, value));
                    model_rows.push(BatchResultRow::failure(index, tool, presented));
                }
            }
        }
    }
    let presented = serde_json::json!({ "results": model_rows }).to_string();
    let mut parts = vec![ModelToolReturnPart::text(presented)];
    parts.extend(attachments);
    CompletedToolCall {
        call_id: wrapper.call_id.clone(),
        provider_call_id: wrapper.provider_call_id.clone(),
        tool_name: wrapper.tool_name.clone(),
        args: wrapper.args.clone(),
        output: ToolCallOutput::success(serde_json::json!({ "results": rows })),
        model_return: ModelToolReturn {
            tool_name: wrapper.tool_name.clone(),
            parts,
            attachment_notices,
        },
        intent_outcomes,
        replay: wrapper.replay.clone(),
    }
}

/// A member's presentation as the model saw it on its own: its text blocks
/// joined (as JSON when the text is JSON), and its attachments, which the
/// wrapper's result carries after its rows.
fn presented_member(parts: Vec<ModelToolReturnPart>) -> (Value, Vec<ModelToolReturnPart>) {
    let mut text = String::new();
    let mut attachments = Vec::new();
    for part in parts {
        match part {
            ModelToolReturnPart::Text { text: block } => text.push_str(&block),
            attachment @ ModelToolReturnPart::Attachment(_) => attachments.push(attachment),
        }
    }
    let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    (value, attachments)
}

#[cfg(test)]
mod tests;
