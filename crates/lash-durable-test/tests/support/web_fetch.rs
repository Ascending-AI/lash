//! A deferred `web.fetch` a code law's cells reach: never listed, granted
//! on first use by [`GrantFetch`], answering its url.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{ToolCall, ToolOutcome};

/// The deferred tool a cell reaches as `web.fetch`, granted on first use.
pub fn fetch_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        "tool:web_fetch",
        "web_fetch",
        "Fetches a url.",
        object.clone(),
        object,
    )
    .expect("the fetch tool's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["web"], "fetch"))
}

/// Serves `web.fetch`, which is never listed: only the resolver's grant
/// reaches it.
pub struct Fetch;

#[async_trait::async_trait]
impl lash_core::ToolProvider for Fetch {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        let definition = fetch_definition();
        (definition.id() == id).then(|| definition.manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "web_fetch").then(|| Arc::new(fetch_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!({ "url": call.args["url"].clone() })).into()
    }
}

/// Grants `web.fetch` to a cell that names it.
pub struct GrantFetch;

#[async_trait::async_trait]
impl lash_lashlang_runtime::DeferredToolResolver for GrantFetch {
    async fn resolve(
        &self,
        _cx: &lash_lashlang_runtime::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash_lashlang_runtime::Resolution> {
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "web.fetch" {
                    lash_lashlang_runtime::Resolution::Resolved(Box::new(
                        lash_lashlang_runtime::ToolGrant::new(fetch_definition())
                            .with_source_id(lash::tools::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    lash_lashlang_runtime::Resolution::NotAvailable
                };
                ((*path).to_owned(), resolution)
            })
            .collect()
    }
}
