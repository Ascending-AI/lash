//! [`StaticToolProvider`] — a reusable [`ToolProvider`] for the common case of
//! serving a *fixed* set of [`ToolDefinition`]s.
//!
//! Almost every single- (or fixed-multi-) tool provider in the workspace used
//! to hand-roll the same idiom: `tool_manifests()` rebuilt `def.manifest()` and
//! `resolve_contract()` rebuilt `def.contract()` on *every* call, re-running
//! schema and doc generation each time. `StaticToolProvider` derives the
//! manifests and contracts **once** in its constructor and serves them from a
//! cache, delegating only `execute` (and, by default, the identity
//! `prepare_tool_call`) to a small [`StaticToolExecute`] implementation that
//! holds the tool's runtime state and behavior.

use std::collections::HashMap;
use std::sync::Arc;

use lash_core::{
    AttemptToolCall, ToolCall, ToolContract, ToolDefinition, ToolId, ToolManifest, ToolPrepareCall,
    ToolPrepareContext, ToolProvider, ToolResult, sansio::PendingToolCall,
};

/// Per-call execution behavior for a [`StaticToolProvider`].
///
/// Implement this on the struct that owns the tool's runtime state (HTTP
/// clients, shared mutable state, configuration flags, ...). The provider's
/// manifests and contracts come from the [`ToolDefinition`]s passed to
/// [`StaticToolProvider::new`]; this trait supplies only the dynamic behavior.
#[async_trait::async_trait]
pub trait StaticToolExecute: Send + Sync + 'static {
    /// Execute a resolved tool call. Dispatch on `call.name` when serving more
    /// than one tool.
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult;

    /// Opt one fixed tool id into the sealed leaf-attempt signature.
    fn supports_attempt_context(&self, _tool_id: &ToolId) -> bool {
        false
    }

    /// Execute an opted-in fixed tool as an atomic leaf attempt.
    async fn execute_attempt(&self, call: AttemptToolCall<'_>) -> lash_core::ToolAttemptResult {
        let result = ToolResult::err_fmt(format!(
            "leaf attempt execution is unavailable for tool `{}`",
            call.name
        ));
        match result {
            ToolResult::Done(output) => lash_core::ToolAttemptResult::done_without_intents(
                lash_core::ToolResultDone::from_output(*output),
            ),
            ToolResult::Pending(pending) => lash_core::ToolAttemptResult::pending(pending),
        }
    }

    /// Opt one fixed tool id into deterministic process-replay orchestration.
    #[doc(hidden)]
    fn supports_orchestration_context(&self, _tool_id: &ToolId) -> bool {
        false
    }

    /// Execute an opted-in fixed tool with the orchestration-only context.
    #[doc(hidden)]
    async fn execute_orchestration(
        &self,
        call: lash_core::facade_support::OrchestratingToolCall<'_>,
    ) -> ToolResult {
        ToolResult::err_fmt(format!(
            "orchestration execution is unavailable for tool `{}`",
            call.name
        ))
    }

    /// Optional argument-preparation hook, mirroring
    /// [`ToolProvider::prepare_tool_call`]. Defaults to the identity transform.
    async fn prepare_tool_call(
        &self,
        tool_id: &ToolId,
        pending: PendingToolCall,
        _context: &ToolPrepareContext,
    ) -> Result<lash_core::PreparedToolCall, ToolResult> {
        Ok(lash_core::PreparedToolCall::identity(
            tool_id.clone(),
            pending,
        ))
    }
}

/// A [`ToolProvider`] that serves a fixed set of [`ToolDefinition`]s from a
/// cache, delegating execution to an [`StaticToolExecute`].
pub struct StaticToolProvider<E: StaticToolExecute> {
    manifests: Vec<ToolManifest>,
    contracts: HashMap<String, Arc<ToolContract>>,
    contracts_by_id: HashMap<ToolId, Arc<ToolContract>>,
    executor: E,
}

impl<E: StaticToolExecute> StaticToolProvider<E> {
    /// Build a provider from a fixed set of definitions and an executor.
    ///
    /// Manifests and contracts are derived once, here, and reused for the life
    /// of the provider.
    pub fn new(definitions: Vec<ToolDefinition>, executor: E) -> Self {
        let mut manifests = Vec::with_capacity(definitions.len());
        let mut contracts = HashMap::with_capacity(definitions.len());
        let mut contracts_by_id = HashMap::with_capacity(definitions.len());
        for def in &definitions {
            let manifest = def.manifest();
            let contract = Arc::new(def.contract());
            contracts.insert(manifest.name.clone(), Arc::clone(&contract));
            contracts_by_id.insert(manifest.id.clone(), contract);
            manifests.push(manifest);
        }
        Self {
            manifests,
            contracts,
            contracts_by_id,
            executor,
        }
    }

    /// Borrow the underlying executor. Useful for tests that need to inspect
    /// the executor's internal state.
    pub fn executor(&self) -> &E {
        &self.executor
    }
}

#[async_trait::async_trait]
impl<E: StaticToolExecute> ToolProvider for StaticToolProvider<E> {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        self.manifests.clone()
    }

    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.manifests
            .iter()
            .find(|manifest| manifest.name == name)
            .cloned()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.manifests
            .iter()
            .find(|manifest| manifest.id == *id)
            .cloned()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.contracts.get(name).cloned()
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        self.contracts_by_id.get(id).cloned()
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolResult> {
        self.executor
            .prepare_tool_call(&call.tool_id, call.pending, call.context)
            .await
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        self.executor.execute(call).await
    }

    fn supports_attempt_context(&self, tool_id: &ToolId) -> bool {
        self.executor.supports_attempt_context(tool_id)
    }

    async fn execute_attempt(&self, call: AttemptToolCall<'_>) -> lash_core::ToolAttemptResult {
        self.executor.execute_attempt(call).await
    }

    fn supports_orchestration_context(&self, tool_id: &ToolId) -> bool {
        self.executor.supports_orchestration_context(tool_id)
    }

    async fn execute_orchestration(
        &self,
        call: lash_core::facade_support::OrchestratingToolCall<'_>,
    ) -> ToolResult {
        self.executor.execute_orchestration(call).await
    }
}
