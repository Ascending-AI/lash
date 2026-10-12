//! A process's step state (FIG-5268), owned by the components its
//! environment names (FIG-6034; final-spec §5.1).
//!
//! The process has no plugin session: its namespaces are keyed by component
//! id, reduced by the components' own reducers and admitted against their
//! declared formats. They live only in committed step outcomes: an
//! activation builds an empty state and its lifecycle republishes every
//! committed step outcome's resolutions into it before any step runs.

use super::publication::{StateAuthority, StateDesk};
use super::*;
use crate::plugin::{FormatVersion, ProcessComposition};
use lash_durable::domain::TurnNamespaceWrite;

/// The state sink of one process activation's step tools.
pub struct ComponentState {
    state: Arc<Mutex<PluginStateRegistry>>,
    owner: crate::RuntimeOwner,
    reducers: BTreeMap<String, BTreeMap<String, StateReducer>>,
    owners: std::collections::BTreeSet<String>,
    trace_limits: lash_trace::TraceLimits,
    data_limits: crate::runtime::RuntimeDataLimits,
}

impl ComponentState {
    /// The empty state of `process`, reduced and admitted by `composition`'s
    /// components, which its components' views read, under the deployment's
    /// `data_limits`.
    #[must_use]
    pub fn new(
        process: crate::ProcessId,
        composition: &ProcessComposition,
        trace_limits: lash_trace::TraceLimits,
        data_limits: crate::runtime::RuntimeDataLimits,
    ) -> Self {
        composition.state.lock_recover().trace_limits = trace_limits;
        Self {
            state: Arc::clone(&composition.state),
            owner: crate::RuntimeOwner::Process(process),
            reducers: composition.reducers.clone(),
            owners: composition.owners.iter().cloned().collect(),
            trace_limits,
            data_limits,
        }
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
    /// A component's step state is at the first format: it is never stored
    /// outside the step outcomes that carry it.
    fn declared(&self, plugin: &str) -> Option<FormatVersion> {
        self.owners.contains(plugin).then_some(FormatVersion::ONE)
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
    fn propose(&self, proposal: Proposal) -> Result<(), crate::PluginError> {
        propose(self, proposal).map_err(crate::PluginError::from)
    }
    fn propose_all(&self, proposals: Vec<Proposal>) -> Result<(), crate::PluginError> {
        propose_all(self, proposals).map_err(crate::PluginError::from)
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
    fn owner(&self) -> &crate::RuntimeOwner {
        &self.owner
    }
    fn trace_limits(&self) -> lash_trace::TraceLimits {
        self.trace_limits
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
