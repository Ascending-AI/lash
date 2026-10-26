use crate::dialect::SessionDialect;
use lash_core::facade_support::reasoning_part;
use lash_core::session_model::{Message, MessageRole, Part, shared_parts};
use lash_vocabulary::identity::TurnId;

use crate::driver_state::RlmReasoningPart;

pub(crate) fn internal_assistant_prose_message_for_turn(
    turn_id: &TurnId,
    message_id: String,
    content: String,
    reasoning: &[RlmReasoningPart],
) -> Message {
    prose_message(
        message_id,
        content,
        reasoning,
        Some(lash_core::MessageOrigin::TurnOutput {
            turn_id: turn_id.clone(),
            source: lash_core::TurnOutputSource::Plugin {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            },
            cell_id: None,
        }),
    )
}

fn prose_message(
    id: String,
    content: String,
    reasoning: &[RlmReasoningPart],
    origin: Option<lash_core::MessageOrigin>,
) -> Message {
    let mut parts = reasoning
        .iter()
        .enumerate()
        .map(|(index, part)| reasoning_part(&id, index, part.text.clone(), part.replay.clone()))
        .collect::<Vec<_>>();
    if !content.is_empty() {
        parts.push(Part::prose(format!("{id}.p{}", parts.len()), content, None));
    }
    Message {
        id,
        role: MessageRole::Assistant,
        parts: shared_parts(parts),
        origin,
        reply_marker: None,
    }
}

/// The reminder a reply that ended a finish-required turn with no control
/// call reads, naming the finish calls `finishing` its surface offers.
pub(crate) fn finish_required_reminder_message(
    dialect: &SessionDialect,
    id: String,
    finishing: &[String],
) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            dialect.finish_required_copy(finishing),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    }
}

/// What the model reads when input arrived at BeforeCompletion and its
/// control call therefore did not end the turn.
pub(crate) fn completion_superseded_message(
    id: String,
    candidate: &lash_core::CompletionCandidate,
) -> Message {
    let did = match candidate.control {
        lash_core::TurnControl::Finish { .. } => "finish the turn",
        lash_core::TurnControl::SwitchAgentFrame { .. } => "switch the agent frame",
    };
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            format!(
                "The `{}` call did not {did}: new input arrived before it took effect, so it was superseded and the turn goes on. Read the new input, and end the turn again when the work is complete.",
                candidate.tool_name
            ),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    }
}

#[cfg(feature = "testing")]
pub(crate) fn internal_assistant_prose_message(
    message_id: String,
    content: String,
    reasoning: &[RlmReasoningPart],
) -> Message {
    prose_message(
        message_id,
        content,
        reasoning,
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    )
}
