//! Relay's control tool, `control.next`. It is a recorded call like any
//! other: the driver reads its arguments from the step's call records, so the
//! tool itself only validates and acknowledges.

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::{ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider};
use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
use serde_json::{Value, json};

use super::{NEXT_TOOL, RelayNext, RelaySettings};

/// A relay session's control tools: `next` alone. A relay session has no
/// `continue_as` (its context is its own to rewrite) and no `read_output`
/// (nothing it printed outlives its step).
pub(crate) struct RelayControlToolsProvider {
    pub(crate) settings: RelaySettings,
}

#[async_trait]
impl ToolProvider for RelayControlToolsProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![next_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == NEXT_TOOL).then(|| Arc::new(next_tool_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            NEXT_TOOL => execute_next(&call, self.settings.context_budget_chars).into(),
            other => ToolOutcome::err_fmt(format_args!("Unknown tool: {other}")).into(),
        }
    }
}

#[expect(clippy::expect_used, reason = "the tool declares fixed valid schemas")]
fn next_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:next",
        NEXT_TOOL,
        "End this step and hand the next one its baton. `context` becomes your whole working memory in the next request, one block per entry; `vars` rebuilds the next step's top-level variables; everything else is wiped. The last call in the program: nothing commits unless the program finishes without error right after it.",
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
                "vars": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["ok", "context_entries", "context_chars", "vars"],
            "additionalProperties": false,
        }),
    )
    .expect("valid declared tool schemas")
    .with_examples(vec![
        "await control.next({ context: [...context, \"read config.toml: port is 8080\"], vars: { port: 8080 } });".into(),
    ])
    .with_tool_binding(ToolBinding::new(["control"], NEXT_TOOL))
}

/// `control.next`: validate the baton. The commit is the driver's.
fn execute_next(call: &ToolCall<'_>, budget_chars: usize) -> ToolOutcome {
    match RelayNext::from_args(call.args, budget_chars) {
        Ok(next) => {
            let mut vars = next.vars.keys().cloned().collect::<Vec<_>>();
            vars.sort();
            ToolOutcome::ok(json!({
                "ok": true,
                "context_entries": next.context.len(),
                "context_chars": super::context_chars(&next.context),
                "vars": vars,
            }))
        }
        Err(error) => ToolOutcome::err(Value::String(format!("next refused: {error}"))),
    }
}
