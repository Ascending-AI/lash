//! Command-free, model-facing tool fixtures for cross-crate tests.
//!
//! These providers exercise the generic tool dispatch path without spawning a
//! process or touching the network. They stand in for product tool crates when
//! a test only needs *a* model-facing tool: the fixture echoes its argument
//! back inside a typed result, so assertions can pin the exact value that
//! travelled through dispatch, tool-call recording, and projection.

use std::sync::Arc;

use crate::{ToolCall, ToolContract, ToolDefinition, ToolManifest, ToolOutcome, ToolProvider};

/// Name of the fixture's echo tool.
pub const FIXTURE_ECHO_TOOL: &str = "fixture_echo";

/// The fixture's echo tool definition: echo `value` back as `echo`.
pub fn fixture_echo_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:fixture_echo",
        FIXTURE_ECHO_TOOL,
        "Echo the supplied literal value back as a typed result.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": {} },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "type": "object",
            "properties": { "echo": {} },
            "required": ["echo"],
            "additionalProperties": false
        }),
    )
}

/// A fixed provider that serves the command-free [`fixture_echo_definition`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FixtureTools;

impl FixtureTools {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ToolProvider for FixtureTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![fixture_echo_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == FIXTURE_ECHO_TOOL).then(|| Arc::new(fixture_echo_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        if call.name != FIXTURE_ECHO_TOOL {
            return ToolOutcome::err_fmt(format_args!("unknown fixture tool: {}", call.name));
        }
        ToolOutcome::ok(serde_json::json!({
            "echo": call.args.get("value").cloned().unwrap_or_default(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::run_tool;

    #[tokio::test]
    async fn fixture_echo_returns_its_argument_as_typed_data() {
        let outcome = run_tool(
            &FixtureTools,
            FIXTURE_ECHO_TOOL,
            &serde_json::json!({ "value": "alpha" }),
        )
        .await;
        assert_eq!(
            outcome.as_output().outcome,
            crate::ToolCallOutcome::Success(crate::ToolValue::untrusted_json(
                serde_json::json!({ "echo": "alpha" })
            ))
        );
    }
}
