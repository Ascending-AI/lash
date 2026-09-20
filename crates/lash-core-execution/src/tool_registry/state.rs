use super::*;
pub use lash_core_store::tool_state::{ToolState, ToolStateEntry};

pub const PLUGIN_TOOL_SOURCE_ID: &str = "plugins";

/// Ephemeral identity for a live tool-provider source.
///
/// Session hosts use this handle to correlate provider bookkeeping and later
/// remove the source from the same open session. The identity is scoped to the
/// registry that issued it: it is not a durable cross-session identifier, and
/// another session may issue the same string for a different provider.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolSourceHandle {
    pub(super) id: String,
}

impl ToolSourceHandle {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    /// Return the source identity used by session hosts to correlate this
    /// provider with host-owned routing or lifecycle state.
    ///
    /// The value is stable for this handle but is valid only for the registry
    /// that issued it. Do not persist it across session rebuilds or use it with
    /// another session.
    pub fn id(&self) -> &str {
        &self.id
    }
}

pub(crate) trait ToolSourceCapture: Send + 'static {
    fn advertised_tools(&self) -> Vec<ToolManifest>;
    /// The ids in the captured advertisement. Implementors that already index
    /// by id override this to avoid materializing every manifest when callers
    /// need only the key set.
    fn advertised_ids(&self) -> BTreeSet<ToolId> {
        self.advertised_tools()
            .into_iter()
            .map(|manifest| manifest.id)
            .collect()
    }
    fn freeze(
        self: Box<Self>,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError>;
}

struct FrozenToolSourceCapture {
    source: Arc<dyn ToolSourceExecutor>,
}

impl ToolSourceCapture for FrozenToolSourceCapture {
    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.source.advertised_tools()
    }

    fn advertised_ids(&self) -> BTreeSet<ToolId> {
        self.source.advertised_ids()
    }

    fn freeze(
        self: Box<Self>,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        self.source.snapshot_execution_source(known_resident_ids)
    }
}

pub(crate) enum ToolSourceExecution<'a> {
    Leaf(&'a dyn LeafToolSourceExecutor),
    Internal(&'a crate::InternalProcessToolDef),
    Orchestrating(&'a crate::tool_provider::orchestration::OrchestratingToolDef),
}

#[async_trait::async_trait]
pub(crate) trait LeafToolSourceExecutor: Send + Sync {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome;
    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool;
}

#[async_trait::async_trait]
pub(crate) trait ToolSourceExecutor: Send + Sync + 'static {
    fn id(&self) -> &str;
    /// Capture this source's current advertisement and route inputs for a
    /// two-phase resident snapshot.
    fn capture_execution_source(&self) -> Result<Box<dyn ToolSourceCapture>, ReconfigureError> {
        let source = self.snapshot_execution_source(&BTreeSet::new())?;
        Ok(Box::new(FrozenToolSourceCapture { source }))
    }
    /// Freeze this source for resident execution, retaining any supplied known
    /// resident IDs that it can resolve. Two-phase callers use
    /// [`Self::capture_execution_source`] so advertisements are not reread.
    fn snapshot_execution_source(
        &self,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError>;
    fn source_key(&self) -> ToolSourceKey {
        ToolSourceKey::Leaf(self.id().to_string())
    }
    fn registration_kind(&self) -> ToolRegistrationKind {
        ToolRegistrationKind::Leaf
    }
    fn advertised_tools(&self) -> Vec<ToolManifest>;
    /// The ids this source advertises. Implementors that already index by id
    /// override this to avoid materializing every manifest when callers need
    /// only the key set.
    fn advertised_ids(&self) -> BTreeSet<ToolId> {
        self.advertised_tools()
            .into_iter()
            .map(|manifest| manifest.id)
            .collect()
    }
    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.advertised_tools()
            .into_iter()
            .find(|manifest| manifest.id == *id)
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>>;
    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id)?;
        self.resolve_contract(&manifest.name)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }
    /// The typed execution capability this source provides. Leaf sources
    /// expose a [`LeafToolSourceExecutor`]; internal and orchestrating sources
    /// expose only their typed definitions, never a leaf body.
    fn execution(&self) -> ToolSourceExecution<'_>;
}
