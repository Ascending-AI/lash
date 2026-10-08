use super::{RuntimeError, RuntimeSessionState, TurnCommitDraft, TurnGraphAppendDraft};

use crate::facade_support::AgentFrameReasonFacadeOps as _;
use crate::facade_support::SessionGraphFacadeOps;
use crate::session_model::SessionHistoryRecord;
use crate::store::{GraphAppend, StoreError};
use crate::{MessageSequence, PluginSession, Session, SessionPolicy, SessionReadView, TurnOutcome};
use std::sync::Arc;

mod execution_state;
mod materialize;
use execution_state::*;
pub(in crate::runtime) use execution_state::{
    SeedCarries, committed_frame_transition, derive_seed_carries,
};
mod durable_commit;
mod recorded_assembly;
pub use recorded_assembly::RecordedTurnAssembly;

/// Derive the stable ids of the nodes `graph` appends under `operation`, and
/// rename them in `state`, its current frame among them.
#[expect(
    clippy::expect_used,
    reason = "derived graph node identities are non-empty"
)]
fn derive_commit_node_ids(
    state: &mut RuntimeSessionState,
    graph: &mut GraphAppend,
    operation: &crate::OperationId,
) -> Result<Vec<(crate::NodeId, crate::NodeId)>, StoreError> {
    let session_id = state.session_id.clone();
    let node_id_mapping = graph.derive_node_ids(&session_id, operation)?;
    state
        .session_graph
        .remap_node_ids(&session_id, &node_id_mapping);
    if let Some(current) = state.current_frame_node_id.as_mut()
        && let Some((_, derived)) = node_id_mapping
            .iter()
            .find(|(draft, _)| draft == current.as_str())
    {
        *current = crate::FrameNodeId::new(derived.clone())
            .expect("derived graph node identities are non-empty");
    }
    state.agent_frames = state.session_graph.agent_frame_records(&session_id);
    Ok(node_id_mapping)
}

fn execution_state_capture_error(err: crate::SessionError) -> StoreError {
    match err {
        crate::SessionError::Plugin(crate::PluginError::Runtime(error)) => {
            StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(error),
            }
        }
        crate::SessionError::Plugin(crate::PluginError::RuntimeEffectController(error)) => {
            StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(error.into_runtime_error()),
            }
        }
        err => StoreError::ExecutionStateCaptureFailed {
            message: err.to_string(),
        },
    }
}

#[derive(Debug)]
pub(super) struct ProgressBoundaryResult {
    pub(super) protocol_events: Vec<crate::ProtocolEvent>,
}

struct ProgressBoundarySnapshot<'a> {
    policy: SessionPolicy,
    turn_index: usize,
    messages: MessageSequence,
    event_delta: Vec<SessionHistoryRecord>,
    execution_state_update: ExecutionStateUpdate,
    plugins: Option<&'a PluginSession>,
}

pub(super) struct TurnBoundary {
    fleet_format: crate::FleetFormat,
    /// `Some` at every point outside `final_state_mut`'s transition, which
    /// takes the stage, rewrites `Drafting` into `Finalized`, and puts it
    /// back. The transient `None` is the honest "in transit" reading: a
    /// fabricated `Finalized` placeholder would install a made-up
    /// `RuntimeSessionState` if the transition ever panicked mid-move.
    stage: Option<TurnCommitStage>,
    clock: Arc<dyn crate::Clock>,
    definition_engines: crate::ProcessEngineRegistry,
    operation_scope: crate::ExecutionScope,
    commit_budget: crate::CommitBudget,
    metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    trace: Option<crate::trace::TraceStanding>,
    trace_metadata: std::collections::BTreeMap<String, serde_json::Value>,
    /// In-turn graph appends riding this turn's commit. Held here as well as
    /// on the draft so services created after finalization still share it.
    graph_appends: TurnGraphAppendDraft,
    /// The reply as the protocol driver materialized it, recorded by the
    /// driver when the turn finishes so the final commit recognizes it by
    /// identity.
    protocol_terminal_output: materialize::ProtocolTerminalOutput,
}

