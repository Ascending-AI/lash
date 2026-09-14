pub use lash_core_store::tool_state::{ToolState, ToolStateEntry};
use super::*;

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

    fn freeze(
        self: Box<Self>,
        known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        self.source.snapshot_execution_source(known_resident_ids)
    }
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
    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.advertised_tools()
            .into_iter()
            .find(|manifest| manifest.name == name)
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
    async fn execute(
        &self,
        tool: &str,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome;
    async fn execute_orchestrating(
        &self,
        _tool_id: &ToolId,
        _args: &serde_json::Value,
        _context: &crate::tool_provider::orchestration::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        ToolOutcome::err_fmt("leaf tools cannot execute in the orchestrating registration lane")
    }
    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::from_tool_result(
            self.execute_by_id(tool_id, args, context).await,
        )
    }
    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return ToolOutcome::err_fmt(format_args!("Unknown tool id: {tool_id}"));
        };
        self.execute(&manifest.name, args, context).await
    }
    async fn execute_internal_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &crate::InternalProcessContext<'_>,
    ) -> ToolOutcome {
        self.execute_by_id(tool_id, args, &context.__attempt_context())
            .await
    }
}
