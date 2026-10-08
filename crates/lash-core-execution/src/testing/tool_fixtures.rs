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
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A fixed provider that serves the command-free [`fixture_echo_definition`].
///
/// Unlike a permissive echo stub, an unknown tool name is a hard error rather
/// than echoing the argument anyway. Migrated tests therefore fail loudly if a
/// tool is renamed or a call is routed to the wrong provider, instead of
/// silently observing a plausible value.
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

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name() != FIXTURE_ECHO_TOOL {
            return ToolOutcome::err_fmt(format_args!("unknown fixture tool: {}", call.name()))
                .into();
        }
        ToolOutcome::ok(serde_json::json!({
            "echo": call.args.get("value").cloned().unwrap_or_default(),
        }))
        .into()
    }
}