/// Explicit two-phase lifecycle for a turn commit.
/// Drafting accumulates progress; finalization irreversibly assembles and
/// commits the completed turn once.
enum TurnCommitStage {
    Drafting(Box<TurnCommitDraft>),
    Finalized(Box<FinalizedTurnCommitStage>),
}

struct FinalizedTurnCommitStage {
    state: RuntimeSessionState,
}

impl TurnBoundary {
    pub(super) fn with_fleet_format(mut self, fleet: crate::FleetFormat) -> Self {
        self.fleet_format = fleet;
        self
    }
    pub(super) fn with_metrics(
        mut self,
        metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    pub(super) fn with_trace_metadata(
        mut self,
        metadata: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Self {
        self.trace_metadata = metadata;
        self
    }

    pub(super) fn with_trace(mut self, trace: crate::trace::TraceStanding) -> Self {
        self.trace = Some(trace);
        self
    }

    pub(super) fn with_definition_engines(mut self, engines: crate::ProcessEngineRegistry) -> Self {
        self.definition_engines = engines;
        self
    }

    pub(super) fn final_operation(&self) -> crate::OperationId {
        crate::OperationId::new(self.operation_scope.clone(), "final")
    }

    #[cfg(test)]
    pub(super) fn from_state(state: RuntimeSessionState) -> Self {
        let scope = crate::ExecutionScope::turn(&state.session_id, "test-turn");
        Self::from_state_with_clock(
            state,
            Arc::new(lash_core_ids::test_clock::TestClock::new(1_700_000_000_000)),
            scope,
            crate::CommitBudget::bounded(1024 * 1024, 512),
        )
    }
    pub(super) fn from_state_with_clock(
        state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        operation_scope: crate::ExecutionScope,
        commit_budget: crate::CommitBudget,
    ) -> Self {
        let graph_appends = TurnGraphAppendDraft::from_resident_state(&state, Arc::clone(&clock));
        Self::from_state_with_graph_appends(
            state,
            clock,
            operation_scope,
            commit_budget,
            graph_appends,
        )
    }

    /// Opens the boundary on a graph-append draft the turn's services already
    /// record into, so appends made before the boundary existed (prepare-turn
    /// hooks) ride this commit too.
    pub(super) fn from_state_with_graph_appends(
        state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        operation_scope: crate::ExecutionScope,
        commit_budget: crate::CommitBudget,
        graph_appends: TurnGraphAppendDraft,
    ) -> Self {
        let draft_clock = Arc::clone(&clock);
        Self {
            fleet_format: crate::FleetFormat::current(),
            stage: Some(TurnCommitStage::Drafting(Box::new(
                TurnCommitDraft::from_state_with_graph_appends(
                    state,
                    draft_clock,
                    operation_scope.id(),
                    graph_appends.clone(),
                ),
            ))),
            clock,
            definition_engines: crate::ProcessEngineRegistry::new(),
            operation_scope,
            commit_budget,
            metrics: Default::default(),
            trace: None,
            trace_metadata: Default::default(),
            graph_appends,
            protocol_terminal_output: materialize::ProtocolTerminalOutput::default(),
        }
    }

    pub(super) fn record_protocol_terminal_output(
        &mut self,
        message_ids: impl IntoIterator<Item = String>,
    ) {
        self.protocol_terminal_output.record(message_ids);
    }

    pub(super) fn graph_appends(&self) -> &TurnGraphAppendDraft {
        &self.graph_appends
    }

    fn stage_ref(&self) -> &TurnCommitStage {
        match self.stage.as_ref() {
            Some(stage) => stage,
            None => {
                unreachable!("turn commit stage is only absent inside final_state_mut")
            }
        }
    }

    fn stage_mut(&mut self) -> &mut TurnCommitStage {
        match self.stage.as_mut() {
            Some(stage) => stage,
            None => {
                unreachable!("turn commit stage is only absent inside final_state_mut")
            }
        }
    }

    pub(super) fn state_mut(&mut self) -> &mut RuntimeSessionState {
        match self.stage_mut() {
            TurnCommitStage::Drafting(draft) => draft.state_mut(),
            TurnCommitStage::Finalized(finalized) => &mut finalized.state,
        }
    }
    pub(super) fn state(&self) -> &RuntimeSessionState {
        match self.stage_ref() {
            TurnCommitStage::Drafting(draft) => draft.state(),
            TurnCommitStage::Finalized(finalized) => &finalized.state,
        }
    }
    pub(super) fn apply_prepared_messages(&mut self, messages: &MessageSequence) {
        self.draft_mut().apply_prepared_messages(messages);
    }
    pub(super) fn read_view(
        &self,
        policy: crate::SessionPolicy,
        turn_index: usize,
        protocol_turn_options: crate::ProtocolTurnOptions,
        messages: MessageSequence,
    ) -> SessionReadView {
        self.draft_ref()
            .read_view(policy, turn_index, protocol_turn_options, messages)
    }
    pub(super) fn active_events(&self) -> lash_sansio::AppendVec<SessionHistoryRecord> {
        self.draft_ref().active_events()
    }

    #[cfg(test)]
    pub(super) fn message_sequence(&self) -> MessageSequence {
        self.draft_ref().message_sequence()
    }
    pub(super) fn finalize_turn_read_state(
        &mut self,
        new_messages: MessageSequence,
        cancelled: bool,
    ) {
        self.draft_mut()
            .finalize_turn_read_state(new_messages, cancelled);
    }

    pub(super) async fn prepared_checkpoint(
        &mut self,
        policy: SessionPolicy,
        turn_index: usize,
        messages: &MessageSequence,
        mut session: Option<&mut Session>,
    ) -> Result<(), StoreError> {
        let fleet_format = self.fleet_format;
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(());
        }

        if let Some(session) = session.as_deref_mut() {
            probe_execution_state_capture(session)
                .await
                .map_err(execution_state_capture_error)?;
        }
        self.apply_prepared_messages(messages);
        let plugins = session
            .as_deref()
            .map(|session| Arc::clone(session.plugins()));
        let state = self.draft_mut().state_mut();
        state.policy = policy;
        state.turn_index = turn_index;
        if let Some(plugins) = plugins.as_ref() {
            state
                .capture_plugin_states(plugins.as_ref(), fleet_format)
                .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                    error: Box::new(error),
                })?;
        }
        Ok(())
    }

    pub(super) async fn progress_boundary(
        &mut self,
        session: &mut Session,
        policy: SessionPolicy,
        turn_index: usize,
        messages: MessageSequence,
        event_delta: Vec<SessionHistoryRecord>,
    ) -> Result<ProgressBoundaryResult, RuntimeError> {
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(ProgressBoundaryResult {
                protocol_events: Vec::new(),
            });
        }

        probe_execution_state_capture(session)
            .await
            .map_err(|err| {
                super::runtime_error_from_store_commit(execution_state_capture_error(err))
            })?;
        let plugins = Arc::clone(session.plugins());
        self.progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy,
            turn_index,
            messages,
            event_delta,
            execution_state_update: ExecutionStateUpdate::Clean,
            plugins: Some(plugins.as_ref()),
        })
        .await
    }

    async fn progress_boundary_with_snapshot(
        &mut self,
        snapshot: ProgressBoundarySnapshot<'_>,
    ) -> Result<ProgressBoundaryResult, RuntimeError> {
        let fleet_format = self.fleet_format;
        let ProgressBoundarySnapshot {
            policy,
            turn_index,
            messages,
            event_delta,
            execution_state_update,
            plugins,
        } = snapshot;
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(ProgressBoundaryResult {
                protocol_events: Vec::new(),
            });
        }

        {
            let draft = self.draft_mut();
            draft.apply_prepared_messages(&messages);
            let state = draft.state_mut();
            state.policy = policy;
            state.turn_index = turn_index;
            execution_state_update
                .apply(state)
                .map_err(super::runtime_error_from_store_commit)?;
            if let Some(plugins) = plugins {
                state.capture_plugin_states(plugins, fleet_format)?;
            }
        }
        let protocol_events = self.apply_event_delta(event_delta);
        Ok(ProgressBoundaryResult { protocol_events })
    }

    pub(super) fn apply_event_delta(
        &mut self,
        event_delta: Vec<SessionHistoryRecord>,
    ) -> Vec<crate::ProtocolEvent> {
        let protocol_events = event_delta
            .into_iter()
            .filter_map(|event| match event {
                SessionHistoryRecord::Protocol(event) => Some(event),
                SessionHistoryRecord::Conversation(_) => None,
            })
            .collect::<Vec<_>>();
        self.draft_mut().append_events(
            protocol_events
                .iter()
                .cloned()
                .map(SessionHistoryRecord::Protocol),
        );
        protocol_events
    }

    fn draft_ref(&self) -> &TurnCommitDraft {
        match self.stage_ref() {
            TurnCommitStage::Drafting(draft) => draft.as_ref(),
            TurnCommitStage::Finalized(_) => {
                panic!("turn commit draft is unavailable after final state materialization")
            }
        }
    }

    fn draft_mut(&mut self) -> &mut TurnCommitDraft {
        match self.stage_mut() {
            TurnCommitStage::Drafting(draft) => draft.as_mut(),
            TurnCommitStage::Finalized(_) => {
                panic!("turn commit draft is unavailable after final state materialization")
            }
        }
    }

    fn final_state_mut(&mut self) -> &mut RuntimeSessionState {
        let stage = self.stage.take();
        self.stage = Some(match stage {
            Some(TurnCommitStage::Drafting(draft)) => {
                TurnCommitStage::Finalized(Box::new(FinalizedTurnCommitStage {
                    state: (*draft).into_final_state(),
                }))
            }
            Some(finalized) => finalized,
            None => unreachable!("turn commit stage is only absent during this transition"),
        });
        match self.stage.as_mut() {
            Some(TurnCommitStage::Finalized(finalized)) => &mut finalized.state,
            _ => unreachable!("stage was just finalized"),
        }
    }

    /// Records a protocol `AgentFrameSwitch` outcome into the turn's one
    /// frame-transition slot (FIG-3303).
    ///
    /// The slot holds only this outcome: a context-pressure frame committed
    /// on its own before the turn ran. Recording the same outcome twice is a
    /// replay and answers the first record; any differing second request
    /// refuses the commit typed (see
    /// [`TurnGraphAppendDraft::record_outcome_frame_switch`]).
    fn record_outcome_frame_switch(&mut self, outcome: &TurnOutcome) -> Result<(), StoreError> {
        let TurnOutcome::AgentFrameSwitch {
            frame_key,
            task,
            initial_nodes,
        } = outcome
        else {
            return Ok(());
        };
        let request = super::turn_commit_draft::OutcomeFrameSwitch {
            operation_id: format!("{}:turn-outcome-frame-switch", self.operation_scope.id()),
            frame_key: frame_key.clone(),
            task: task.clone(),
            reason: crate::AgentFrameReason::continue_as(),
            initial_nodes: initial_nodes.clone(),
        };
        let session_id = self.state().session_id.clone();
        let current_frame_node_id = self.state().current_frame_node_id.clone();
        self.graph_appends
            .record_outcome_frame_switch(&session_id, current_frame_node_id.as_deref(), &request)
            .map(|_| ())
            .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(error),
            })
    }
}

#[cfg(test)]
mod tests;
