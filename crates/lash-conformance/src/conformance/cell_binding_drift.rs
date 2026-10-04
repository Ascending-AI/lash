//! Shared tool provider for the current model-call binding laws.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::ToolDefinitionBindingExt as _;

/// What the registry offers for `tools.probe`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Probe {
    Registered,
    Removed,
}

/// The plugin that registers `probe` for `tools.probe`.
pub(super) fn probe_factory(
    probe: Probe,
    executions: Arc<AtomicUsize>,
) -> Arc<dyn crate::facade_support::PluginFactory> {
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(ProbeTool { probe, executions });
    Arc::new(crate::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("conformance-binding-probe"),
        crate::facade_support::PluginSpec::new().with_tool_provider(tools),
    ))
}

struct ProbeTool {
    probe: Probe,
    executions: Arc<AtomicUsize>,
}

impl ProbeTool {
    #[expect(
        clippy::expect_used,
        reason = "this module declares the tool or payload schema and admission checks its invariant"
    )]
    fn definition(&self) -> Option<crate::ToolDefinition> {
        let description = match self.probe {
            Probe::Registered => "Binding-drift probe tool.",
            Probe::Removed => return None,
        };
        let definition = crate::ToolDefinition::raw(
            "tool:probe",
            "probe",
            description,
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            serde_json::json!({ "type": "object" }),
        ).expect("valid declared tool schemas")
        .with_tool_binding(crate::ToolBinding::new(["tools"], "probe"));
        Some(definition)
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.definition()
            .map(|definition| definition.manifest())
            .into_iter()
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        let definition = self.definition()?;
        (definition.manifest.name == name).then(|| Arc::new(definition.contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcome::ok(serde_json::json!({ "probed": true })).into()
    }
}
