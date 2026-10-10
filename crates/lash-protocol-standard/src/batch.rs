//! `batch` is protocol sugar (ADR 0116 §2): never a tool, never executed.
//!
//! The driver expands each `batch` call of a response into slots of the
//! Run's tool round, beside the response's native calls ([`crate::round`]),
//! and folds the slots' results back into one result per wrapper. Both directions are pure
//! functions of the recorded response and the turn's admitted configuration,
//! so a replay recomputes the identical plan and presentation.

use std::num::NonZeroUsize;

use lash_core::ToolDefinition;
use lash_core::facade_support::{ModelToolReturn, ModelToolReturnPart};
use lash_core::sansio::{CompletedToolCall, ExpandedRow, ExpandedWrapper, ToolExpansionPlan};
use lash_core::{ToolCallOutput, ToolControl};
use lash_tool_support::object_schema;
use serde_json::Value;

pub type BatchResultRow = lash_sansio::BatchResultRow;

/// The name the model calls the sugar by.
pub(crate) const BATCH_TOOL_NAME: &str = "batch";

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn batch_tool_definition(max_members: NonZeroUsize) -> lash_core::ToolDraft {
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
    ).expect("valid declared tool schemas")
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

pub(crate) struct Member {
    pub(crate) tool: String,
    pub(crate) parameters: Value,
}

pub(crate) fn parse_members(
    args: &Value,
    max_members: NonZeroUsize,
) -> Result<Vec<Member>, String> {
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
    // A member's settled control is the step's: the wrapper carries it, and
    // [`control_member`] names the member it came from.
    let mut control = None;
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
                if success && let Some(turn @ ToolControl::Turn { .. }) = &member.output.control {
                    control = Some(turn.clone());
                }
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
        output: ToolCallOutput {
            control,
            ..ToolCallOutput::success(serde_json::json!({ "results": rows }))
        },
        model_return: ModelToolReturn {
            tool_name: wrapper.tool_name.clone(),
            parts,
            attachment_notices,
        },
        intent_outcomes,
        replay: wrapper.replay.clone(),
    }
}

/// The member of the folded `batch` result `wrapper` whose settled control
/// the wrapper carries: the one member that succeeded calling a tool whose
/// call ends the turn, as a step runs at most one control call. Its identity
/// is its wrapper's child at its member index.
pub(crate) fn control_member(
    wrapper: &CompletedToolCall,
    ends_the_turn: &dyn Fn(&str) -> bool,
) -> Option<(lash_core::ToolCallId, String)> {
    let results = wrapper.output.value_for_projection();
    let rows =
        serde_json::from_value::<Vec<BatchResultRow>>(results.get("results")?.clone()).ok()?;
    rows.into_iter()
        .find(|row| row.success && ends_the_turn(&row.tool))
        .map(|row| (wrapper.call_id.child(row.index as u64), row.tool))
}

/// A member's presentation as the model saw it on its own: its text blocks
/// joined (as JSON when the text is JSON), and its attachments and retained
/// outputs, which the wrapper's result carries after its rows. A retained
/// output's row names its attachment; its witness is shown once, in its
/// block.
fn presented_member(parts: Vec<ModelToolReturnPart>) -> (Value, Vec<ModelToolReturnPart>) {
    let mut text = String::new();
    let mut attachments = Vec::new();
    for part in parts {
        match part {
            ModelToolReturnPart::Text { text: block } => text.push_str(&block),
            ModelToolReturnPart::Retained(retained) => {
                text.push_str(&format!(
                    "[output retained as attachment {}; its witness follows the results]",
                    retained.reference.id
                ));
                attachments.push(ModelToolReturnPart::Retained(retained));
            }
            attachment @ ModelToolReturnPart::Attachment(_) => attachments.push(attachment),
        }
    }
    let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    (value, attachments)
}

#[cfg(test)]
mod tests;
