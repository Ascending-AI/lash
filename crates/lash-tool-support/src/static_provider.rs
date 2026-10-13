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
    OwnerConfigView, ToolCall, ToolContract, ToolDefinition, ToolId, ToolManifest, ToolOutcome,
    ToolPrepareCall, ToolPrepareContext, ToolProvider, sansio::PendingToolCall,
};

use crate::ToolSelection;

/// Per-call execution behavior for a [`StaticToolProvider`].
///
/// The provider's manifests and contracts come from the [`ToolDefinition`]s passed to
/// [`StaticToolProvider::new`]; this trait supplies only the dynamic behavior.
/// What a tool may do beyond an inline Done result — defer, declare intents —
/// is declared on its definition with
/// [`ToolDefinition::with_declaration`](lash_core::ToolDefinition::with_declaration).
#[async_trait::async_trait]
pub trait StaticToolExecute: Send + Sync + 'static {
    /// Dispatch on `call.name()` when serving more than one tool.
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome;

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
/// cache, delegating execution to an [`StaticToolExecute`]. Selected by an
/// owner ([`Self::selected_by`]), it offers only the tools that owner's
/// [`ToolSelection`] admits under the run's config.
pub struct StaticToolProvider<E: StaticToolExecute> {
    manifests: Vec<ToolManifest>,
    contracts_by_id: HashMap<ToolId, Arc<ToolContract>>,
    selected_by: Option<String>,
    executor: E,
}

impl<E: StaticToolExecute> StaticToolProvider<E> {
    /// Manifests and contracts are derived once, here, and reused for the life
    /// of the provider.
    pub fn new(definitions: Vec<ToolDefinition>, executor: E) -> Self {
        let mut manifests = Vec::with_capacity(definitions.len());
        let mut contracts_by_id = HashMap::with_capacity(definitions.len());
        for def in &definitions {
            let manifest = def.manifest();
            let contract = Arc::new(def.contract());
            contracts_by_id.insert(manifest.id.clone(), contract);
            manifests.push(manifest);
        }
        Self {
            manifests,
            contracts_by_id,
            selected_by: None,
            executor,
        }
    }

    /// Offer only the tools the [`ToolSelection`] in `owner`'s config
    /// namespace admits, under each run's config.
    #[must_use]
    pub fn selected_by(mut self, owner: impl Into<String>) -> Self {
        self.selected_by = Some(owner.into());
        self
    }

    /// Whether this provider offers `tool` under `config`. A selection that
    /// does not decode offers nothing.
    fn offers(&self, tool: &ToolId, config: &OwnerConfigView) -> bool {
        self.selected_by.as_deref().is_none_or(|owner| {
            ToolSelection::of(config, owner).is_ok_and(|selection| selection.admits(tool))
        })
    }

    /// Borrow the underlying executor. Useful for tests that need to inspect
    /// the executor's internal state.
    pub fn executor(&self) -> &E {
        &self.executor
    }
}

#[async_trait::async_trait]
impl<E: StaticToolExecute> ToolProvider for StaticToolProvider<E> {
    fn tool_manifests(
        &self,
        config: &OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash_core::PluginError> {
        Ok({
            self.manifests
                .iter()
                .filter(|manifest| self.offers(&manifest.id, config))
                .cloned()
                .collect()
        })
    }

    fn resolve_manifest(&self, name: &str, config: &OwnerConfigView) -> Option<ToolManifest> {
        self.manifests
            .iter()
            .find(|manifest| manifest.name == name && self.offers(&manifest.id, config))
            .cloned()
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.manifests
            .iter()
            .find(|manifest| manifest.id == *id && self.offers(id, config))
            .cloned()
    }

    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.resolve_manifest(name, config)
            .and_then(|manifest| self.contracts_by_id.get(&manifest.id).cloned())
    }

    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.offers(id, config)
            .then(|| self.contracts_by_id.get(id).cloned())
            .flatten()
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        self.executor
            .prepare_tool_call(&call.tool_id, call.pending, call.context)
            .await
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executor.execute(call).await
    }
}
