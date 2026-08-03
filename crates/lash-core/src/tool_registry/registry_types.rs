#[derive(Clone, PartialEq)]
struct ToolRegistryEntry {
    manifest: ToolManifest,
    binding: ToolBinding,
    /// Tool Catalog membership. A member is callable; a non-member does not
    /// exist to the model. Orphaned entries are never members.
    member: bool,
}

impl ToolRegistryEntry {
    fn new(manifest: ToolManifest, source_id: impl Into<String>) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Bound(source_id.into()),
            member: true,
        }
    }

    fn orphaned(manifest: ToolManifest) -> Self {
        Self {
            manifest,
            binding: ToolBinding::Orphaned,
            member: true,
        }
    }

    fn is_orphaned(&self) -> bool {
        self.binding == ToolBinding::Orphaned
    }

    fn is_member(&self) -> bool {
        self.member && !self.is_orphaned()
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
    #[error("unknown tool source: {0}")]
    UnknownSource(String),
    #[error("generation mismatch: expected {expected}, actual {actual}")]
    GenerationMismatch { expected: u64, actual: u64 },
}

#[derive(Clone)]
pub struct ToolRegistry {
    sources: Arc<RwLock<BTreeMap<String, Arc<dyn ToolSourceExecutor>>>>,
    state: Arc<RwLock<ToolRegistryState>>,
    /// Authority exclusions are part of registry policy, not snapshot shape.
    /// Keeping them at this seam prevents a live-source rebuild from granting
    /// a hidden tool merely because its id was absent from the snapshot.
    hidden_tool_names: Arc<BTreeSet<String>>,
}

pub(crate) mod tool_registry_facade_ops {
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
                let mut state = self
                    .state
                    .write()
                    .expect("tool registry state lock poisoned");
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
            self.remove_source_id(handle.as_str())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceReconcilePolicy {
    RejectExternalConflicts,
    OverlayReplacingConflicts,
}
