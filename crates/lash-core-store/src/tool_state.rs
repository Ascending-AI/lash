//! Durable tool-registry state carried in a session checkpoint.

use crate::{ToolId, ToolManifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolStateEntry {
    pub manifest: ToolManifest,
    /// Orphaned entries keep their last-known manifest, are excluded from the Tool Catalog
    /// (non-members until their source returns), and rebind automatically when a source
    /// re-advertises the same tool id.
    ///
    /// Required in every serialized entry: a pre-cutover snapshot that omits
    /// the flag fails to decode rather than being reconstructed as bound.
    pub orphaned: bool,
    /// ToolId-keyed host curation intent. Authority exclusions are transient
    /// policy and never change this bit. Hosts toggle it via
    /// `set_tool_membership`.
    #[serde(
        default = "is_member_default",
        skip_serializing_if = "is_default_member"
    )]
    pub member: bool,
    /// Persisted registration-lane hint. Required in every serialized entry:
    /// a pre-cutover snapshot that omits it fails to decode rather than being
    /// reconstructed as a leaf registration. On rebind the live source is
    /// authoritative and re-derives the effective lane.
    pub registration_kind: ToolRegistrationKind,
}
impl ToolStateEntry {
    pub fn new(manifest: ToolManifest) -> Self {
        Self {
            manifest,
            orphaned: false,
            member: true,
            registration_kind: ToolRegistrationKind::Leaf,
        }
    }

    /// The stored manifest as exposed to callers.
    pub fn manifest(&self) -> ToolManifest {
        self.manifest.clone()
    }

    pub fn stored_manifest(&self) -> &ToolManifest {
        &self.manifest
    }

    pub fn is_orphaned(&self) -> bool {
        self.orphaned
    }

    /// Orphaned entries are never effective members even when their retained curation bit is
    /// true.
    pub fn is_member(&self) -> bool {
        self.member && !self.orphaned
    }
}
#[derive(Clone, Debug, Default)]
pub struct ToolState {
    pub generation: u64,
    pub(super) tools: Arc<BTreeMap<ToolId, ToolStateEntry>>,
}
impl ToolState {
    pub fn new(generation: u64, tools: BTreeMap<ToolId, ToolStateEntry>) -> Self {
        Self {
            generation,
            tools: Arc::new(tools),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Edit a manifest in an explicit `ToolRegistry::apply_state` delta.
    ///
    /// Automatic rebuilds replace stored manifests with their live versions,
    /// so this is not a persistent source-curation mechanism.
    pub fn manifest_mut(&mut self, id: &ToolId) -> Option<&mut ToolManifest> {
        Arc::make_mut(&mut self.tools)
            .get_mut(id)
            .map(|entry| &mut entry.manifest)
    }

    /// Lets store and durable-substrate implementors test whether this `ToolState` is empty while
    /// snapshotting or restoring durable session state.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Reports the number of entries to store and durable-substrate implementors while snapshotting
    /// or restoring durable session state.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Iterates over the entries in the order consumed by store and durable-substrate implementors
    /// while snapshotting or restoring durable session state.
    pub fn iter(&self) -> impl Iterator<Item = (&ToolId, &ToolStateEntry)> {
        self.tools.iter()
    }

    /// Deletion intentionally removes the entry for that delta only. Use
    /// [`Self::set_membership`] for curation that must survive a rebuild from
    /// live sources.
    pub fn remove(&mut self, id: &ToolId) -> Option<ToolStateEntry> {
        Arc::make_mut(&mut self.tools).remove(id)
    }

    pub fn entries(&self) -> &BTreeMap<ToolId, ToolStateEntry> {
        self.tools.as_ref()
    }
}
impl Serialize for ToolState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct ToolStateRef<'a> {
            generation: u64,
            tools: &'a BTreeMap<ToolId, ToolStateEntry>,
        }

        ToolStateRef {
            generation: self.generation,
            tools: self.tools.as_ref(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for ToolState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ToolStateOwned {
            generation: u64,
            tools: BTreeMap<ToolId, ToolStateEntry>,
        }

        let owned = ToolStateOwned::deserialize(deserializer)?;
        Ok(Self {
            generation: owned.generation,
            tools: Arc::new(owned.tools),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRegistrationKind {
    #[default]
    Leaf,
    Orchestrating,
}

fn is_member_default() -> bool {
    true
}
fn is_default_member(member: &bool) -> bool {
    *member
}

pub mod facade_ops {
    use super::*;

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

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReconfigureError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error(
        "tool id `{tool_id}` is registered in both the leaf and orchestrating lanes (leaf source `{leaf_source_id}`)"
    )]
    CrossLaneToolIdCollision {
        tool_id: ToolId,
        leaf_source_id: String,
    },
    #[error("unknown tool source: {0}")]
    UnknownSource(String),
    #[error("generation mismatch: expected {expected}, actual {actual}")]
    GenerationMismatch { expected: u64, actual: u64 },
}

/// Generation injection for backend checkpoint round-trip fixtures.
///
/// `ToolState::with_generation` stays crate-private so the facade's sealed-surface
/// contract holds: a host cannot forge a tool-state generation. Certification
/// fixtures reach it through this trait, which `lash-core` re-exports at
/// `crate::testing::conformance_support::ToolStateConformanceAccess`.
#[cfg(any(test, feature = "testing"))]
pub trait ToolStateConformanceAccess {
    fn with_generation_for_conformance(self, generation: u64) -> Self;
}

#[cfg(any(test, feature = "testing"))]
impl ToolStateConformanceAccess for ToolState {
    fn with_generation_for_conformance(self, generation: u64) -> Self {
        ToolState::with_generation(self, generation)
    }
}
