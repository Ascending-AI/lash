use std::sync::Arc;

use super::{PluginError, PluginNamespaceState, PluginSession, Proposal, StateResolution};
use lash_durable::domain::TurnNamespaceWrite;

/// The state sink of the owner executing a call. Publication remains tied
/// to the recorded outcome, including reservations held by staged changes.
#[async_trait::async_trait]
pub trait ExecutionState: Send + Sync {
    fn propose(&self, proposal: Proposal) -> Result<(), PluginError>;
    fn propose_all(&self, proposals: Vec<Proposal>) -> Result<(), PluginError>;
    fn publish_committed_state(
        &self,
        state: &[StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError>;
    fn run_changes(&self) -> Vec<TurnNamespaceWrite>;
    fn run_changes_committed(&self, written: &[TurnNamespaceWrite]);
    fn owner_state(&self, owner: &str) -> PluginNamespaceState;

    fn committed_namespaces(
        &self,
    ) -> std::collections::BTreeMap<String, super::prompt::CommittedPluginNamespace>;

    fn owner(&self) -> &crate::RuntimeOwner;

    fn trace_limits(&self) -> lash_trace::TraceLimits;

    /// Identity of the state sink scoped to a recorded body.
    fn proposal_key(&self) -> usize;
    fn state_segment(&self) -> crate::tool_run::SegmentOrdinal;
    async fn reduce_proposals(
        &self,
        address: &crate::EffectAddress,
        segment: crate::tool_run::SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<StateResolution>, super::PluginStateError>;
    fn release_publication(&self, address: &crate::EffectAddress);
    fn abandon_publication(&self, address: &crate::EffectAddress);
    fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: crate::RuntimeEffectOutcome,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError>;
}

#[async_trait::async_trait]
impl ExecutionState for PluginSession {
    fn propose(&self, proposal: Proposal) -> Result<(), PluginError> {
        super::propose(self, proposal).map_err(PluginError::from)
    }
    fn propose_all(&self, proposals: Vec<Proposal>) -> Result<(), PluginError> {
        super::propose_all(self, proposals).map_err(PluginError::from)
    }
    fn publish_committed_state(
        &self,
        state: &[StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        PluginSession::publish_committed_state(self, state)
    }
    fn run_changes(&self) -> Vec<TurnNamespaceWrite> {
        PluginSession::run_changes(self)
    }
    fn run_changes_committed(&self, written: &[TurnNamespaceWrite]) {
        PluginSession::run_changes_committed(self, written);
    }
    fn owner_state(&self, owner: &str) -> PluginNamespaceState {
        self.export_state()
            .plugins
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }
    fn committed_namespaces(
        &self,
    ) -> std::collections::BTreeMap<String, super::prompt::CommittedPluginNamespace> {
        PluginSession::committed_namespaces(self)
    }
    fn owner(&self) -> &crate::RuntimeOwner {
        PluginSession::owner(self)
    }
    fn trace_limits(&self) -> lash_trace::TraceLimits {
        self.host().trace_runtime().limits()
    }
    fn proposal_key(&self) -> usize {
        Arc::as_ptr(&self.state) as usize
    }
    fn state_segment(&self) -> crate::tool_run::SegmentOrdinal {
        PluginSession::state_segment(self)
    }
    async fn reduce_proposals(
        &self,
        address: &crate::EffectAddress,
        segment: crate::tool_run::SegmentOrdinal,
        proposals: Vec<Proposal>,
    ) -> Result<Vec<StateResolution>, super::PluginStateError> {
        PluginSession::reduce_proposals(self, address, segment, proposals).await
    }
    fn release_publication(&self, address: &crate::EffectAddress) {
        PluginSession::release_publication(self, address);
    }
    fn abandon_publication(&self, address: &crate::EffectAddress) {
        PluginSession::abandon_publication(self, address);
    }
    fn publish_recorded(
        &self,
        address: &crate::EffectAddress,
        outcome: crate::RuntimeEffectOutcome,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        PluginSession::publish_recorded(self, address, outcome)
    }
}

/// The tool boundary's state refusal text, before plugin error attribution.
pub(crate) fn proposal_failure_message(error: &PluginError) -> String {
    match error {
        PluginError::State(cause) => cause.to_string(),
        error => error.to_string(),
    }
}
