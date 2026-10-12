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

    /// Deployment payload admission limits associated with this state sink.
    fn data_limits(&self) -> crate::runtime::RuntimeDataLimits {
        crate::runtime::RuntimeDataLimits::standard()
    }

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
    fn data_limits(&self) -> crate::runtime::RuntimeDataLimits {
        PluginSession::data_limits(self)
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

#[cfg(test)]
mod tests {
    use super::ExecutionState;
    use crate::plugin::{PluginHost, PluginSessionRequest, SessionAuthorityContext};
    use std::sync::Arc;

    /// INTENT-ADMISSION-POLICY: dispatches retain the deployment's intent limits through the state sink.
    #[test]
    fn configured_intent_limits_follow_the_state_sink() {
        let mut limits = crate::runtime::RuntimeDataLimits::standard();
        limits.tool_intents = crate::ToolIntentLimits::new(2, 4_096, 1)
            .expect("the configured count fits the intent index");
        let session = PluginHost::empty(
            crate::ExecutionBudgets::recommended(),
            crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
        )
        .with_data_limits(limits)
        .build_session(PluginSessionRequest::creation(
            "configured-intent-limits",
            SessionAuthorityContext::ambient_fixture(),
        ))
        .expect("plugin session");
        let state: Arc<dyn ExecutionState> = session;

        assert_eq!(state.data_limits(), limits);
    }
}
