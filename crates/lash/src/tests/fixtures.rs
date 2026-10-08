//! Host tools the facade laws share.

use super::*;
use lash_core::ToolDefinitionBindingExt as _;

/// `app_lookup`, a deferring host tool that answers `{ "ok": true }`.
pub(crate) struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

fn app_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas"),
        "app_lookup",
    )
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}
