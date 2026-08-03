use crate::LashCore;
use std::sync::Arc;

/// Read-only projection of the tool composition configured on a [`LashCore`].
///
/// The view delegates to the same internal registry implementation used by
/// runtime sessions. It does not accept providers and cannot mutate catalog
/// membership.
#[derive(Clone)]
pub struct ToolCatalogView {
    registry: Arc<lash_core::ToolRegistry>,
}

impl ToolCatalogView {
    /// Manifests composed from the core's configured providers and plugins.
    pub fn manifests(&self) -> Vec<lash_core::ToolManifest> {
        lash_core::facade_support::tool_registry_manifests(&self.registry)
    }

    /// Resolve the full contract the runtime registry would use for `name`.
    pub fn resolve_contract(
        &self,
        name: &str,
    ) -> Result<Arc<lash_core::ToolContract>, ToolCatalogMiss> {
        resolve_catalog_contract(&self.registry, name)
    }
}

/// Typed result of asking a tool catalog to resolve a name it does not expose.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool `{name}` is not available in this catalog")]
pub struct ToolCatalogMiss {
    pub name: String,
}

pub(crate) fn resolve_catalog_contract(
    registry: &lash_core::ToolRegistry,
    name: &str,
) -> Result<Arc<lash_core::ToolContract>, ToolCatalogMiss> {
    lash_core::facade_support::resolve_tool_registry_contract(registry, name).ok_or_else(|| {
        ToolCatalogMiss {
            name: name.to_string(),
        }
    })
}

impl LashCore {
    /// Inspect the tool composition this core was built with.
    pub fn tool_catalog(&self) -> ToolCatalogView {
        ToolCatalogView {
            registry: Arc::clone(&self.tool_registry),
        }
    }
}
