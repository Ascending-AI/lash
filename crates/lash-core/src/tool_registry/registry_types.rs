#[derive(Clone, PartialEq)]
struct ToolRegistryEntry {
    manifest: ToolManifest,
    binding: ToolBinding,
    kind: ToolRegistrationKind,
    /// Tool Catalog membership. A member is callable; a non-member does not
    /// exist to the model. Orphaned entries are never members.
    member: bool,
}

impl ToolRegistryEntry {
    fn new(
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

    fn orphaned(manifest: ToolManifest, kind: ToolRegistrationKind) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Orphaned,
            kind,
            member: true,
        }
    }

    fn is_orphaned(&self) -> bool {
        self.binding == ToolBinding::Orphaned
    }

    fn is_member(&self) -> bool {
        self.member && !self.is_orphaned()
    }

    fn registration_kind(&self) -> ToolRegistrationKind {
        self.kind
    }

    /// The manifest as exposed to surfaces and catalogs. Membership is the
    /// execution gate, so the view is just the stored manifest; orphaned and
    /// host-removed entries are filtered out by the caller, not flagged here.
    fn view_manifest(&self) -> ToolManifest {
        self.manifest.clone()
    }

    fn export(&self) -> ToolStateEntry {
        ToolStateEntry {
            manifest: self.manifest.clone(),
            orphaned: self.is_orphaned(),
            member: self.member,
            registration_kind: self.kind,
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
struct ToolRegistryState {
    generation: u64,
    tools: BTreeMap<ToolId, ToolRegistryEntry>,
    next_live_source_id: u64,
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
    sources: Arc<RwLock<BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>>>,
    state: Arc<RwLock<ToolRegistryState>>,
    /// Authority exclusions are part of registry policy, not snapshot shape.
    /// Keeping them at this seam prevents a live-source rebuild from granting
    /// a hidden tool merely because its id was absent from the snapshot.
    hidden_tool_names: Arc<BTreeSet<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceReconcilePolicy {
    RejectExternalConflicts,
    OverlayReplacingConflicts,
}
