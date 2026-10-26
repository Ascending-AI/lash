//! A deferred `web.fetch` a code law's cells reach: never listed, granted
//! on first use by [`GrantFetch`], answering its url.

#![allow(dead_code)]

use crate::packages;

use std::sync::Arc;

use lash_core::{ToolCall, ToolOutcome};

/// The deferred tool a cell reaches as `web.fetch`, granted on first use.
pub fn fetch_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_vocabulary::tool_contract::ToolDefinition::raw(
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
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_vocabulary::tool_contract::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        let definition = fetch_definition();
        (definition.id() == id).then(|| definition.manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
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
impl lash_core::plugin::packages::DeferredPackageProvider for GrantFetch {
    fn validate_grant(
        &self,
        grant: &lash_kernel_value::package::DeferredGrant,
        registry: lash_vocabulary::claims::RegistryKind,
    ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
        packages::validate_grant("tools", "deferred", "web", grant, registry)
    }
    async fn resolve(
        &self,
        _: &lash_core::DeferredResolveContext<'_>,
        _: &lash_kernel_value::package::DeferredGrant,
        requested: &lash_kernel_value::names::QualifiedName,
    ) -> Result<
        lash_core::plugin::packages::DeferredAnswer,
        lash_core::plugin::packages::ResolverFault,
    > {
        use lash_core::plugin::packages::DeferredAnswer;
        if requested.member.as_str() != "fetch" {
            return Ok(DeferredAnswer::Unavailable { note: None });
        }
        let definition = fetch_definition();
        let route = lash_core_store::tool_surface::OperationRoute::resolved(
            lash::plugins::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                "tools",
                lash::tool_provider_key!("tools"),
            ),
            definition.clone(),
            None,
        );
        Ok(DeferredAnswer::Resolved {
            definition: Box::new(definition),
            route: Box::new(route),
        })
    }
}

/// The plugin that registers [`GrantFetch`] as a session's deferred
/// provider.
pub fn grant_fetch() -> Arc<dyn lash::plugins::PluginFactory> {
    Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial("tools"),
        packages::deferred_spec(
            "tools",
            lash::tool_provider_key!("deferred"),
            "web",
            Arc::new(GrantFetch),
        )
        .with_tool_provider(lash::tool_provider_key!("tools"), Arc::new(Fetch)),
    ))
}
