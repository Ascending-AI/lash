use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind};
use lash_core::llm::types::{LlmOutputPart, LlmUsage};
use serde_json::Value;

use crate::responses_shared::ResponsesStreamState;
use crate::responses_stream_event::ResponsesStreamEvent;
use crate::schema::{classify_openai_error, responses_error_retry_verdict};

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
    let retry_verdict = error.map(responses_error_retry_verdict).unwrap_or_default();
    let failure = LlmTransportError::new(message)
        .with_kind(ProviderFailureKind::Stream)
        .with_retry_verdict(retry_verdict)
        .with_raw(event.to_string());
    classify_openai_error(event, failure)
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
/// a first-class stream part.
pub(super) fn handle_output_evidence_event(
    event_type: ResponsesStreamEvent,
    event: &Value,
    state: &mut ResponsesStreamState,
) {
    match event_type {
        // A named SSE ping loses its `event:` name in the shared framers and
        // arrives as `{}`. Every other untyped payload is possible generated
        // output: extensions must fail safe without a field-name allowlist.
        ResponsesStreamEvent::EmptyPing => {
            state.streamed_item_content_received |=
                !event.as_object().is_some_and(serde_json::Map::is_empty);
        }
        // OpenRouter request-debug metadata describes the upstream request;
        // it is never model-generated response output.
        ResponsesStreamEvent::ResponseDebug => {}
        ResponsesStreamEvent::ResponseContentPartAdded
        | ResponsesStreamEvent::ResponseContentPartDone => {
            state.streamed_item_content_received |=
                event.get("part").is_some_and(output_content_has_evidence);
        }
        ResponsesStreamEvent::ResponseReasoningSummaryPartAdded
        | ResponsesStreamEvent::ResponseReasoningSummaryPartDone => {
            state.streamed_item_content_received |= event
                .get("part")
                .is_some_and(|part| has_nonempty_string(part, &["text"]));
        }
        ResponsesStreamEvent::ResponseReasoningSummaryTextDelta => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["delta"]);
        }
        ResponsesStreamEvent::ResponseReasoningSummaryTextDone => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["text"]);
        }
        ResponsesStreamEvent::ResponseReasoningTextDelta
        | ResponsesStreamEvent::ResponseRefusalDelta => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["delta"]);
        }
        ResponsesStreamEvent::ResponseReasoningTextDone => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["text"]);
        }
        ResponsesStreamEvent::ResponseRefusalDone => {
            state.streamed_item_content_received |= has_nonempty_string(event, &["refusal"]);
        }
        // Lifecycle events carry no generated content of their own. Their
        // response snapshots are retained and inspected independently.
        ResponsesStreamEvent::ResponseCreated
        | ResponsesStreamEvent::ResponseInProgress
        | ResponsesStreamEvent::ResponseQueued => {}
        // These events prove output or hosted-tool work even when Lash does not
        // project their modality into an LlmOutputPart.
        ResponsesStreamEvent::ResponseAudioDone
        | ResponsesStreamEvent::ResponseAudioTranscriptDone
        | ResponsesStreamEvent::ResponseCodeInterpreterCallCompleted
        | ResponsesStreamEvent::ResponseCodeInterpreterCallInProgress
        | ResponsesStreamEvent::ResponseCodeInterpreterCallInterpreting
        | ResponsesStreamEvent::ResponseFileSearchCallCompleted
        | ResponsesStreamEvent::ResponseFileSearchCallInProgress
        | ResponsesStreamEvent::ResponseFileSearchCallSearching
        | ResponsesStreamEvent::ResponseWebSearchCallCompleted
        | ResponsesStreamEvent::ResponseWebSearchCallInProgress
        | ResponsesStreamEvent::ResponseWebSearchCallSearching
        | ResponsesStreamEvent::ResponseImageGenerationCallCompleted
        | ResponsesStreamEvent::ResponseImageGenerationCallGenerating
        | ResponsesStreamEvent::ResponseImageGenerationCallInProgress
        | ResponsesStreamEvent::ResponseImageGenerationCallPartialImage
        | ResponsesStreamEvent::ResponseMcpCallCompleted
        | ResponsesStreamEvent::ResponseMcpCallFailed
        | ResponsesStreamEvent::ResponseMcpCallInProgress
        | ResponsesStreamEvent::ResponseMcpListToolsCompleted
        | ResponsesStreamEvent::ResponseMcpListToolsFailed
        | ResponsesStreamEvent::ResponseMcpListToolsInProgress
        | ResponsesStreamEvent::ResponseOutputTextAnnotationAdded => {
            state.streamed_item_content_received = true;
        }
        // Empty unsupported delta/done payloads are allocation markers; a
        // non-empty payload is possible generated output.
        ResponsesStreamEvent::ResponseAudioDelta
        | ResponsesStreamEvent::ResponseAudioTranscriptDelta
        | ResponsesStreamEvent::ResponseCodeInterpreterCallCodeDelta
        | ResponsesStreamEvent::ResponseCodeInterpreterCallCodeDone
        | ResponsesStreamEvent::ResponseMcpCallArgumentsDelta
        | ResponsesStreamEvent::ResponseMcpCallArgumentsDone
        | ResponsesStreamEvent::ResponseCustomToolCallInputDelta
        | ResponsesStreamEvent::ResponseCustomToolCallInputDone => {
            state.streamed_item_content_received |=
                has_nonempty_string(event, &["delta", "code", "arguments", "input"]);
        }
        ResponsesStreamEvent::ResponseCompleted
        | ResponsesStreamEvent::ResponseDone
        | ResponsesStreamEvent::ResponseFailed
        | ResponsesStreamEvent::ResponseFunctionCallArgumentsDelta
        | ResponsesStreamEvent::ResponseFunctionCallArgumentsDone
        | ResponsesStreamEvent::ResponseIncomplete
        | ResponsesStreamEvent::ResponseOutputItemAdded
        | ResponsesStreamEvent::ResponseOutputItemDone
        | ResponsesStreamEvent::ResponseOutputTextDelta
        | ResponsesStreamEvent::ResponseOutputTextDone
        | ResponsesStreamEvent::Unknown => {}
    }
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
