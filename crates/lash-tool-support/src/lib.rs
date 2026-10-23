//! Shared implementation support for Lash tools.

use lash_core::{ToolFailure, ToolOutcome};

mod selected_tools;
mod selection;
mod static_provider;
pub use selected_tools::{
    SelectedToolsPlugin, SetToolSelection, ToolSelectionConfig, ToolSelectionOwner,
};
pub use selection::ToolSelection;
pub use static_provider::{StaticToolExecute, StaticToolProvider};

pub fn invalid_tool_args(message: impl Into<String>) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::invalid_request("invalid_tool_args", message))
}

/// Accepts `null` or `"none"` when `allow_none` is true.
pub fn parse_optional_usize_arg(
    args: &serde_json::Value,
    key: &str,
    default: Option<usize>,
    allow_none: bool,
    min: usize,
) -> Result<Option<usize>, ToolOutcome> {
    match args.get(key) {
        None => Ok(default),
        Some(v) if v.is_null() => {
            if allow_none {
                Ok(None)
            } else {
                Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int >= {min}"
                )))
            }
        }
        Some(v) => {
            if let Some(s) = v.as_str() {
                if allow_none && s.eq_ignore_ascii_case("none") {
                    return Ok(None);
                }
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                )));
            }
            let n = v.as_u64().ok_or_else(|| {
                invalid_tool_args(format!(
                    "Invalid {key}: expected int{}",
                    if allow_none {
                        ", null, or \"none\""
                    } else {
                        ""
                    }
                ))
            })? as usize;
            if n < min {
                return Err(invalid_tool_args(format!(
                    "Invalid {key}: must be >= {min}{}",
                    if allow_none {
                        ", or use null/\"none\" for no cap"
                    } else {
                        ""
                    }
                )));
            }
            Ok(Some(n))
        }
    }
}

pub fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod certification;

/// Explicit package advertisement over a runner's definitions. Registering
/// this adapter supplies code operations; the host separately selects model tools.
pub struct ToolPackageProvider {
    owner: String,
    key: lash_core::plugin::ToolProviderKey,
    roots: std::collections::BTreeSet<lash_vocabulary::names::Segment>,
    provider: std::sync::Arc<dyn lash_core::ToolProvider>,
}
impl ToolPackageProvider {
    pub fn new(
        owner: impl Into<String>,
        key: lash_core::plugin::ToolProviderKey,
        roots: std::collections::BTreeSet<lash_vocabulary::names::Segment>,
        provider: std::sync::Arc<dyn lash_core::ToolProvider>,
    ) -> Self {
        Self {
            owner: owner.into(),
            key,
            roots,
            provider,
        }
    }
}
#[async_trait::async_trait]
impl lash_core::plugin::packages::PackageProvider for ToolPackageProvider {
    async fn hold(
        &self,
        _: &lash_core_store::artifact_referrer::ReferrerClaim,
        _: &[&lash_vocabulary::package::PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        Ok(())
    }

    fn roots(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> std::collections::BTreeSet<lash_vocabulary::names::Segment> {
        self.roots.clone()
    }
    fn advertise(
        &self,
        config: &lash_core::OwnerConfigView,
        context: &lash_vocabulary::package::PublicationContext,
    ) -> Result<
        Vec<lash_core::plugin::packages::PackageContribution>,
        lash_execution_contracts::PluginError,
    > {
        let mut definitions = Vec::new();
        for manifest in self.provider.tool_manifests(config)? {
            let contract = self
                .provider
                .resolve_contract_by_id(&manifest.id, config)
                .ok_or_else(|| {
                    lash_execution_contracts::PluginError::ResidentToolContractUnavailable {
                        tool_id: manifest.id.clone(),
                        operation: manifest.tool(),
                    }
                })?;
            definitions.push(lash_core::ToolDefinition {
                manifest,
                contract: (*contract).clone(),
            });
        }
        let handle = lash_core::plugin::ToolProviderHandle::new(
            config.registry_kind(),
            self.owner.clone(),
            self.key,
        );
        lash_core::plugin::packages::operation_packages(
            &handle,
            definitions,
            |definition| {
                self.provider
                    .link_payload(&definition.manifest, config)
                    .map(
                        |(format, payload)| lash_vocabulary::providers::ExecutionBinding {
                            codec: handle.clone(),
                            format,
                            payload,
                        },
                    )
            },
            context,
        )
    }
}
