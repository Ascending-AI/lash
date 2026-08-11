use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
use lash_core::llm::types::{LlmOutputPart, LlmUsage};
use serde_json::Value;

use crate::responses_shared::ResponsesStreamState;
use crate::schema::responses_error_is_retryable;

pub(super) fn response_failed_error(provider: &str, event: &Value) -> LlmTransportError {
    let error = event
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| event.get("error"));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{provider} response failed"));
    let retryable = error.is_some_and(responses_error_is_retryable);
    LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Stream)
        .retryable(retryable)
        .with_raw(event.to_string())
}

fn has_nonempty_string(value: &Value, fields: &[&str]) -> bool {
    fields.iter().any(|field| {
        value
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    })
}

fn output_content_has_evidence(part: &Value) -> bool {
    match part.get("type").and_then(Value::as_str) {
        Some("output_text") | Some("reasoning_text") => has_nonempty_string(part, &["text"]),
        Some("refusal") => has_nonempty_string(part, &["refusal", "text"]),
        Some("") | None => false,
        Some(_) => true,
    }
}

pub(super) fn output_item_has_output_evidence(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => item
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| content.iter().any(output_content_has_evidence)),
        Some("reasoning") => reasoning_item_has_output_evidence(item),
        _ => false,
    }
}

/// Classify official Responses events whose content Lash does not project into
/// a first-class stream part. Returns `true` when the event type was handled.
pub(super) fn handle_evidence_only_event(
    event_type: &str,
    event: &Value,
    state: &mut ResponsesStreamState,
) -> bool {
    match event_type {
        // A named SSE ping loses its `event:` name in the shared framers. A
        // JSON object without a Responses type is benign unless it carries a
        // field that can hold generated output.
        "" => {
            state.streamed_item_content_received |= untyped_event_has_output_evidence(event);
        }
        // OpenRouter request-debug metadata describes the upstream request;
        // it is never model-generated response output.
        "response.debug" => {}
        "response.content_part.added" | "response.content_part.done" => {
            state.streamed_item_content_received |=
                event.get("part").is_some_and(output_content_has_evidence);
        }
        // These events still need structural handling in `responses_shared`,
        // so classify their required payload here and then return `false`.
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
            state.streamed_item_content_received |= event
                .get("part")
                .is_some_and(|part| has_nonempty_string(part, &["text"]));
            return false;
        }
        "response.reasoning_summary_text.delta" => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["delta"]);
            return false;
        }
        "response.reasoning_summary_text.done" => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["text"]);
            return false;
        }
        "response.reasoning_text.delta" | "response.refusal.delta" => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["delta"]);
        }
        "response.reasoning_text.done" => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["text"]);
        }
        "response.refusal.done" => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["refusal"]);
        }
        // Lifecycle events carry no generated content of their own. Their
        // response snapshots are retained and inspected independently.
        "response.created" | "response.in_progress" | "response.queued" => {}
        // These events prove output or hosted-tool work even when Lash does not
        // project their modality into an LlmOutputPart.
        "response.audio.done"
        | "response.audio.transcript.done"
        | "response.code_interpreter_call.completed"
        | "response.code_interpreter_call.in_progress"
        | "response.code_interpreter_call.interpreting"
        | "response.file_search_call.completed"
        | "response.file_search_call.in_progress"
        | "response.file_search_call.searching"
        | "response.web_search_call.completed"
        | "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.image_generation_call.completed"
        | "response.image_generation_call.generating"
        | "response.image_generation_call.in_progress"
        | "response.image_generation_call.partial_image"
        | "response.mcp_call.completed"
        | "response.mcp_call.failed"
        | "response.mcp_call.in_progress"
        | "response.mcp_list_tools.completed"
        | "response.mcp_list_tools.failed"
        | "response.mcp_list_tools.in_progress"
        | "response.output_text.annotation.added" => {
            state.streamed_item_content_received = true;
        }
        // Empty unsupported delta/done payloads are allocation markers; a
        // non-empty payload is possible generated output.
        "response.audio.delta"
        | "response.audio.transcript.delta"
        | "response.code_interpreter_call_code.delta"
        | "response.code_interpreter_call_code.done"
        | "response.mcp_call_arguments.delta"
        | "response.mcp_call_arguments.done"
        | "response.custom_tool_call_input.delta"
        | "response.custom_tool_call_input.done" => {
            state.streamed_item_content_received |=
                has_nonempty_string(event, &["delta", "code", "arguments", "input"]);
        }
        _ => return false,
    }
    true
}

fn untyped_event_has_output_evidence(event: &Value) -> bool {
    has_nonempty_string(
        event,
        &[
            "delta",
            "text",
            "refusal",
            "code",
            "arguments",
            "input",
            "content",
            "output_text",
            "encrypted_content",
        ],
    ) || event.get("part").is_some_and(output_content_has_evidence)
        || event
            .get("response")
            .is_some_and(response_value_has_output_evidence)
        || event
            .get("output")
            .is_some_and(|output| !output.as_array().is_some_and(Vec::is_empty))
        || event
            .get("usage")
            .is_some_and(lash_core::llm::types::provider_usage_has_quantities)
}

pub(super) fn reasoning_item_has_output_evidence(item: &Value) -> bool {
    item.get("encrypted_content").is_some_and(Value::is_string)
        || item
            .get("summary")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                })
            })
}

fn part_has_output_evidence(part: &LlmOutputPart) -> bool {
    match part {
        LlmOutputPart::Text { text, .. } => !text.is_empty(),
        LlmOutputPart::Reasoning { text, replay } => {
            !text.is_empty()
                || replay.as_ref().is_some_and(|replay| {
                    replay.encrypted_content.is_some()
                        || replay.summary.iter().any(|text| !text.is_empty())
                })
        }
        LlmOutputPart::ToolCall { input_json, .. } => !input_json.is_empty(),
    }
}

fn response_value_has_output_evidence(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output
                .iter()
                .any(|item| match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        !crate::responses_shared::message_text_from_item(item).is_empty()
                    }
                    Some("reasoning") => reasoning_item_has_output_evidence(item),
                    Some("function_call") => ["name", "arguments"].iter().any(|field| {
                        item.get(field)
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty())
                    }),
                    Some("") | None => false,
                    Some(_) => true,
                })
        })
        || response
            .get("output_text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
}

impl ResponsesStreamState {
    /// Whether the provider generated billable output, even when the
    /// accumulator cannot yet project it into a complete response part.
    pub fn output_started(&self) -> bool {
        self.unrecognized_event_observed
            || self.streamed_item_content_received
            || self
                .final_response
                .as_ref()
                .is_some_and(response_value_has_output_evidence)
            || !self.full_text.is_empty()
            || !self.pending_text_deltas.is_empty()
            || !self.reasoning_deltas.is_empty()
            || self
                .provider_usage
                .as_ref()
                .is_some_and(lash_core::llm::types::provider_usage_has_quantities)
            || self.usage != LlmUsage::default()
            || self.parts.iter().any(part_has_output_evidence)
            || self
                .tool_calls
                .values()
                .any(|tool_call| !tool_call.input_json.is_empty())
    }
}
