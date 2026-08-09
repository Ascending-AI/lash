use lash_sansio::sync::RwLockExt;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::{
    PreparedToolCall, ToolCall, ToolContext, ToolContract, ToolExecutionGrant, ToolId,
    ToolManifest, ToolPrepareCall, ToolProvider, ToolResult,
};

#[cfg(test)]
use self::facade_ops::ToolRegistryFacadeOps;
use self::facade_ops::ToolStateFacadeOps;

include!("tool_registry/state.rs");
include!("tool_registry/sources.rs");
include!("tool_registry/registry_types.rs");
include!("tool_registry/registry_impl.rs");
include!("tool_registry/restore_execute.rs");
include!("tool_registry/rebind.rs");
include!("tool_registry/tests.rs");

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
                let mut state = self.state.write_recover();
                state.next_live_source_id += 1;
                format!("live:{}", state.next_live_source_id)
            };
            self.upsert_source(Arc::new(ToolProviderSource::new(
                source_id.clone(),
                provider,
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
