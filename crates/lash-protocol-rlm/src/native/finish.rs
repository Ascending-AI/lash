use lash_core::facade_support::reasoning_part;
use lash_core::session_model::{Message, MessageRole, Part, shared_parts};

use crate::dialect::RlmDialect;

use super::state::RlmReasoningPart;

pub(crate) fn turn_limit_final_message(
    dialect: &dyn RlmDialect,
    message_id: String,
    max_turns: usize,
) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{message_id}.p0"),
            super::prompt::transport_copy(&dialect.turn_limit_final_copy(max_turns), dialect),
            None,
        )]),
        origin: None,
    }
}

pub(super) fn internal_assistant_prose_message_for_turn(
    turn_id: &str,
    message_id: String,
    content: String,
    reasoning: &[RlmReasoningPart],
) -> Message {
    prose_message(
        message_id,
        content,
        reasoning,
        Some(lash_core::MessageOrigin::TurnOutput {
            turn_id: turn_id.to_string(),
            source: lash_core::TurnOutputSource::Plugin {
                plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            },
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
    }
}

pub(super) fn finish_required_reminder_message(
    dialect: &dyn RlmDialect,
    id: String,
    requires_schema: bool,
) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            super::prompt::transport_copy(&dialect.finish_required_copy(requires_schema), dialect),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn finish_schema_mismatch_message(dialect: &dyn RlmDialect, id: String) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            super::prompt::transport_copy(&dialect.finish_schema_mismatch_copy(), dialect),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) use crate::protocol::finish::validate_finish_value;

/// The transcript record left behind when a turn exhausts its no-progress
/// budget.
///
/// It is a system message rather than dialect copy because it addresses no
/// language construct: the turn is over, and nothing will read it as an
/// instruction to repair.
pub(super) fn no_progress_stop_message(id: String, attempts: usize) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            format!(
                "Stopped after {attempts} consecutive model responses that executed nothing. \
                 The turn's no-progress budget is exhausted; no further model calls were made."
            ),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}
