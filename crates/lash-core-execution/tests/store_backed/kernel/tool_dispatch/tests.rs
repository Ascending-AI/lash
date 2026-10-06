// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::plugin::PluginSessionRequest;
use crate::plugin::StaticPluginFactory;
use crate::{ToolCall, ToolOutcome, ToolProvider};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod composition_laws;

fn named_beta_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
}

fn manifests(definitions: Vec<crate::ToolDefinition>) -> Vec<crate::ToolManifest> {
    definitions
        .into_iter()
        .map(|tool| tool.manifest())
        .collect()
}

struct HiddenDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for HiddenDispatchTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![named_beta_tool("hidden")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (name == "hidden").then(|| Arc::new(named_beta_tool("hidden").contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("hidden")).into()
    }
}

mod single_gate;
