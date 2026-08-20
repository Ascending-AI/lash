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
    InternalProcessToolCall, ToolCall, ToolContract, ToolDefinition, ToolId, ToolManifest,
    ToolOutcome, ToolPrepareCall, ToolPrepareContext, ToolProvider, sansio::PendingToolCall,
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
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome;

    /// Execute a tool resolved as an internal owner-bound process body.
    ///
    /// This is ADR 0051's protocol and process-engine implementor class.
    async fn execute_internal(&self, call: InternalProcessToolCall<'_>) -> ToolOutcome {
        let attempt_context = call.context.__attempt_context();
        self.execute(ToolCall {
            name: call.name,
            args: call.args,
            context: &attempt_context,
        })
        .await
    }

    /// Execute the fixed tool as a recorded leaf attempt that may declare
    /// typed intents. Defaults to the pure [`execute`](Self::execute) body,
    /// which receives the same sealed attempt context.
    async fn execute_attempt(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match self.execute(call).await {
            ToolOutcome::Done(output) => lash_core::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::from_output(*output),
            ),
            ToolOutcome::Pending(pending) => lash_core::ToolAttemptOutcome::pending(pending),
        }
    }

    /// Declare that a tool may return `Pending` from its recorded attempt.
    ///
    /// A recorded attempt reads its completion key from the sealed
    /// `AttemptContext`, and the runtime only pre-derives that key for a tool
    /// that declares it here. Defaults to `false`: a tool that parks without
    /// declaring it observes a typed refusal instead of a key.
    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        let _ = tool_id;
        false
    }

    /// Optional argument-preparation hook, mirroring
    /// [`ToolProvider::prepare_tool_call`]. Defaults to the identity transform.
    async fn prepare_tool_call(
        &self,
        tool_id: &ToolId,
        pending: PendingToolCall,
        _context: &ToolPrepareContext,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
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
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        self.executor
            .prepare_tool_call(&call.tool_id, call.pending, call.context)
            .await
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.executor.execute(call).await
    }

    async fn execute_internal(&self, call: InternalProcessToolCall<'_>) -> ToolOutcome {
        self.executor.execute_internal(call).await
    }

    async fn execute_attempt(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executor.execute_attempt(call).await
    }

    fn attempt_may_defer(&self, tool_id: &ToolId) -> bool {
        self.executor.attempt_may_defer(tool_id)
    }
}
