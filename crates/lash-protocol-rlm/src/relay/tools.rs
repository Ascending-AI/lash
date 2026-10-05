//! Relay's two control tools. Both are recorded calls like any other: the
//! driver reads their arguments from the step's call records, so the tools
//! themselves only validate and acknowledge.

use lash_core::{ToolCall, ToolDefinition, ToolOutcome};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::{Value, json};

use super::{NEXT_TOOL, RelayNext, SEND_USER_OUTPUT_TOOL};

#[expect(clippy::expect_used, reason = "the tool declares fixed valid schemas")]
pub(crate) fn next_tool_definition(cell_noun: &str) -> ToolDefinition {
    ToolDefinition::raw(
        "tool:next",
        NEXT_TOOL,
        format!(
            "End this step and hand the next one its baton. `context` becomes your whole working memory in the next prompt, one block per entry; `vars` rebuilds the next step's top-level variables; everything else is wiped. `final: true` ends the turn. Terminal action: the last call in the {cell_noun}. Nothing commits unless the {cell_noun} finishes without error right after it."
        ),
        json!({
            "type": "object",
            "properties": {
                "context": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "The next step's working memory, one entry per block."
                },
                "vars": {
                    "type": "object",
                    "additionalProperties": true,
                    "description": "Plain values the next step finds as top-level variables."
                },
                "final": {
                    "type": "boolean",
                    "description": "End the turn once this step commits."
                }
            },
            "required": ["context"],
            "additionalProperties": false,
        }),
        json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"},
                "context_entries": {"type": "integer", "minimum": 0},
                "context_chars": {"type": "integer", "minimum": 0},
                "vars": {"type": "array", "items": {"type": "string"}},
                "final": {"type": "boolean"}
            },
            "required": ["ok", "context_entries", "context_chars", "vars", "final"],
            "additionalProperties": false,
        }),
    )
    .expect("valid declared tool schemas")
    .with_examples(vec![
        "await control.next({ context: [...context, \"read config.toml: port is 8080\"], vars: { port: 8080 } });".into(),
    ])
    .with_tool_binding(ToolBinding::new(["control"], NEXT_TOOL))
}

#[expect(clippy::expect_used, reason = "the tool declares fixed valid schemas")]
pub(crate) fn send_user_output_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:send_user_output",
        SEND_USER_OUTPUT_TOOL,
        "Send text to the user. It is delivered only when this step commits; a step that fails or never calls `control.next` sends nothing.",
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "What the user reads."}
            },
            "required": ["text"],
            "additionalProperties": false,
        }),
        json!({
            "type": "object",
            "properties": {"buffered": {"type": "boolean"}},
            "required": ["buffered"],
            "additionalProperties": false,
        }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["control"], SEND_USER_OUTPUT_TOOL))
}

/// `control.next`: validate the baton. The commit is the driver's.
pub(crate) fn execute_next(call: &ToolCall<'_>, budget_chars: usize) -> ToolOutcome {
    match RelayNext::from_args(call.args, budget_chars) {
        Ok(next) => {
            let mut vars = next.vars.keys().cloned().collect::<Vec<_>>();
            vars.sort();
            ToolOutcome::ok(json!({
                "ok": true,
                "context_entries": next.context.len(),
                "context_chars": super::context_chars(&next.context),
                "vars": vars,
                "final": next.final_turn,
            }))
        }
        Err(error) => ToolOutcome::err(Value::String(format!("next refused: {error}"))),
    }
}

/// `control.send_user_output`: acknowledge. Delivery is the driver's, at
/// commit.
pub(crate) fn execute_send_user_output(call: &ToolCall<'_>) -> ToolOutcome {
    match call.args.get("text") {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            ToolOutcome::ok(json!({ "buffered": true }))
        }
        _ => ToolOutcome::err(json!("send_user_output needs `text`, a non-empty string")),
    }
}
