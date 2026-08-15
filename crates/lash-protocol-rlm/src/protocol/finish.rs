use lash_core::facade_support::reasoning_part;
use lash_core::session_model::{Message, MessageRole, Part, shared_parts};
use serde_json::Value;

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
            dialect.turn_limit_final_copy(max_turns),
            None,
        )]),
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
            dialect.finish_required_copy(requires_schema),
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
            dialect.finish_schema_mismatch_copy(),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn invalid_cell_message(
    dialect: &dyn RlmDialect,
    id: String,
    error_text: &str,
) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            dialect.invalid_cell_retry_copy(error_text),
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

/// The retry copy a model reads after the output limit truncated its answer.
///
/// Vocabulary-taking so the walker can render it in both dialects: "per cell"
/// is TypeScript's noun for a unit of code, and a Lashlang reader has only ever
/// been shown blocks.
pub(crate) fn output_limit_retry_copy(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    output_token_cap: Option<usize>,
) -> String {
    let cap = output_token_cap
        .map(|cap| format!(" (the request cap was {cap} tokens)"))
        .unwrap_or_default();
    let noun = vocabulary.cell_noun;
    format!(
        "Your answer was cut off by the output limit{cap} — retry with a shorter answer. Do less per {noun} and continue in a later step."
    )
}

pub(super) fn output_limit_retry_message(
    vocabulary: crate::dialect::DialectPromptVocabulary,
    id: String,
    output_token_cap: Option<usize>,
) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(
            format!("{id}.p0"),
            output_limit_retry_copy(vocabulary, output_token_cap),
            None,
        )]),
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
