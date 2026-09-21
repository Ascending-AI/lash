use super::*;
pub use lash_core_store::tool_state::{ReconfigureError, ToolRegistrationKind};

#[derive(Clone, PartialEq)]
pub(super) struct ToolRegistryEntry {
    pub(super) manifest: ToolManifest,
    pub(super) binding: ToolBinding,
    pub(super) kind: ToolRegistrationKind,
    /// ToolId-keyed host curation intent. Authority policy is applied only to
    /// a pinned model-request surface and is never written back here.
    pub(super) member: bool,
}

impl ToolRegistryEntry {
    pub(super) fn new(
        manifest: ToolManifest,
        source_key: ToolSourceKey,
        kind: ToolRegistrationKind,
    ) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Bound { source_key },
            kind,
            member: true,
        }
    }

    pub(super) fn orphaned(
        manifest: ToolManifest,
        kind: ToolRegistrationKind,
        member: bool,
    ) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Orphaned,
            kind,
            member,
        }
    }

    pub(super) fn is_orphaned(&self) -> bool {
        self.binding == ToolBinding::Orphaned
    }

    pub(super) fn is_member(&self) -> bool {
        self.member && !self.is_orphaned()
    }

    pub(super) fn registration_kind(&self) -> ToolRegistrationKind {
        self.kind
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
            registration_kind: self.kind,
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

/// Typed registry-source identity. Leaf source labels and orchestrating tool
/// identities occupy disjoint namespaces even when their rendered text is
/// identical.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ToolSourceKey {
    Leaf(String),
    Internal(ToolId),
    Orchestrating(ToolId),
}

impl std::fmt::Display for ToolSourceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Leaf(source_id) => formatter.write_str(source_id),
            Self::Internal(tool_id) => write!(formatter, "internal:{tool_id}"),
            Self::Orchestrating(tool_id) => write!(formatter, "orchestrating:{tool_id}"),
        }
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

/// A persisted tool identity that a live id has replaced by owning its
/// model-facing name. The old grant is not transferred: the live id is a
/// default member and the retired id is dropped from the surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupersededToolIdentity {
    /// The persisted tool id no source resolves any more.
    pub retired_id: ToolId,
    /// The live tool id that now owns the model-facing name.
    pub live_id: ToolId,
    /// The model-facing name both identities carry.
    pub name: String,
}

/// Outcome of `ToolRegistry::restore_state`: the adopted generation plus what
/// happened to each persisted tool id no registered source resolved.
///
/// The three classes are different facts about the session, and only the first
/// is capability loss:
///
/// * [`lost_members`](Self::lost_members) — persisted `member: true`, nothing
///   resolves the id. The session opened without a tool the host had curated
///   in. This is what a host surfaces to its user and what
///   [`ToolSourcePolicy::Require`] refuses on.
/// * [`parked_opt_outs`](Self::parked_opt_outs) — unresolved ids the host had
///   already opted out of (`member: false`). Nothing the session could use is
///   missing; the entry is kept so the opt-out survives the source's return.
/// * [`superseded_identities`](Self::superseded_identities) — an old id dropped
///   because a live id owns its model-facing name. The capability is present
///   under a new identity, which is a default member.
///
/// Entries in the first two classes remain in tool state as orphans and rebind
/// automatically when their source returns.
#[derive(Clone, Debug, Default)]
pub struct ToolRestoreReport {
    pub generation: u64,
    pub lost_members: Vec<ToolId>,
    pub parked_opt_outs: Vec<ToolId>,
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

/// Host policy for **opening** a session whose persisted tools no live source
/// resolves.
///
/// The default is [`Tolerate`](Self::Tolerate): locking a user out of a
/// conversation is worse than degrading it, so a lost tool is a typed fact the
/// host receives rather than a refusal. Unattended and fixed-tool deployments
/// opt into [`Require`](Self::Require).
///
/// It governs opening only. Opening a session is the host's claim that it can
/// run that session, so a refusal there costs nothing: the half-built runtime
/// is discarded whole. Installing persisted tool state onto a runtime the host
/// already holds — an explicit `restore_tool_state`, a persisted-state install,
/// the resident re-sync after an invalidation — always tolerates and reports,
/// whatever this policy says. Those installs reconcile the live registry before
/// anything could refuse, so a refusal would leave the session with a changed
/// registry, a stale tool catalog and no report, which is worse than the
/// degraded session `Require` exists to prevent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSourcePolicy {
    #[default]
    Tolerate,
    /// Open refuses when the report has lost members. Parked opt-outs and
    /// superseded identities never refuse: neither is a missing capability.
    Require,
}

/// What a session **open** does with the persisted tool surface (FIG-3353).
///
/// The default is [`Reconcile`](Self::Reconcile): the open installs the
/// persisted `ToolState` through `install_persisted_tool_state`, reconciling
/// every persisted id against live sources — unresolved ids orphan, the
/// catalog generation bumps when the surface changed, and a report classifies
/// the loss.
///
/// [`PreservePersisted`](Self::PreservePersisted) is the host's declaration
/// that this open will not run a turn — an enqueue-only or read-only open on a
/// core that may not carry the session's tool sources at all. The open skips
/// the reconcile entirely: the persisted snapshot is not installed, the
/// catalog is not rebuilt, no generation bumps, no report is produced and no
/// loss warning fires (the intentional mode is silent, not merely quieter).
/// The runtime keeps the loaded snapshot on its state instead of restamping it
/// from the live registry, so a commit the open takes — a host append, a
/// config write — carries the persisted surface forward untouched rather than
/// recording every tool as orphaned or dropping it.
///
/// The declaration is enforced, not advisory: every turn-execution entry —
/// direct turns, queued and prepared drives, and the shared logical-turn
/// funnel — refuses a `PreservePersisted` open with
/// `RuntimeErrorCode::TurnExecutionRequiresReconciledToolSurface` before
/// admission, because the surface was never reconciled and no
/// [`ToolSourcePolicy`] was enforced. To run a turn the host must reopen in
/// [`Reconcile`](Self::Reconcile) mode, which performs the restore, applies
/// the source policy and rebuilds the catalog. Whole-state replacements
/// (resident reload, append-receipt replay, rollback) reassert the
/// per-open preservation marker from the runtime's open configuration so the
/// durable snapshot cannot be restamped from the unreconciled registry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolSurfaceOpenMode {
    /// Install the persisted snapshot and rebuild the catalog, reporting any
    /// lost members under the host's [`ToolSourcePolicy`].
    #[default]
    Reconcile,
    /// Leave the persisted snapshot untouched: no reconcile, no catalog
    /// rebuild, no report, no warning, no restamp at commit time — and no
    /// turn execution.
    PreservePersisted,
}

#[derive(Clone)]
pub struct ToolRegistry {
    /// The source map and admitted surface share one lock so readers cannot
    /// observe a source/surface half-commit.
    pub(super) inner: Arc<RwLock<ToolRegistryInner>>,
}
