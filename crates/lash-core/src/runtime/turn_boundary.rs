use super::turn_graph_editor::ReadProjectionDiagnostic;
use super::{RuntimeError, RuntimeSessionState, TurnCommitDraft, TurnGraphAppendDraft};
use crate::ActorContext;
use crate::TurnId;
use crate::facade_support::AgentFrameReasonFacadeOps as _;
use crate::facade_support::SessionGraphFacadeOps;
use crate::runtime::turn_settlement::TurnIngressSettlement;
use crate::session_model::SessionHistoryRecord;
use crate::store::{GraphAppend, RuntimeCommit, StoreError};
use crate::{
    AssembledTurn, MessageSequence, PluginSession, Session, SessionPolicy, SessionReadView,
    TurnOutcome,
};
use std::sync::Arc;

mod materialize;
use materialize::*;
mod execution_state;
use execution_state::*;
pub(in crate::runtime) use execution_state::{
    SeedCarries, committed_frame_transition, derive_seed_carries,
};
mod final_commit_input;
use final_commit_input::FinalCommitInput;
mod recorded_assembly;
pub use recorded_assembly::RecordedTurnAssembly;
#[cfg(feature = "testing")]
pub use recorded_assembly::classify_output_state;
type FinalCommitResult = Result<(crate::TurnCancelInputOutcome, bool), StoreError>;

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
    /// What the final commit presents when the turn runs under an admitted
    /// run (FIG-3600 S7): the run's shift fence, and the run's terminal
    /// evidence when this turn ends it.
    shift_commit: Option<ShiftCommit>,
    /// The logical run the turn runs under, whose park the final commit
    /// clears (FIG-3600 S7, D2 §1.3 P3).
    park_run: Option<crate::TurnId>,
}

/// The frame end a final commit makes (ADR 0113 §3.1). A switch the turn
/// makes ends the frame the turn was admitted on (`ended`) and carries
/// `carries` into the frame it opens; with no `ended`, the commit ends the
/// last committed frame when a resident open moved the session past it. The
/// committing turn is the gate.
pub(super) struct FrameSwitchCommit {
    ended: Option<crate::FrameNodeId>,
    carries: SeedCarries,
    committing: crate::ExecutionScope,
}

