//! Provider response construction and observed tool results.
use super::*;

/// A text answer, streamed as one delta.
pub(super) fn text(request: &LlmRequest, text: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: text.to_owned(),
        }));
    }
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// One native tool call.
/// A model call of the operation `tool`, by its Standard-mode name.
pub(super) fn call(id: &str, tool: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: id.to_owned(),
            tool_name: tool.replace('.', "__"),
            input_json: input.to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}

/// The tool results a request carries, rendered.
pub(super) fn results(request: &LlmRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            lash_core::llm::types::LlmContentBlock::ToolResult { content, .. } => {
                Some(format!("{content:?}"))
            }
            _ => None,
        })
        .collect()
}
