use lash_core::facade_support::reasoning_part;
use lash_core::session_model::{Message, MessageRole, Part, PartKind, PruneState, shared_parts};
use serde_json::Value;

use super::state::RlmReasoningPart;

pub(crate) fn turn_limit_final_message(message_id: String, max_turns: usize) -> Message {
    Message {
        id: message_id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part {
            id: format!("{message_id}.p0"),
            kind: PartKind::Text,
            content: format!(
                "Turn limit reached ({max_turns}). You MUST reply in plain prose now containing:\n\
                1. Summary of what you accomplished\n\
                2. List of remaining tasks not yet completed\n\
                3. Recommended next steps\n\
                Do NOT emit a <lashlang> block, invoke module operations, or call finish/control.continue_as."
            ),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: None,
    }
}

pub(super) fn internal_assistant_prose_message(
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
        parts.push(Part {
            id: format!("{id}.p{}", parts.len()),
            kind: PartKind::Prose,
            content,
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        });
    }
    Message {
        id,
        role: MessageRole::Assistant,
        parts: shared_parts(parts),
        origin,
    }
}

pub(super) fn finish_required_reminder_message(id: String, requires_schema: bool) -> Message {
    let content = if requires_schema {
        "Deliver the final answer from a paired `<lashlang>...</lashlang>` block by calling `finish <value>` with a value matching the required output schema. Plain text before the block is recorded only as progress."
    } else {
        "Your prose was recorded, but this turn requires an explicit final value. Add a paired `<lashlang>...</lashlang>` block containing `finish <value>`. Use `finish null` only when null is intentional."
    };
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: content.to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn finish_schema_mismatch_message(id: String) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: "The `finish` value didn't match the required output schema. Fix the value described in the failed-step observation and call `finish <corrected>` from another paired `<lashlang>...</lashlang>` block.".to_string(),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn invalid_lashlang_cell_message(id: String, error_text: &str) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: format!(
                "{error_text}\n\nReply again using exactly one paired `<lashlang>...</lashlang>` block, with no text after `</lashlang>`."
            ),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn output_limit_retry_message(id: String, output_token_cap: Option<usize>) -> Message {
    let cap = output_token_cap
        .map(|cap| format!(" (the request cap was {cap} tokens)"))
        .unwrap_or_default();
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part {
            id: format!("{id}.p0"),
            kind: PartKind::Text,
            content: format!(
                "Your answer was cut off by the output limit{cap} — retry with a shorter answer. Do less per cell and continue in a later step."
            ),
            attachment: None,
            tool_call_id: None,
            tool_name: None,
            tool_replay: None,
            prune_state: PruneState::Intact,
            reasoning_meta: None,
            response_meta: None,
        }]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn validate_finish_value(value: &Value, schema: &Value) -> Result<(), String> {
    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|err| format!("required output schema is invalid: {err}"))?;
    if let Err(errors) = compiled.validate(value) {
        let message = errors
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(message);
    }
    Ok(())
}