/// A final commit's shift fence and run, and the terminal evidence it
/// writes.
pub(super) type ShiftCommit = crate::runtime::shift::ShiftCommit;

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
            shift_commit: None,
            park_run: None,
        }
    }

    /// Present `shift_commit` on the final commit.
    pub(super) fn set_shift_commit(&mut self, shift_commit: Option<ShiftCommit>) {
        self.shift_commit = shift_commit;
    }

    /// Clear `run`'s park with the final commit.
    pub(super) fn set_park_run(&mut self, run: crate::TurnId) {
        self.park_run = Some(run);
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
    pub(super) fn message_sequence(&self) -> MessageSequence {
        self.draft_ref().message_sequence()
    }
    pub(super) fn take_projection_diagnostics(&mut self) -> Vec<ReadProjectionDiagnostic> {
        self.draft_mut().take_projection_diagnostics()
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

    pub(super) fn export_state_for_assembly(&mut self) -> crate::SessionSnapshot {
        self.final_state_mut().to_snapshot()
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

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn final_commit(
        &mut self,
        returned_turn: &mut AssembledTurn,
        session: Option<&mut Session>,
        ingress_settlement: TurnIngressSettlement,
        pending_follow_on: Option<crate::store::PendingFollowOn>,
        interrupted_turn: Option<crate::store::InterruptedTurnClosure>,
        turn_control_resolver: Option<&ActorContext>,
        recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
    ) -> Result<bool, StoreError> {
        // Record the outcome before capturing execution state: a second author
        // that conflicts refuses here, with nothing captured and nothing
        // written.
        self.record_outcome_frame_switch(&returned_turn.outcome)?;
        let agent_frame_switch_materializes = self.recorded_frame_switch_materializes();
        let (store, plugins, execution_state_update) = match session {
            Some(session) => {
                let store = session.history_store();
                // The final gate may cancel an already accepted suspension.
                // Terminal cleanup precedes the capture published atomically
                // with the Run's terminal and the cleared follow-on.
                let run_terminates = self.shift_commit.as_ref().map_or_else(
                    || {
                        matches!(
                            returned_turn.outcome,
                            TurnOutcome::Finished(_) | TurnOutcome::Stopped(_)
                        )
                    },
                    |commit| commit.terminal.is_some(),
                );
                if run_terminates && let Some(executor) = session.plugins().code_executor() {
                    executor
                        .settle_code_execution(crate::plugin::CodeExecutionOutcome::Terminated)
                        .await
                        .map_err(execution_state_capture_error)?;
                }
                let execution_state_update = if agent_frame_switch_materializes {
                    let initial_nodes = self
                        .graph_appends
                        .pending_frame_switch()
                        .map(|recorded| recorded.initial_nodes().to_vec())
                        .unwrap_or_default();
                    let successor = self
                        .graph_appends
                        .pending_frame_switch()
                        .map(|recorded| {
                            crate::session_graph::frame_node_id(
                                &self.state().session_id,
                                recorded.request.frame_key.as_str(),
                            )
                        })
                        .ok_or_else(|| {
                            StoreError::Backend("frame switch has no successor".into())
                        })?;
                    frame_switch_execution_state_update(session, &successor, &initial_nodes)
                        .await
                        .map_err(execution_state_capture_error)?
                } else {
                    capture_execution_state_update(session)
                        .await
                        .map_err(execution_state_capture_error)?
                };
                let plugins = Arc::clone(session.plugins());
                (store, Some(plugins), execution_state_update)
            }
            None => (None, None, ExecutionStateUpdate::Clean),
        };
        let captured_execution_state = !agent_frame_switch_materializes
            && !matches!(execution_state_update, ExecutionStateUpdate::Clean);
        // The returned state moves into the commit; a successful commit
        // hands the committed state back below, and a failed one abandons
        // the turn.
        let returned_graph = std::mem::take(&mut returned_turn.state.session_graph);
        let returned_state = crate::SessionSnapshot {
            session_graph: returned_graph,
            ..returned_turn.state.clone()
        };
        let commit_result = self
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state,
                tool_calls: &returned_turn.tool_calls,
                omitted: returned_turn.omitted.as_ref(),
                retained_outputs: &returned_turn.retained_outputs,
                plugins: plugins.as_deref(),
                execution_state_update,
                agent_frame_switch_materializes,
                store: store.as_ref(),
                failure_evidence: &returned_turn.failure_evidence,
                outcome: &returned_turn.outcome,
                ingress_settlement,
                pending_follow_on,
                interrupted_turn,
                turn_control_resolver,
                recorded_attachment_intent_ids,
            })
            .await;
        settle_execution_state_capture(
            plugins.as_deref(),
            captured_execution_state,
            commit_result.is_ok(),
        )
        .await;
        let (turn_cancel_input_outcome, work_remaining) = commit_result?;
        returned_turn.state = self.final_state_mut().to_snapshot();
        returned_turn.turn_cancel_input_outcome = turn_cancel_input_outcome;
        Ok(work_remaining)
    }

    pub(super) fn append_cancelled_opener_messages(&mut self, messages: &[crate::Message]) {
        let clock = Arc::clone(&self.clock);
        self.final_state_mut()
            .append_active_conversation_messages_with_clock(messages, clock.as_ref());
    }

    /// A native intent CAS miss changes only terminal assembly. The issued
    /// work and admission remain this turn's; neither is executed again.
    pub(super) async fn refresh_cancelled_commit(
        &mut self,
        turn: &mut AssembledTurn,
        interrupted: &mut crate::store::InterruptedTurnClosure,
        store: &crate::store::SessionStore,
    ) -> Result<(), StoreError> {
        let authorization = interrupted.settlement.authorization();
        let address = authorization.address();
        let observed = store.turn_cancel_request_intent(&address).await?;
        let refusal = || StoreError::TurnCancelIntentChanged {
            session_id: address.session_id.clone(),
            turn_id: address.turn_id.clone(),
        };
        if interrupted.admitted_intent.is_none() || observed == interrupted.observed_intent {
            return Err(refusal());
        }
        let base = observed.request().ok_or_else(refusal)?.evidence();
        let effective = interrupted
            .settlement
            .effective_cancellation()
            .filter(|previous| {
                previous.undelivered == base.undelivered
                    && (previous.request_id == base.request_id
                        || previous.mode.is_stronger_than(base.mode))
            })
            .cloned()
            .unwrap_or_else(|| base.clone());
        let shift = self.shift_commit.as_mut().ok_or_else(refusal)?;
        let refreshed = crate::TurnCancelClosureAuthorization::new(
            address.clone(),
            authorization.binding_id(),
            authorization.admitted_scope().clone(),
            authorization.cancel_key().clone(),
            authorization.escalation_key().clone(),
            authorization.terminal_key().clone(),
            crate::TurnCancelClosureProposal::CancelRequested(base.clone()),
            observed.clone(),
            &shift.fence,
        )
        .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
            error: Box::new(error),
        })?;
        interrupted.observed_intent = observed;
        interrupted.settlement =
            crate::TurnCancelClosureSettlement::new(refreshed, Some(base), Some(effective.clone()));
        let stop = crate::TurnStop::Cancelled {
            evidence: effective,
        };
        shift.terminal = Some(crate::store::RunTerminalWrite {
            run: shift.run.clone(),
            commit: crate::store::TurnCommitId::of_physical_turn(&shift.run, &address.turn_id)
                .ok_or_else(refusal)?,
            turn: address.turn_id,
            outcome: crate::store::RunCommittedOutcome::Stopped(stop.clone()),
        });
        // The failed commit materialized the graph but published none of it.
        // Restore that candidate to the assembly before recapturing the
        // terminated executor and clearing the successor intent.
        turn.state = self.state().to_snapshot();
        turn.outcome = TurnOutcome::Stopped(stop);
        Ok(())
    }

    pub(super) fn into_final_state(self) -> RuntimeSessionState {
        match self.stage {
            Some(TurnCommitStage::Drafting(draft)) => (*draft).into_final_state(),
            Some(TurnCommitStage::Finalized(finalized)) => finalized.state,
            None => {
                unreachable!("turn commit stage is only absent inside final_state_mut")
            }
        }
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

    /// Whether this turn's one recorded switch opens a frame the session is
    /// not already in. Derived from the slot alone, so the commit and the
    /// protocol-execution clear it executes answer the same question.
    fn recorded_frame_switch_materializes(&self) -> bool {
        self.graph_appends
            .pending_frame_switch()
            .is_some_and(|recorded| {
                materialize::agent_frame_switch_materializes(
                    &self.state().session_id,
                    &recorded.request.frame_key,
                    self.state().current_frame_node_id.as_deref(),
                )
            })
    }

    async fn final_commit_with_snapshots(
        &mut self,
        input: FinalCommitInput<'_>,
    ) -> FinalCommitResult {
        let fleet_format = self.fleet_format;
        let FinalCommitInput {
            returned_state,
            tool_calls,
            omitted,
            retained_outputs,
            plugins,
            execution_state_update,
            agent_frame_switch_materializes,
            store,
            failure_evidence,
            outcome,
            ingress_settlement,
            pending_follow_on,
            interrupted_turn,
            turn_control_resolver,
            recorded_attachment_intent_ids,
        } = input;
        // Every path into the final commit reconciles the same way. A turn
        // executed through `final_commit` already recorded this outcome so the
        // refusal lands before execution state is captured; recording it here
        // again is a replay of that record and answers it unchanged.
        self.record_outcome_frame_switch(outcome)?;
        let clock = Arc::clone(&self.clock);
        let graph_appends = self.graph_appends.clone();
        let protocol_terminal_output = self.protocol_terminal_output.clone();
        let turn_id = crate::TurnId::parse(self.operation_scope.id())?;
        let terminal_message_id = format!("m_turn_{turn_id}_assistant");
        let state = self.final_state_mut();
        state.adopt_snapshot(returned_state);
        // The follow-on the head owes after this commit: written by a frame
        // switch, cleared by the follow-on's own terminal commit (ADR 0101
        // §3). A store-less session keeps the same fact resident.
        state.pending_follow_on = pending_follow_on.map(Box::new);
        if let Some(plugins) = plugins {
            state
                .capture_plugin_states(plugins, fleet_format)
                .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                    error: Box::new(error),
                })?;
        }
        // The frame the turn was admitted on, which a switch this commit
        // opens ends (ADR 0113 §3.1), and what the switch carries out of it.
        let admitted_frame = state.current_frame_node_id.clone();
        let frame_carries = execution_state_update.carries();
        execution_state_update.apply(state)?;
        materialize_turn_reply(
            state,
            outcome,
            clock.as_ref(),
            &turn_id,
            &terminal_message_id,
            &protocol_terminal_output,
        );
        // The pre-snapshot decision that cleared protocol execution state and
        // this post-snapshot state must never diverge; fail in debug/tests
        // instead of silently clearing the wrong frame's state.
        debug_assert_eq!(
            agent_frame_switch_materializes,
            graph_appends
                .pending_frame_switch()
                .is_some_and(|recorded| materialize::agent_frame_switch_materializes(
                    &state.session_id,
                    &recorded.request.frame_key,
                    state.current_frame_node_id.as_deref(),
                ))
        );
        // Appends recorded after finalization (finalize-turn hooks) land here,
        // after everything the turn materialized, and the turn's one recorded
        // agent-frame switch opens after them.
        graph_appends
            .fold_into_final_state(state)
            .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(error),
            })?;
        // `apply_commit` takes the finalized state directly, so the values it
        // read from `self` are hoisted before the state borrow begins.
        let operation = self.final_operation();
        let commit_budget = self.commit_budget;
        let metrics = self.metrics.clone();
        let trace = self.trace.clone();
        let trace_metadata = self.trace_metadata.clone();
        let shift_commit = self.shift_commit.clone();
        let park_run = self.park_run.clone();
        // A switch this turn makes ends the frame the turn was admitted on;
        // otherwise the commit ends whatever frame a resident open left
        // behind, if any.
        let definition_engines = self.definition_engines.clone();
        let frame_switch = FrameSwitchCommit {
            ended: admitted_frame.filter(|_| agent_frame_switch_materializes),
            carries: frame_carries,
            committing: self.operation_scope.clone(),
        };
        let state = self.final_state_mut();

        if let Some(store) = store {
            let graph = state.pending_graph_commit();
            let committed_attachment_ids =
                committed_attachment_ids(state, tool_calls, omitted, retained_outputs);
            // ADR 0058: this deduped union of explicit ids and recorded
            // write-ahead intent ids is a declared estimate, not the stamped
            // row count — replay can undercount prior-attempt rows, and
            // cancelled or failed puts can overcount. That residual is
            // accepted; admission never queries the store.
            let adopted_intent_rows = committed_attachment_ids
                .iter()
                .cloned()
                .chain(recorded_attachment_intent_ids)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                .try_into()
                .unwrap_or(u64::MAX);
            Box::pin(Self::apply_commit(
                &definition_engines,
                state,
                commit_budget,
                &metrics,
                trace.as_ref(),
                &trace_metadata,
                super::turn_loop::trace_outcome(outcome),
                store,
                graph,
                failure_evidence,
                crate::store::TurnCommitOutcome::from_terminal(outcome),
                operation,
                ingress_settlement,
                interrupted_turn,
                turn_control_resolver,
                committed_attachment_ids,
                adopted_intent_rows,
                shift_commit,
                park_run,
                frame_switch,
            ))
            .await
        } else {
            // No store will ever rehydrate this commit: the accepted execution
            // stays resident for the next same-frame restore (FIG-2521).
            state.discard_runtime_snapshots_retaining_accepted_execution();
            Ok((Default::default(), true))
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::expect_used,
        reason = "derived graph node identities are non-empty"
    )]
    async fn apply_commit(
        definition_engines: &crate::ProcessEngineRegistry,
        state: &mut RuntimeSessionState,
        commit_budget: crate::CommitBudget,
        metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
        trace: Option<&crate::trace::TraceStanding>,
        trace_metadata: &std::collections::BTreeMap<String, serde_json::Value>,
        trace_outcome: Option<lash_trace::TraceTurnOutcome>,
        store: &crate::store::SessionStore,
        mut graph: GraphAppend,
        failure_evidence: &[crate::TurnFailureEvidence],
        outcome: crate::store::TurnCommitOutcome,
        operation: crate::OperationId,
        ingress_settlement: TurnIngressSettlement,
        interrupted_turn: Option<crate::store::InterruptedTurnClosure>,
        _turn_control_resolver: Option<&ActorContext>,
        committed_attachment_ids: Vec<crate::AttachmentId>,
        adopted_intent_rows: u64,
        shift_commit: Option<ShiftCommit>,
        park_run: Option<TurnId>,
        frame_switch: FrameSwitchCommit,
    ) -> FinalCommitResult {
        let session_id = state.session_id.clone();
        let node_id_mapping = graph.derive_node_ids(&session_id, &operation)?;
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
        let FrameSwitchCommit {
            ended,
            carries,
            committing,
        } = frame_switch;
        let ended = ended.map(|ended| {
            node_id_mapping
                .iter()
                .find(|(draft, _)| draft == ended.as_str())
                .map(|(_, derived)| {
                    crate::FrameNodeId::new(derived.clone())
                        .expect("derived graph node identities are non-empty")
                })
                .unwrap_or(ended)
        });
        let persisted_node_ids = graph
            .nodes()
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let frame_transition =
            committed_frame_transition(state, ended, carries, &committing, &persisted_node_ids)?;
        let mut commit = RuntimeCommit::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            operation,
            commit_budget,
            store.fleet_format(),
        )?
        .with_committed_attachments(committed_attachment_ids);
        commit.failure_evidence = failure_evidence.to_vec();
        commit.outcome = Some(outcome);
        commit.trace = trace.zip(trace_outcome).and_then(|(trace, outcome)| {
            let scope = trace.scope()?.clone();
            let lash_trace::TraceScopeOwner::Turn { turn_id, .. } = &scope.scope.owner else {
                return None;
            };
            let context = lash_trace::TraceContext::default()
                .for_session(session_id.clone())
                .for_turn(turn_id.clone())
                .for_turn_index(state.turn_index);
            Some(Box::new(crate::store::TurnTraceReceipt {
                metadata: trace_metadata.clone(),
                scope,
                context,
                outcome,
                run_scope: shift_commit
                    .as_ref()
                    .filter(|commit| commit.terminal.is_some())
                    .and_then(|commit| commit.trace_scope.clone()),
            }))
        });
        commit.adopted_intent_rows = adopted_intent_rows;
        // A cancelled turn's undelivered input follows the cancellation's
        // disposition; every other handed-back row is deferred.
        let disposition = interrupted_turn
            .as_ref()
            .and_then(crate::store::InterruptedTurnClosure::cancellation)
            .map_or(crate::TurnCancelUndeliveredInputPolicy::Defer, |evidence| {
                evidence.undelivered
            });
        commit.interrupted_turn = interrupted_turn;
        // The rows a turn settles are its run's, settled under the run's
        // shift fence (FIG-3927): a turn that runs under no admitted run
        // admitted nothing and settles nothing.
        match shift_commit {
            Some(shift_commit) => {
                commit.shift_fence = Some(Box::new(shift_commit.fence));
                commit.run_terminal = shift_commit.terminal.map(Box::new);
                if !ingress_settlement.is_empty() {
                    commit.ingress =
                        Some(ingress_settlement.into_ingress(shift_commit.run, disposition));
                }
            }
            None if !ingress_settlement.is_empty() => {
                return Err(StoreError::IngressSettlementUnfenced { session_id });
            }
            None => {}
        }
        commit.park_run = park_run;
        super::frame_definition_carry::prepare(definition_engines, frame_transition.as_ref())
            .await?;
        commit.frame_transition = frame_transition;
        // Cancellation-intent retries are progress-fenced: every refusal
        // proves a newer durable intent revision. Refresh only that snapshot:
        // the settlement and materialized cancellation evidence are already
        // authenticated and may contain live execution enrichment (such as the
        // iteration that honoured an AfterStep request) which a raw promise
        // peek cannot reconstruct.
        let result = loop {
            match store
                .commit_runtime_state_verified(commit.clone(), metrics)
                .await
            {
                Ok(result) => break result,
                Err(error @ crate::StoreError::TurnCancelIntentChanged { .. }) => {
                    // Only a commit closing an interrupted turn carries the
                    // predicate this refusal names.
                    let Some(interrupted) = commit.interrupted_turn.as_mut() else {
                        return Err(error);
                    };
                    if interrupted.admitted_intent.is_some() {
                        // A newer intent needs turn reassembly before publication.
                        // Redrive the recorded work rather than patching its terminal.
                        return Err(error);
                    }
                    let address = crate::TurnAddress::new(&session_id, interrupted.turn_id());
                    interrupted.observed_intent =
                        store.turn_cancel_request_intent(&address).await?;
                }
                Err(err) => return Err(err),
            }
        };
        if !result.receipt_replayed
            && let (Some(trace), Some(receipt)) = (trace, result.trace.as_ref())
        {
            let standing = trace.under(receipt.scope.clone());
            let permit = lash_trace::EmissionPermit::new_transition();
            standing.transition(
                Some(&permit),
                receipt.scope.started_at_ms,
                lash_trace::TraceTransitionKind::Started,
                0,
                || {
                    (
                        receipt.context.clone(),
                        lash_trace::TraceEvent::TurnStarted {
                            metadata: receipt.metadata.clone(),
                        },
                    )
                },
            );
            standing.transition(
                Some(&permit),
                result.committed_at_ms,
                lash_trace::TraceTransitionKind::Terminal,
                0,
                || {
                    (
                        receipt.context.clone(),
                        lash_trace::TraceEvent::TurnCompleted {
                            outcome: receipt.outcome.clone(),
                        },
                    )
                },
            );
        }
        if !result.receipt_replayed
            && let (Some(trace), Some(receipt)) = (trace, result.trace.as_ref())
            && let Some(scope) = &receipt.run_scope
        {
            let status = match &result.outcome {
                Some(crate::store::TurnCommitOutcome::Cancelled) => {
                    lash_trace::TraceDomainStatus::Cancelled
                }
                Some(crate::store::TurnCommitOutcome::Failed(_)) => {
                    lash_trace::TraceDomainStatus::Failed
                }
                _ => lash_trace::TraceDomainStatus::Completed,
            };
            trace.under(scope.clone()).transition(
                Some(&lash_trace::EmissionPermit::new_transition()),
                result.committed_at_ms,
                lash_trace::TraceTransitionKind::Terminal,
                0,
                || {
                    (
                        receipt.context.clone(),
                        lash_trace::TraceEvent::DomainCompleted {
                            completion: lash_trace::TraceDomainCompletion::new(
                                lash_trace::TraceDomainOperation::Run,
                                scope.started_at_ms,
                                status,
                            ),
                        },
                    )
                },
            );
        }
        let turn_cancel_input_outcome = result.turn_cancel_input_outcome.clone();
        let work_remaining = result.work_remaining;
        state.apply_persisted_commit_result(result);
        state.mark_node_ids_persisted(persisted_node_ids);
        Ok((turn_cancel_input_outcome, work_remaining))
    }
}

#[cfg(test)]
mod tests;
