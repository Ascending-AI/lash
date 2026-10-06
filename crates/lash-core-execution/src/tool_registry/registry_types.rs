use super::*;
pub use lash_core_store::tool_state::{
    ReconfigureError, SupersededToolIdentity, ToolRestoreReport,
};

#[derive(Clone, PartialEq)]
pub(super) struct ToolRegistryEntry {
    pub(super) manifest: ToolManifest,
    pub(super) binding: ToolBinding,
    /// ToolId-keyed host curation intent. Authority policy is applied only to
    /// a pinned model-request surface and is never written back here.
    pub(super) member: bool,
}

impl ToolRegistryEntry {
    pub(super) fn new(manifest: ToolManifest, source_key: ToolSourceKey) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Bound { source_key },
            member: true,
        }
    }

    pub(super) fn orphaned(manifest: ToolManifest, member: bool) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Orphaned,
            member,
        }
    }

    pub(super) fn is_orphaned(&self) -> bool {
        self.binding == ToolBinding::Orphaned
    }

    pub(super) fn is_member(&self) -> bool {
        self.member && !self.is_orphaned()
    }

    /// The manifest as exposed to surfaces and catalogs. The view carries no
    /// curation or authority flags; callers derive effective membership.
    pub(super) fn view_manifest(&self) -> ToolManifest {
        self.manifest.clone()
    }

    pub(super) fn export(&self) -> ToolStateEntry {
        ToolStateEntry {
            manifest: self.manifest.clone(),
            orphaned: self.is_orphaned(),
            member: self.member,
        }
    }
}

#[derive(Clone, Default, PartialEq)]
pub(super) struct ToolSurface {
    pub(super) by_id: BTreeMap<ToolId, ToolRegistryEntry>,
    pub(super) by_name: BTreeMap<String, ToolId>,
}

#[derive(Debug)]
pub(super) enum ToolSurfaceInsertError {
    DuplicateId,
    DuplicateName { name: String },
}

impl ToolSurface {
    pub(super) fn insert(
        &mut self,
        entry: ToolRegistryEntry,
    ) -> Result<(), ToolSurfaceInsertError> {
        let id = entry.manifest.id.clone();
        let name = entry.manifest.name.clone();
        match (self.by_id.contains_key(&id), self.by_name.get(&name)) {
            (true, _) => Err(ToolSurfaceInsertError::DuplicateId),
            (false, Some(_)) => Err(ToolSurfaceInsertError::DuplicateName { name }),
            (false, None) => {
                let previous_name = self.by_name.insert(name, id.clone());
                let previous_entry = self.by_id.insert(id, entry);
                debug_assert!(previous_name.is_none());
                debug_assert!(previous_entry.is_none());
                Ok(())
            }
        }
    }

    pub(super) fn remove(&mut self, id: &ToolId) -> Option<ToolRegistryEntry> {
        let entry = self.by_id.remove(id)?;
        let removed_name = self.by_name.remove(&entry.manifest.name);
        debug_assert_eq!(removed_name.as_ref(), Some(id));
        Some(entry)
    }

    pub(super) fn get(&self, id: &ToolId) -> Option<&ToolRegistryEntry> {
        self.by_id.get(id)
    }

    pub(super) fn get_mut(&mut self, id: &ToolId) -> Option<&mut ToolRegistryEntry> {
        self.by_id.get_mut(id)
    }

    pub(super) fn get_by_name(&self, name: &str) -> Option<(&ToolId, &ToolRegistryEntry)> {
        let id = self.by_name.get(name)?;
        self.by_id.get(id).map(|entry| (id, entry))
    }

    pub(super) fn debug_assert_invariant(&self) {
        debug_assert_eq!(self.by_id.len(), self.by_name.len());
        for (id, entry) in &self.by_id {
            debug_assert_eq!(self.by_name.get(&entry.manifest.name), Some(id));
        }
        for (name, id) in &self.by_name {
            debug_assert_eq!(
                self.by_id.get(id).map(|entry| &entry.manifest.name),
                Some(name)
            );
        }
    }
}

/// Typed registry-source identity: the label of the source that advertises
/// a tool.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ToolSourceKey(String);

impl ToolSourceKey {
    pub(crate) fn new(source_id: impl Into<String>) -> Self {
        Self(source_id.into())
    }
}

impl std::fmt::Display for ToolSourceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone)]
pub(super) struct ToolRegistryState {
    pub(super) generation: u64,
    /// The admitted surface behind an `Arc`: every mutation installs a
    /// freshly reconciled surface rather than editing in place, so cloning
    /// the state — the per-pin snapshot and the optimistic-CAS retry copy —
    /// is a refcount bump instead of a deep copy of every manifest.
    pub(super) surface: Arc<ToolSurface>,
    pub(super) next_live_source_id: u64,
}

#[derive(Clone)]
pub(super) struct ToolRegistryInner {
    /// The optimistic-CAS fence every retry loop observes. [`Self::commit`]
    /// is its only bump site, so "every write moves the fence" is structural.
    /// `state.generation` is a different counter — the public admitted-surface
    /// identity exported in `ToolState` — and is not part of this fence.
    pub(super) write_revision: u64,
    pub(super) sources: BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>,
    /// Original live sources retained only by a pinned registry for explicit
    /// replay-grant routing. Resident dispatch uses `sources` exclusively.
    pub(super) granted_sources: Option<BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>>,
    pub(super) state: ToolRegistryState,
}

fn checked_write_revision(write_revision: u64) -> Result<u64, ReconfigureError> {
    write_revision.checked_add(1).ok_or_else(|| {
        ReconfigureError::Validation("tool registry write revision overflow".to_string())
    })
}

impl ToolRegistryInner {
    /// Advance the write fence exactly once. Every mutator routes its write
    /// through here, before mutating under the write guard, so an overflow
    /// refusal leaves the registry unmodified.
    pub(super) fn commit(&mut self) -> Result<(), ReconfigureError> {
        self.write_revision = checked_write_revision(self.write_revision)?;
        Ok(())
    }
}

/// Host policy for **running** a session whose persisted tools no live source
/// resolves.
///
/// The default is [`Tolerate`](Self::Tolerate): locking a user out of a
/// conversation is worse than degrading it, so a lost tool is a typed fact the
/// host receives on the run that restored it rather than a refusal.
/// Unattended and fixed-tool deployments opt into [`Require`](Self::Require).
///
/// Opening a session builds no capabilities (FIG-4857), so an open never
/// consults this policy. A turn run's recorded plugin transition does: under
/// `Require` it refuses the run before the transition publishes anything, and
/// the sender reads the refusal as the run's terminal answer (FIG-5134).
/// Installing persisted tool state onto a session that already holds
/// capabilities — a host restore command, a persisted-state install, the
/// resident re-sync after an invalidation — always tolerates and reports,
/// whatever this policy says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSourcePolicy {
    #[default]
    Tolerate,
    /// A turn run refuses at its plugin transition when restoring the
    /// session's persisted tool state would lose a member. Parked opt-outs
    /// and superseded identities never refuse: neither is a missing
    /// capability.
    Require,
}

#[derive(Clone)]
pub struct ToolRegistry {
    /// The source map and admitted surface share one lock so readers cannot
    /// observe a source/surface half-commit.
    pub(super) inner: Arc<RwLock<ToolRegistryInner>>,
}
