pub(crate) mod history;

use std::sync::Arc;

#[cfg(any(test, feature = "testing"))]
use lash_core::llm::types::{LlmContentBlock, LlmMessage};
use lash_core::{ProtocolBuildInput, TurnDriverPreamble};

use crate::dialect::SessionDialect;
#[cfg(test)]
use crate::projection::rlm_protocol_event;

#[cfg(feature = "testing")]
use crate::driver_state::RlmReasoningPart;
#[cfg(test)]
use crate::native::projector::RlmContextProjector;
pub(crate) use crate::native::projector::build_rlm_preamble_with_dialect;
#[cfg(feature = "testing")]
use crate::protocol::finish::internal_assistant_prose_message;
use history::RlmHistoryRenderInput;
#[cfg(any(test, feature = "testing"))]
use history::render_history_messages;
#[cfg(feature = "testing")]
use lash_core::llm::types::{LlmRole, append_assistant_text_part};
#[cfg(feature = "testing")]
use lash_core::{LlmOutputPart, Message};

/// A prompt-only RLM preamble's configuration: the host's selected dialect
/// and the prompt knobs.
#[derive(Clone)]
pub struct RlmProjectorConfig {
    pub dialect: crate::dialect::CellDialect,
    pub max_output_chars: usize,
}

pub(crate) struct RlmPreambleConfig {
    pub(crate) max_output_chars: usize,
}

impl RlmProjectorConfig {
    /// A preamble in `dialect`, with the default prompt knobs.
    pub fn new(dialect: crate::dialect::CellDialect) -> Self {
        Self {
            dialect,
            max_output_chars: 10_000,
        }
    }
}

pub fn build_rlm_preamble(
    input: ProtocolBuildInput,
    config: RlmProjectorConfig,
) -> TurnDriverPreamble {
    let dialect: Arc<SessionDialect> =
        Arc::new(SessionDialect::prompt_only(config.dialect.clone()));
    build_rlm_preamble_with_dialect(
        input,
        RlmPreambleConfig {
            max_output_chars: config.max_output_chars,
        },
        dialect,
    )
}

impl crate::native::projector::RlmContextProjector {
    #[cfg(test)]
    fn format_history(&self, events: &[lash_core::SessionHistoryRecord]) -> String {
        let messages = render_history_messages(&RlmHistoryRenderInput {
            dialect: self.dialect.as_ref(),
            events,
            turn_messages: &lash_core::facade_support::MessageSequence::default(),
            max_output_chars: self.max_output_chars,
            protocol_iteration: 0,
        })
        .expect("valid history fixture");
        messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Render one durable assistant message through the production RLM history
/// renderer for provider conformance tests.
#[cfg(feature = "testing")]
pub(crate) fn render_conformance_history_message(
    message: lash_core::Message,
) -> Result<LlmMessage, String> {
    let dialect = SessionDialect::prompt_only(crate::dialect::CellDialect::typescript());
    let events = [lash_core::SessionHistoryRecord::Conversation(
        lash_core::session_model::ConversationRecord::from_message(message),
    )];

    let rendered = render_history_messages(&RlmHistoryRenderInput {
        dialect: &dialect,
        events: &events,
        turn_messages: &lash_core::facade_support::MessageSequence::default(),
        max_output_chars: 10_000,
        protocol_iteration: 0,
    })
    .map_err(|error| error.to_string())?;
    let attachment_count = rendered
        .iter()
        .flat_map(|message| message.blocks.iter())
        .flat_map(LlmContentBlock::attachments)
        .count();
    match rendered.as_slice() {
        [message] if attachment_count == 0 => Ok(message.clone()),
        _ => Err(format!(
            "RLM conformance history rendered {} messages and {} attachments; expected exactly one message and no attachments",
            rendered.len(),
            attachment_count
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(feature = "testing")]
struct ProjectedResponse {
    assistant_text: String,
    reasoning: Vec<RlmReasoningPart>,
}

#[derive(Debug)]
#[cfg(feature = "testing")]
struct NativeToolCall {
    tool_name: String,
}

#[cfg(feature = "testing")]
fn project_response(parts: Vec<LlmOutputPart>) -> Result<ProjectedResponse, NativeToolCall> {
    let mut assistant_text = String::new();
    let mut reasoning = Vec::new();
    for part in parts {
        match part {
            LlmOutputPart::Text { text, .. } => {
                append_assistant_text_part(&mut assistant_text, &text);
            }
            LlmOutputPart::Reasoning { text, replay } => {
                let text = if text.trim().is_empty() {
                    replay
                        .as_ref()
                        .map(|meta| meta.summary.join("\n\n"))
                        .unwrap_or_default()
                } else {
                    text
                };
                if !text.trim().is_empty() || replay.as_ref().is_some_and(|meta| !meta.is_empty()) {
                    reasoning.push(RlmReasoningPart { text, replay });
                }
            }
            LlmOutputPart::ToolCall { tool_name, .. } => {
                return Err(NativeToolCall { tool_name });
            }
        }
    }
    Ok(ProjectedResponse {
        assistant_text,
        reasoning,
    })
}

/// Test support for exercising the production RLM response-to-history seam.
/// The bridge uses the same typed response projection, durable `Part`
/// representation, JSON round trip, and RLM history renderer as production.
#[cfg(feature = "testing")]
pub fn project_conformance_messages_through_rlm_history(
    messages: Vec<LlmMessage>,
) -> Result<Vec<LlmMessage>, String> {
    messages
        .into_iter()
        .map(|message| {
            if !matches!(message.role, LlmRole::Assistant) {
                return Ok(message);
            }
            let parts = message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text {
                        text,
                        response_meta,
                        ..
                    } => Some(LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: response_meta.clone(),
                    }),
                    LlmContentBlock::Reasoning { text, replay } => Some(LlmOutputPart::Reasoning {
                        text: text.clone(),
                        replay: replay.clone(),
                    }),
                    LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                    } => Some(LlmOutputPart::ToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        input_json: input_json.clone(),
                        replay: replay.clone(),
                    }),
                    LlmContentBlock::Attachment { .. } | LlmContentBlock::ToolResult { .. } => None,
                })
                .collect();
            let projected = project_response(parts).map_err(|tool_call| {
                format!(
                    "RLM conformance history fixture contains native tool call `{}`",
                    tool_call.tool_name
                )
            })?;
            let durable = internal_assistant_prose_message(
                "conformance.rlm.assistant".to_string(),
                projected.assistant_text,
                &projected.reasoning,
            );
            let encoded = serde_json::to_string(&durable).map_err(|err| {
                format!("RLM conformance history message did not serialize: {err}")
            })?;
            let durable: Message = serde_json::from_str(&encoded).map_err(|err| {
                format!("RLM conformance history message did not deserialize: {err}")
            })?;
            crate::driver::render_conformance_history_message(durable)
        })
        .collect()
}
