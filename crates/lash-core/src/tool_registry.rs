use lash_sansio::sync::RwLockExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::{
    PreparedToolCall, ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolPrepareCall,
    ToolProvider,
};

#[cfg(test)]
use self::facade_ops::ToolRegistryFacadeOps;
use self::facade_ops::ToolStateFacadeOps;

mod state;
pub use state::{PLUGIN_TOOL_SOURCE_ID, ToolSourceHandle, ToolState, ToolStateEntry};
pub(crate) use state::{ToolSourceCapture, ToolSourceExecutor};
mod sources;
use sources::{OrchestratingToolSource, ToolBinding, ToolProviderSource};
mod registry_types;
pub use registry_types::{ReconfigureError, ToolRegistry, ToolRestoreReport};
pub(crate) use registry_types::{ToolRegistrationKind, ToolSourceKey};
use registry_types::{
    ToolRegistryEntry, ToolRegistryInner, ToolRegistryState, ToolSurface, ToolSurfaceInsertError,
};
mod rebind;
mod registry_impl;
mod restore_execute;
#[cfg(test)]
use rebind::insert_result_entry;
use rebind::{
    ReconcileMode, export_tool_state_entries, insert_advertised_entry,
    manifest_with_compact_contract, reconcile_tool_state_entries, validate_unique_manifests,
};
#[cfg(test)]
mod pinning_tests;
#[cfg(test)]
mod tests;

/// Project every catalog member to a JSON record for host-owned discovery
/// (e.g. the production `tools.search` path in agent-workbench). The projection
/// ranges over members and emits no tiered state.
pub(crate) fn project_tool_catalog<I>(entries: I) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = crate::ToolCatalogEntry>,
{
    entries
        .into_iter()
        .map(|entry| {
            let manifest = entry.manifest;
            let compact_contract = entry.contract.compact_contract(&manifest);
            let mut projected = serde_json::json!({
                "id": manifest.id,
                "name": manifest.name,
                "description": manifest.description,
                "bindings": manifest.bindings,
                "activation": manifest.activation,
                "inline": manifest.inline,
            });
            projected
                .as_object_mut()
                .expect("projected tool catalog entry is an object")
                .insert("contract".to_string(), serde_json::json!(compact_contract));
            projected
        })
        .collect()
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`ToolRegistry`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ToolRegistryFacadeOps {
        fn add_tool_provider(
            &self,
            provider: Arc<dyn ToolProvider>,
        ) -> Result<ToolSourceHandle, ReconfigureError>;

        fn remove_source(&self, handle: &ToolSourceHandle) -> Result<u64, ReconfigureError>;
    }

    impl ToolRegistryFacadeOps for ToolRegistry {
        fn add_tool_provider(
            &self,
            provider: Arc<dyn ToolProvider>,
        ) -> Result<ToolSourceHandle, ReconfigureError> {
            let source_id = {
                let mut inner = self.inner.write_recover();
                let next_live_source_id = inner
                    .state
                    .next_live_source_id
                    .checked_add(1)
                    .ok_or_else(|| {
                        ReconfigureError::Validation("tool registry live source id overflow".into())
                    })?;
                let state_revision =
                    super::registry_impl::checked_state_revision(inner.state_revision)?;
                inner.state.next_live_source_id = next_live_source_id;
                inner.state_revision = state_revision;
                format!("live:{next_live_source_id}")
            };
            self.upsert_source(Arc::new(ToolProviderSource::new(
                source_id.clone(),
                vec![provider],
            )))?;
            Ok(ToolSourceHandle::new(source_id))
        }

        fn remove_source(&self, handle: &ToolSourceHandle) -> Result<u64, ReconfigureError> {
            self.remove_source_id(handle.id())
        }
    }

    /// Facade-internal operations for [`ToolState`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait ToolStateFacadeOps {
        fn generation(&self) -> u64;

        /// Manifests for current Tool Catalog members. Orphaned and host-removed
        /// entries are excluded (non-membership) but kept in state for rebind.
        fn tool_manifests(&self) -> Vec<ToolManifest>;

        fn get(&self, id: &ToolId) -> Option<&ToolStateEntry>;

        fn contains(&self, id: &ToolId) -> bool;

        /// Toggle Tool Catalog membership for a tool. `present == false` removes
        /// the tool from the catalog (non-membership) while keeping its state
        /// entry; `present == true` restores membership.
        fn set_membership(&mut self, id: &ToolId, present: bool) -> Result<(), ReconfigureError>;
    }

    impl ToolStateFacadeOps for ToolState {
        fn generation(&self) -> u64 {
            self.generation
        }

        fn tool_manifests(&self) -> Vec<ToolManifest> {
            self.tools
                .values()
                .filter(|entry| entry.is_member())
                .map(ToolStateEntry::manifest)
                .collect()
        }

        fn get(&self, id: &ToolId) -> Option<&ToolStateEntry> {
            self.tools.get(id)
        }

        fn contains(&self, id: &ToolId) -> bool {
            self.tools.contains_key(id)
        }

        fn set_membership(&mut self, id: &ToolId, present: bool) -> Result<(), ReconfigureError> {
            let Some(entry) = Arc::make_mut(&mut self.tools).get_mut(id) else {
                return Err(ReconfigureError::Validation(format!(
                    "unknown tool id `{id}`"
                )));
            };
            entry.member = present;
            Ok(())
        }
    }
}
