//! A process's step state (FIG-5268), owned by the components its
//! environment names (FIG-6034; final-spec §5.1).
//!
//! The process has no plugin session: its namespaces are keyed by component
//! id, reduced by the components' own reducers and admitted against their
//! declared formats. An activation reconstructs their committed step
//! publications over the last recorded format postimage before binding any
//! component. Its lifecycle deduplicates those original receipts and publishes
//! later outcomes before another step reads them.

use super::publication::{StateAuthority, StateDesk};
use super::*;
use crate::plugin::ProcessComposition;
use lash_durable::domain::TurnNamespaceWrite;
use lash_kernel_value::providers::FormatVersion;

/// The state sink of one process activation's step tools.
pub struct ComponentState {
    state: Arc<Mutex<PluginStateRegistry>>,
    owner: lash_vocabulary::identity::RuntimeOwner,
    reducers: BTreeMap<String, BTreeMap<String, StateReducer>>,
    formats: BTreeMap<String, FormatVersion>,
    evidence: lash_core_store::execution_limits::EvidenceLimits,
    data_limits: crate::runtime::RuntimeDataLimits,
}

impl ComponentState {
    /// The recorded state of `process`, reduced and admitted by `composition`'s
    /// components, which its components' views read, under the deployment's
    /// `data_limits`.
    #[must_use]
    pub fn new(
        process: lash_vocabulary::identity::ProcessId,
        composition: &ProcessComposition,
        evidence: lash_core_store::execution_limits::EvidenceLimits,
        diagnostics: lash_core_store::execution_limits::DiagnosticLimits,
        data_limits: crate::runtime::RuntimeDataLimits,
    ) -> Self {
        composition.state.lock_recover().diagnostics = diagnostics;
        Self {
            state: Arc::clone(&composition.state),
            owner: lash_vocabulary::identity::RuntimeOwner::Process(process),
            reducers: composition.reducers.clone(),
            formats: composition.formats.clone(),
            evidence,
            data_limits,
        }
    }

    /// Reconstruct recorded publications without constructing a component.
    #[doc(hidden)]
    pub fn replay_snapshot(
        state: PluginState,
        resolutions: &[StateResolution],
    ) -> Result<PluginState, PluginStateError> {
        super::publication::replay_state(state, resolutions)
    }

    fn desk(&self) -> StateDesk<'_> {
        StateDesk {
            state: &self.state,
            owner: &self.owner,
            authority: self,
            limits: self.data_limits.plugin_state,
        }
    }
}

impl StateAuthority for ComponentState {
    /// The single declared format of the component's state and config.
    fn declared(&self, plugin: &str) -> Option<FormatVersion> {
        self.formats.get(plugin).copied()
    }

    fn reducer(&self, plugin: &str, name: &str) -> Option<StateReducer> {
        self.reducers
            .get(plugin)
            .and_then(|reducers| reducers.get(name))
            .cloned()
    }
}

#[async_trait::async_trait]
impl crate::plugin::ExecutionState for ComponentState {
    fn propose(
        &self,
        proposal: Proposal,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        propose(self, proposal).map_err(lash_execution_contracts::plugin_error::PluginError::from)
    }
    fn propose_all(
        &self,
        proposals: Vec<Proposal>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        propose_all(self, proposals)
            .map_err(lash_execution_contracts::plugin_error::PluginError::from)
    }
    fn reserve_committed_state(
        &self,
        state: &[StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.desk().reserve_committed_state(state)
    }
    fn publish_committed_state(
        &self,
        state: &[StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.desk().publish_committed_state(state)
    }
    /// A process runs no durable run: its state is its step outcomes'.
    fn run_changes(&self) -> Vec<TurnNamespaceWrite> {
        Vec::new()
    }
    fn run_changes_committed(&self, _written: &[TurnNamespaceWrite]) {}
    fn owner_state(&self, owner: &str) -> PluginNamespaceState {
        self.state
            .lock_recover()
            .data
            .plugins
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }
    fn committed_namespaces(
        &self,
    ) -> BTreeMap<String, crate::plugin::prompt::CommittedPluginNamespace> {
        self.state
            .lock_recover()
            .data
            .plugins
            .iter()
            .map(|(plugin, namespace)| {
                (
                    plugin.clone(),
                    crate::plugin::prompt::CommittedPluginNamespace::new(
                        namespace.generation,
                        namespace.values.clone(),
                    ),
                )
            })
            .collect()
    }
    fn owner(&self) -> &lash_vocabulary::identity::RuntimeOwner {
        &self.owner
    }
    fn evidence_limits(&self) -> lash_core_store::execution_limits::EvidenceLimits {
        self.evidence
    }
    fn data_limits(&self) -> crate::runtime::RuntimeDataLimits {
        self.data_limits
    }
    fn proposal_key(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }
    fn state_segment(&self) -> crate::tool_run::SegmentOrdinal {
        self.desk().state_segment()
    }
    async fn reduce_proposals(
        &self,
        address: &crate::EffectAddress,
        segment: crate::tool_run::SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<StateResolution>, PluginStateError> {
        self.desk()
            .reduce_proposals(address, segment, proposals)
            .await
    }
    fn release_publication(&self, address: &crate::EffectAddress) {
        self.desk().release_publication(address);
    }
    fn abandon_publication(&self, address: &crate::EffectAddress) {
        self.desk().abandon_publication(address);
    }
    fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: crate::RuntimeEffectOutcome,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        self.desk().publish_recorded(address, outcome)
    }
}
