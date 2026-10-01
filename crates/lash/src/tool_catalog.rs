use std::sync::Arc;

/// Typed result of asking a session's tool catalog to resolve a name it does
/// not expose.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool `{name}` is not available in this catalog")]
pub struct ToolCatalogMiss {
    /// Name of the tool that was absent from the catalog.
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
