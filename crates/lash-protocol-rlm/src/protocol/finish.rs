use crate::dialect::SessionDialect;
use lash_core::facade_support::reasoning_part;
use lash_core::session_model::{Message, MessageRole, Part, shared_parts};
use lash_sansio::TurnId;
use serde_json::Value;

use super::state::RlmReasoningPart;

#[cfg(feature = "testing")]
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

pub(super) fn internal_assistant_prose_message_for_turn(
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
            turn_id: TurnId::from(turn_id.to_string()),
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
    dialect: &SessionDialect,
    id: String,
    requires_schema: bool,
) -> Message {
    let tags = dialect.cell_tags();
    let repair = format!(
        "No code from that response executed. Markdown code fences do not execute here. Resend the needed program between `{}` and `{}` on their own lines, without backticks. {}",
        tags.open,
        tags.close,
        dialect.finish_required_copy(requires_schema, crate::plugin::RlmChannel::Cell),
    );
    Message {
        id: id.clone(),
        role: MessageRole::System,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), repair, None)]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
    }
}

pub(super) fn finish_schema_mismatch_message(dialect: &SessionDialect, id: String) -> Message {
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
    dialect: &SessionDialect,
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

pub(crate) fn validate_finish_value(value: &Value, schema: &Value) -> Result<(), String> {
    let compiled = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7.detect(schema))
        .should_validate_formats(true)
        .build(schema)
        .map_err(|err| format!("required output schema is invalid: {err}"))?;
    if !compiled.is_valid(value) {
        let errors = compiled.iter_errors(value);
        let message = errors
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(message);
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::validate_finish_value;
    use serde_json::json;

    #[test]
    fn finish_validation_keeps_formats_and_all_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "email": { "type": "string", "format": "email" },
                "count": { "type": "integer" }
            }
        });
        assert!(
            validate_finish_value(&json!({ "email": "sam@example.com", "count": 1 }), &schema)
                .is_ok()
        );
        let error =
            validate_finish_value(&json!({ "email": "invalid", "count": "invalid" }), &schema)
                .unwrap_err();
        assert!(error.contains("email"), "{error}");
        assert!(error.contains("integer"), "{error}");
        assert!(error.contains("; "), "{error}");
    }

    #[test]
    fn finish_validation_honors_draft202012() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "array",
            "prefixItems": [{ "type": "string", "format": "email" }],
            "items": false
        });
        assert!(validate_finish_value(&json!(["sam@example.com"]), &schema).is_ok());
        assert!(validate_finish_value(&json!([42]), &schema).is_err());
        assert!(validate_finish_value(&json!(["invalid"]), &schema).is_err());
        assert!(validate_finish_value(&json!(["sam@example.com", "extra"]), &schema).is_err());
    }
}
