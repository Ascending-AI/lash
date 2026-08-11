use lash_core::llm::types::{LlmOutputPart, LlmUsage};
use serde_json::Value;

use crate::responses_shared::ResponsesStreamState;

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

impl ResponsesStreamState {
    /// Whether the provider generated billable output, even when the
    /// accumulator cannot yet project it into a complete response part.
    pub fn output_started(&self) -> bool {
        self.streamed_item_content_received
            || !self.full_text.is_empty()
            || !self.pending_text_deltas.is_empty()
            || !self.reasoning_deltas.is_empty()
            || self.provider_usage.is_some()
            || self.usage != LlmUsage::default()
            || self.parts.iter().any(part_has_output_evidence)
            || self
                .tool_calls
                .values()
                .any(|tool_call| !tool_call.input_json.is_empty())
    }
}
