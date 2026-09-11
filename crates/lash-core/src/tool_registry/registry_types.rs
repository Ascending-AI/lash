use super::*;

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

    pub(super) fn orphaned(manifest: ToolManifest, kind: ToolRegistrationKind) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Orphaned,
            kind,
            member: true,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolRegistrationKind {
    #[default]
    Leaf,
    Orchestrating,
}

/// Typed registry-source identity. Leaf source labels and orchestrating tool
/// identities occupy disjoint namespaces even when their rendered text is
/// identical.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ToolSourceKey {
    Leaf(String),
    Orchestrating(ToolId),
}

impl std::fmt::Display for ToolSourceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Leaf(source_id) => formatter.write_str(source_id),
            Self::Orchestrating(tool_id) => write!(formatter, "orchestrating:{tool_id}"),
        }
    }
}

#[derive(Clone)]
pub(super) struct ToolRegistryState {
    pub(super) generation: u64,
    pub(super) surface: ToolSurface,
    pub(super) next_live_source_id: u64,
}

#[derive(Clone)]
pub(super) struct ToolRegistryInner {
    /// Changes whenever the live source map changes, even when the admitted
    /// surface is byte-equivalent and its generation therefore stays stable.
    pub(super) source_revision: u64,
    /// Changes whenever private registry state changes, including restores
    /// that intentionally preserve or adopt the public generation.
    pub(super) state_revision: u64,
    pub(super) sources: BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>,
    /// Original live sources retained only by a pinned registry for explicit
    /// replay-grant routing. Resident dispatch uses `sources` exclusively.
    pub(super) granted_sources: Option<BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>>,
    pub(super) state: ToolRegistryState,
}

/// Outcome of `ToolRegistry::restore_state`: the adopted generation plus the
/// ids of persisted tools that no registered source currently resolves.
/// Hosts should surface a non-empty `orphaned` list to the user — the session
/// opened, but those tools are non-members until their source returns.
#[derive(Clone, Debug, Default)]
pub struct ToolRestoreReport {
    pub generation: u64,
    pub orphaned: Vec<ToolId>,
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

#[derive(Clone)]
pub struct ToolRegistry {
    /// The source map and admitted surface share one lock so readers cannot
    /// observe a source/surface half-commit.
    pub(super) inner: Arc<RwLock<ToolRegistryInner>>,
}
