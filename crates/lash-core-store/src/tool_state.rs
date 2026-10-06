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
}
impl ToolStateEntry {
    pub fn new(manifest: ToolManifest) -> Self {
        Self {
            manifest,
            orphaned: false,
            member: true,
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
#[derive(Clone, Debug, Default, PartialEq)]
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

/// A persisted tool identity that a live id has replaced by owning its
/// model-facing name. The old grant is not transferred: the live id is a
/// default member and the retired id is dropped from the surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SupersededToolIdentity {
    /// The persisted tool id no source resolves any more.
    pub retired_id: ToolId,
    /// The live tool id that now owns the model-facing name.
    pub live_id: ToolId,
    /// The model-facing name both identities carry.
    pub name: String,
}

/// Outcome of restoring a persisted [`ToolState`] over live sources: the
/// adopted generation plus what happened to each persisted tool id no
/// registered source resolved.
///
/// The three classes are different facts about the session, and only the first
/// is capability loss:
///
/// * [`lost_members`](Self::lost_members) — persisted `member: true`, nothing
///   resolves the id. The session runs without a tool the host had curated
///   in. This is what a host surfaces to its user and what
///   `ToolSourcePolicy::Require` refuses a run on.
/// * [`parked_opt_outs`](Self::parked_opt_outs) — unresolved ids the host had
///   already opted out of (`member: false`). Nothing the session could use is
///   missing; the entry is kept so the opt-out survives the source's return.
/// * [`superseded_identities`](Self::superseded_identities) — an old id dropped
///   because a live id owns its model-facing name. The capability is present
///   under a new identity, which is a default member.
///
/// Entries in the first two classes remain in tool state as orphans and rebind
/// automatically when their source returns.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolRestoreReport {
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_members: Vec<ToolId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parked_opt_outs: Vec<ToolId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub superseded_identities: Vec<SupersededToolIdentity>,
}

impl ToolRestoreReport {
    pub fn has_lost_members(&self) -> bool {
        !self.lost_members.is_empty()
    }

    pub fn is_clean(&self) -> bool {
        self.lost_members.is_empty()
            && self.parked_opt_outs.is_empty()
            && self.superseded_identities.is_empty()
    }

    /// The ids retained as orphaned entries: lost members and parked opt-outs.
    /// A superseded identity is not retained, so it is not listed here.
    pub fn orphaned_ids(&self) -> impl Iterator<Item = &ToolId> {
        self.lost_members.iter().chain(self.parked_opt_outs.iter())
    }
}

/// A host's change to a session's tool state, carried by a
/// [`SessionCommand::ChangeToolState`](crate::SessionCommand::ChangeToolState)
/// (FIG-5134). A session holds no tool registry until a run publishes its
/// plugin transition, so a change is a durable command the command run
/// applies, in lane order, against the capabilities that run built.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum ToolStateChange {
    /// Toggle Tool Catalog membership, all of `updates` or none.
    SetMembership { updates: Vec<ToolMembershipUpdate> },
    /// Replace the whole snapshot, guarded by its generation: the change
    /// applies only while the session's tool state is at
    /// `state.generation`.
    Apply {
        #[schemars(with = "serde_json::Value")]
        state: ToolState,
    },
    /// Restore a persisted snapshot over the live sources, adopting its
    /// generation. Unresolved ids are reported, never refused.
    Restore {
        #[schemars(with = "serde_json::Value")]
        state: ToolState,
    },
}

/// One membership toggle of a [`ToolStateChange::SetMembership`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolMembershipUpdate {
    pub tool_id: ToolId,
    pub member: bool,
}

/// How a [`ToolStateChange`] the command lane applied settled (FIG-5134).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolStateChangeOutcome {
    /// A membership change or snapshot apply landed at `generation`.
    Applied { generation: u64 },
    /// A restore landed; `report` classifies what no source resolved.
    Restored { report: ToolRestoreReport },
    /// The change did not apply against the session's tool state, and
    /// nothing of it committed.
    Refused { error: ReconfigureError },
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

    #[doc(hidden)]
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

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReconfigureError {
    #[error("validation error: {0}")]
    Validation(String),
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

#[doc(hidden)]
#[cfg(any(test, feature = "testing"))]
impl ToolStateConformanceAccess for ToolState {
    fn with_generation_for_conformance(self, generation: u64) -> Self {
        ToolState::with_generation(self, generation)
    }
}
