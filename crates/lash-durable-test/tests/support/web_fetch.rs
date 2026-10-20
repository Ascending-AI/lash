//! A deferred `web.fetch` a code law's cells reach: never listed, granted
//! on first use by [`GrantFetch`], answering its url.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{ToolCall, ToolOutcome};

/// The deferred tool a cell reaches as `web.fetch`, granted on first use.
pub fn fetch_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        "tool:web_fetch",
        "web.fetch",
        "Fetches a url.",
        object.clone(),
        object,
    )
    .expect("the fetch tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// Serves `web.fetch`, which is never listed: only the resolver's grant
/// reaches it.
pub struct Fetch;

#[async_trait::async_trait]
impl lash_core::ToolProvider for Fetch {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_core::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_core::ToolManifest> {
        let definition = fetch_definition();
        (definition.id() == id).then(|| definition.manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "web.fetch").then(|| Arc::new(fetch_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!({ "url": call.args["url"].clone() })).into()
    }
}

/// The deferred provider that resolves `web.fetch` for a cell that names
/// it, to the tool [`Fetch`] serves under `tools`. It serves nothing itself.
pub struct GrantFetch;

#[async_trait::async_trait]
impl lash_core::ToolProvider for GrantFetch {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name())).into()
    }
}

#[async_trait::async_trait]
impl lash::tools::DeferredToolProvider for GrantFetch {
    async fn resolve(
        &self,
        _cx: &lash::tools::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, lash::tools::DeferredOutcome> {
        paths
            .iter()
            .map(|path| {
                let outcome = if *path == "web.fetch" {
                    lash::tools::DeferredOutcome::Resolved {
                        provider: lash::plugins::ToolProviderHandle::new(
                            lash::plugins::RegistryKind::Session,
                            "tools",
                            lash::tool_provider_key!("tools"),
                        ),
                        definition: fetch_definition(),
                        binding: None,
                    }
                } else {
                    lash::tools::DeferredOutcome::Unavailable { note: None }
                };
                ((*path).to_owned(), outcome)
            })
            .collect()
    }
}

/// The plugin that registers [`GrantFetch`] as a session's deferred
/// provider.
pub fn grant_fetch() -> Arc<dyn lash::plugins::PluginFactory> {
    Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("grant-fetch"),
        lash::plugins::PluginSpec::new().with_deferred_provider(
            lash::tool_provider_key!("deferred"),
            lash::tools::DeferredToolProviderRegistration::new(Arc::new(GrantFetch)),
        ),
    ))
}
