//! `finish`, the control tool a `TerminalRequired` standard session is
//! offered (DESIGN §2): its call ends the turn with its `value`, and
//! returns nothing.

use std::sync::Arc;

use async_trait::async_trait;
use lash_core::{
    ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider, TurnControls,
};
use serde_json::{Value, json};

/// The name the model calls `finish` by: what a turn it ended records as
/// its finishing tool.
pub(crate) const FINISH_TOOL_NAME: &str = "finish";

const FINISH_TOOL_ID: &str = "tool:finish";

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool schema and admission checks its invariant"
)]
fn finish_tool_definition() -> ToolDefinition {
    ToolDefinition::control(
        FINISH_TOOL_ID,
        FINISH_TOOL_NAME,
        "End the turn with `value` as its answer. Call it on its own once every other call has returned; it returns nothing. A prose reply does not end this turn.",
        json!({
            "type": "object",
            "properties": {
                "value": { "description": "The turn's answer: any JSON value." }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        TurnControls::finish(),
    )
    .expect("valid declared tool schema")
    // No work of its own: the call is its control.
    .with_execution(std::time::Duration::from_secs(30))
    // Its body reads its argument and nothing else, so running it again is
    // safe: a call a crash cut off runs again on the owner that resumes it.
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(3).expect("three is non-zero"),
        100,
        1_000,
    ))
}

/// Provides `finish` to a `TerminalRequired` session.
pub(crate) struct FinishToolProvider;

#[async_trait]
impl ToolProvider for FinishToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![finish_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == FINISH_TOOL_NAME).then(|| Arc::new(finish_tool_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() != FINISH_TOOL_NAME {
            return ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into();
        }
        ToolOutcome::finish(call.args.get("value").cloned().unwrap_or(Value::Null)).into()
    }
}
