use super::turn_graph_editor::ReadProjectionDiagnostic;
use super::{
    RuntimeError, RuntimeErrorCode, RuntimeSessionState, TurnCommitDraft, TurnGraphAppendDraft,
};
use crate::TurnId;
use crate::facade_support::AgentFrameReasonFacadeOps;
use crate::facade_support::SessionGraphFacadeOps;
#[cfg(test)]
use crate::facade_support::SessionNodeProjection;
use crate::runtime::claim_settlement::TurnClaimSettlement;
use crate::session_model::SessionHistoryRecord;
use crate::store::{GraphAppend, RuntimeCommit, RuntimePersistence, StoreError};
use crate::{
    AssembledTurn, MessageSequence, PluginSession, Session, SessionPolicy, SessionReadView,
    TurnOutcome,
};
use std::sync::Arc;

mod materialize;
use materialize::*;
mod accepted_commit;
pub(super) use accepted_commit::AcceptedTurnCommit;
mod execution_state;
use execution_state::*;
mod final_commit_input;
use final_commit_input::FinalCommitInput;
mod recorded_assembly;
pub use recorded_assembly::RecordedTurnAssembly;
#[cfg(feature = "testing")]
pub use recorded_assembly::classify_output_state;
type FinalCommitResult = Result<
    (
        Vec<crate::store::RuntimeUsageDeltaIdentity>,
        crate::TurnCancelInputOutcome,
    ),
    StoreError,
>;

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
    /// `Some` at every point outside `final_state_mut`'s transition, which
    /// takes the stage, rewrites `Drafting` into `Finalized`, and puts it
    /// back. The transient `None` is the honest "in transit" reading: a
    /// fabricated `Finalized` placeholder would install a made-up
    /// `RuntimeSessionState` if the transition ever panicked mid-move.
    stage: Option<TurnCommitStage>,
    clock: Arc<dyn crate::Clock>,
    operation_scope: crate::ExecutionScope,
    commit_budget: crate::CommitBudget,
    /// In-turn graph appends riding this turn's commit. Held here as well as
    /// on the draft so services created after finalization still share it.
    graph_appends: TurnGraphAppendDraft,
    /// The reply as the protocol driver materialized it, recorded by the
    /// driver when the turn finishes so the final commit recognizes it by
    /// identity.
    protocol_terminal_output: materialize::ProtocolTerminalOutput,
    /// What the final commit presents when the turn runs under an admitted
    /// root (FIG-3600 S7): the root's drive fence, and the root's terminal
    /// evidence when this turn ends it.
    drive_commit: Option<DriveCommit>,
}

/// A final commit's drive fence, and the terminal evidence it writes.
pub(super) type DriveCommit = (
    crate::store::DriveFence,
    Option<crate::store::RootTerminalWrite>,
);

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
    pub(super) fn final_operation(&self) -> crate::OperationId {
        crate::OperationId::new(self.operation_scope.clone(), "final")
    }

    #[cfg(test)]
    pub(super) fn from_state(state: RuntimeSessionState) -> Self {
        let scope = crate::ExecutionScope::turn(&state.session_id, "test-turn");
        Self::from_state_with_clock(
            state,
            Arc::new(crate::SystemClock),
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
            stage: Some(TurnCommitStage::Drafting(Box::new(
                TurnCommitDraft::from_state_with_graph_appends(
                    state,
                    draft_clock,
                    operation_scope.id(),
                    graph_appends.clone(),
                ),
            ))),
            clock,
            operation_scope,
            commit_budget,
            graph_appends,
            protocol_terminal_output: materialize::ProtocolTerminalOutput::default(),
            drive_commit: None,
        }
    }

    /// Present `drive_commit` on the final commit.
    pub(super) fn set_drive_commit(&mut self, drive_commit: Option<DriveCommit>) {
        self.drive_commit = drive_commit;
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
    ) -> Result<SessionReadView, crate::SessionGraphScopeError> {
        self.draft_ref()
            .read_view(policy, turn_index, protocol_turn_options, messages)
    }
    pub(super) fn active_events(&self) -> Arc<Vec<SessionHistoryRecord>> {
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
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(());
        }

        if let Some(session) = session.as_deref_mut() {
            probe_execution_state_capture(session)
                .await
                .map_err(accepted_commit::execution_state_capture_error)?;
        }
        self.apply_prepared_messages(messages);
        let plugins = session
            .as_deref()
            .map(|session| Arc::clone(session.plugins()));
        let state = self.draft_mut().state_mut();
        state.policy = policy;
        state.turn_index = turn_index;
        if let Some(plugins) = plugins.as_ref() {
            state.capture_plugin_states(plugins.as_ref());
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
                RuntimeError::new(
                    RuntimeErrorCode::ExecutionStateCaptureFailed,
                    format!("failed to snapshot dirty execution state: {err}"),
                )
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
                state.capture_plugin_states(plugins);
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
        usage_deltas: &[crate::store::RuntimeUsageDelta],
        claim_settlement: TurnClaimSettlement,
        current_session_lease_fence: Option<crate::SessionExecutionLeaseAuthority>,
        pending_follow_on: Option<crate::store::PendingFollowOn>,
        queued_run: Option<Box<crate::store::QueuedRunCommit>>,
        interrupted_turn_input_turn_id: Option<TurnId>,
        interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
        interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
        turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
        turn_control_resolver: Option<&dyn crate::AwaitEventResolver>,
        recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseAuthority>,
    ) -> Result<AcceptedTurnCommit, StoreError> {
        // Record the outcome before capturing execution state: a second author
        // that conflicts refuses here, with nothing captured and nothing
        // written.
        self.record_outcome_frame_switch(&returned_turn.outcome)?;
        let agent_frame_switch_materializes = self.recorded_frame_switch_materializes();
        let (store, plugins, execution_state_update) = match session {
            Some(session) => {
                let store = session.history_store();
                let execution_state_update = if agent_frame_switch_materializes {
                    ExecutionStateUpdate::Clear
                } else {
                    capture_execution_state_update(session)
                        .await
                        .map_err(accepted_commit::execution_state_capture_error)?
                };
                let plugins = Arc::clone(session.plugins());
                (store, Some(plugins), execution_state_update)
            }
            None => (None, None, ExecutionStateUpdate::Clean),
        };
        let captured_execution_state = !agent_frame_switch_materializes
            && !matches!(execution_state_update, ExecutionStateUpdate::Clean);
        let commit_result = self
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_turn.state,
                tool_calls: &returned_turn.tool_calls,
                omitted: returned_turn.omitted.as_ref(),
                plugins: plugins.as_deref(),
                execution_state_update,
                agent_frame_switch_materializes,
                store: store.as_ref().map(|store| store.as_ref()),
                usage_deltas,
                failure_evidence: &returned_turn.failure_evidence,
                outcome: &returned_turn.outcome,
                claim_settlement,
                current_session_lease_fence,
                pending_follow_on,
                queued_run,
                interrupted_turn_input_turn_id,
                interrupted_turn_input_cancellation,
                interrupted_turn_cancel_intent,
                turn_cancel_closure_settlement,
                turn_control_resolver,
                recorded_attachment_intent_ids,
                session_execution_lease_completion,
            })
            .await;
        settle_execution_state_capture(
            plugins.as_deref(),
            captured_execution_state,
            commit_result.is_ok(),
        )
        .await;
        let (confirmed_usage, turn_cancel_input_outcome) = commit_result?;
        returned_turn.state = self.final_state_mut().to_snapshot();
        returned_turn.turn_cancel_input_outcome = turn_cancel_input_outcome;
        Ok(AcceptedTurnCommit::new(confirmed_usage))
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
    /// agent-frame switch slot (FIG-3303).
    ///
    /// The outcome is one author of the turn's switch, not a second place the
    /// switch lives: it reconciles with a plugin-recorded switch through the
    /// slot's own conflict rule (see
    /// [`TurnGraphAppendDraft::record_frame_switch`]), so two authors naming
    /// different frames refuse the commit instead of opening two frames, and
    /// two authors naming the same frame commit one open carrying one set of
    /// seed nodes. Recording the same outcome twice is a replay and answers
    /// the first record.
    fn record_outcome_frame_switch(&mut self, outcome: &TurnOutcome) -> Result<(), StoreError> {
        let TurnOutcome::AgentFrameSwitch {
            frame_key,
            task,
            initial_nodes,
        } = outcome
        else {
            return Ok(());
        };
        let request = crate::SwitchAgentFrameRequest::new(
            format!("{}:turn-outcome-frame-switch", self.operation_scope.id()),
            frame_key.clone(),
            crate::AgentFrameReason::continue_as(),
        )
        .with_task(task.clone())
        .with_initial_nodes(initial_nodes.clone());
        let session_id = self.state().session_id.clone();
        let current_frame_node_id = self.state().current_frame_node_id.clone();
        self.graph_appends
            .record_frame_switch(&session_id, current_frame_node_id.as_deref(), &request)
            .map(|_| ())
            .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
                error: Box::new(RuntimeError::new(
                    RuntimeErrorCode::AgentFrameSwitchAuthorConflict,
                    error.to_string(),
                )),
            })
    }

    /// Whether this turn's one recorded switch opens a frame the session is
    /// not already in. Derived from the slot alone, so the commit and the
    /// protocol-execution clear it drives answer the same question.
    fn recorded_frame_switch_materializes(&self) -> bool {
        self.graph_appends
            .pending_frame_switch()
            .is_some_and(|recorded| {
                materialize::agent_frame_switch_materializes(
                    &self.state().session_id,
                    &recorded.frame_key,
                    self.state().current_frame_node_id.as_deref(),
                )
            })
    }

    async fn final_commit_with_snapshots(
        &mut self,
        input: FinalCommitInput<'_>,
    ) -> FinalCommitResult {
        let FinalCommitInput {
            returned_state,
            tool_calls,
            omitted,
            plugins,
            execution_state_update,
            agent_frame_switch_materializes,
            store,
            usage_deltas,
            failure_evidence,
            outcome,
            claim_settlement,
            current_session_lease_fence,
            pending_follow_on,
            queued_run,
            interrupted_turn_input_turn_id,
            interrupted_turn_input_cancellation,
            interrupted_turn_cancel_intent,
            turn_cancel_closure_settlement,
            turn_control_resolver,
            recorded_attachment_intent_ids,
            session_execution_lease_completion,
        } = input;
        // Every path into the final commit reconciles the same way. A turn
        // driven through `final_commit` already recorded this outcome so the
        // refusal lands before execution state is captured; recording it here
        // again is a replay of that record and answers it unchanged.
        self.record_outcome_frame_switch(outcome)?;
        let clock = Arc::clone(&self.clock);
        let graph_appends = self.graph_appends.clone();
        let protocol_terminal_output = self.protocol_terminal_output.clone();
        let turn_id = crate::TurnId::from(self.operation_scope.id());
        let terminal_message_id = format!("m_turn_{turn_id}_assistant");
        let state = self.final_state_mut();
        state.apply_snapshot(returned_state);
        // The follow-on the head owes after this commit: written by a frame
        // switch, cleared by the follow-on's own terminal commit (ADR 0101
        // §3). A store-less session keeps the same fact resident.
        state.pending_follow_on = pending_follow_on.map(Box::new);
        for delta in usage_deltas {
            crate::store::merge_token_ledger_entry_checked(
                &mut state.token_ledger,
                delta.entry.clone(),
            )?;
        }
        if let Some(plugins) = plugins {
            state.capture_plugin_states(plugins);
        }
        execution_state_update.apply(state)?;
        materialize_terminal_output(
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
                    &recorded.frame_key,
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
        let drive_commit = self.drive_commit.clone();
        let state = self.final_state_mut();

        if let Some(store) = store {
            let graph = state.pending_graph_commit();
            let committed_attachment_ids = committed_attachment_ids(state, tool_calls, omitted);
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
            Self::apply_commit(
                state,
                commit_budget,
                store,
                graph,
                usage_deltas,
                failure_evidence,
                operation,
                claim_settlement,
                current_session_lease_fence,
                queued_run,
                interrupted_turn_input_turn_id,
                interrupted_turn_input_cancellation,
                interrupted_turn_cancel_intent,
                turn_cancel_closure_settlement,
                turn_control_resolver,
                committed_attachment_ids,
                adopted_intent_rows,
                session_execution_lease_completion,
                drive_commit,
            )
            .await
        } else {
            // No store will ever rehydrate this commit: the accepted execution
            // stays resident for the next same-frame restore (FIG-2521).
            state.discard_runtime_snapshots_retaining_accepted_execution();
            Ok((
                usage_deltas
                    .iter()
                    .map(|delta| delta.identity.clone())
                    .collect(),
                Default::default(),
            ))
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::expect_used,
        reason = "derived graph node identities are non-empty"
    )]
    async fn apply_commit(
        state: &mut RuntimeSessionState,
        commit_budget: crate::CommitBudget,
        store: &(dyn RuntimePersistence + '_),
        mut graph: GraphAppend,
        usage_deltas: &[crate::store::RuntimeUsageDelta],
        failure_evidence: &[crate::TurnFailureEvidence],
        operation: crate::OperationId,
        mut claim_settlement: TurnClaimSettlement,
        current_session_lease_fence: Option<crate::SessionExecutionLeaseAuthority>,
        queued_run: Option<Box<crate::store::QueuedRunCommit>>,
        interrupted_turn_input_turn_id: Option<TurnId>,
        interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
        interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
        turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
        _turn_control_resolver: Option<&dyn crate::AwaitEventResolver>,
        committed_attachment_ids: Vec<crate::AttachmentId>,
        adopted_intent_rows: u64,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseAuthority>,
        drive_commit: Option<DriveCommit>,
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
        let persisted_node_ids = graph
            .nodes()
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let mut commit =
            RuntimeCommit::persisted_state_with_graph_commit_and_staged_usage_and_budget(
                state,
                graph,
                usage_deltas,
                operation,
                commit_budget,
            )?
            .with_committed_attachments(committed_attachment_ids);
        commit.failure_evidence = failure_evidence.to_vec();
        commit.adopted_intent_rows = adopted_intent_rows;
        let current_session_lease_generation = current_session_lease_fence
            .as_ref()
            .map(|fence| fence.fencing_token);
        // ADR 0029: final settlement is authorized by head CAS and durable
        // cancellation facts, even after expiry or takeover. A retained lease
        // is not a borrowed append-lane fence. Its release remains ancillary.
        if queued_run.is_none()
            && let Some(completion) = session_execution_lease_completion
        {
            commit = commit.releasing_session_execution_lease(completion);
        }
        commit.completed_queue_claims = claim_settlement.queued.completions.clone();
        commit.completed_turn_input_claims = claim_settlement.turn_inputs.completions.clone();
        commit.undelivered_turn_input_claims =
            std::mem::take(&mut claim_settlement.undelivered_turn_inputs);
        if queued_run.is_some() {
            commit.session_execution_lease_fence = current_session_lease_fence.clone();
        }
        commit.queued_run = queued_run;
        commit.interrupted_turn_input_turn_id = interrupted_turn_input_turn_id;
        commit.interrupted_turn_input_cancellation = interrupted_turn_input_cancellation;
        commit.interrupted_turn_cancel_intent = interrupted_turn_cancel_intent;
        commit.turn_cancel_closure_settlement = turn_cancel_closure_settlement;
        if let Some((fence, root_terminal)) = drive_commit {
            commit.drive_fence = Some(Box::new(fence));
            commit.root_terminal = root_terminal.map(Box::new);
        }
        // Cancellation-intent retries are progress-fenced: every refusal
        // proves a newer durable intent revision. Refresh only that snapshot:
        // the settlement and materialized cancellation evidence are already
        // authenticated and may contain live execution enrichment (such as the
        // iteration that honoured an AfterStep request) which a raw promise
        // peek cannot reconstruct.
        let result = loop {
            commit.validate_claim_settlement(
                claim_settlement.queued.originating(),
                claim_settlement.turn_inputs.originating(),
            )?;
            match crate::store::commit_runtime_state_verified(store, commit.clone()).await {
                Ok(result) => break result,
                Err(crate::StoreError::TurnCancelIntentChanged { .. }) => {
                    let turn_id =
                        commit
                            .interrupted_turn_input_turn_id
                            .as_ref()
                            .ok_or_else(|| {
                                StoreError::Backend(
                                    "cancellation intent CAS failed without an interrupted turn id"
                                        .to_string(),
                                )
                            })?;
                    let address = crate::TurnAddress::new(&session_id, turn_id);
                    let observed = store.turn_cancel_request_intent(&address).await?;
                    commit.interrupted_turn_cancel_intent = Some(observed);
                }
                // A claim this turn restored from an earlier execution, or its
                // journaled drive, was superseded: another driver took those
                // rows, and the journal already holds this turn's answer to
                // them. Cede and commit nothing, never drop the rows and
                // commit the same words again (ADR 0069 §6, FIG-3552). The
                // refusal travels as the typed runtime error the turn's
                // caller classifies.
                Err(err) if claim_settlement.cedes(&err, current_session_lease_generation) => {
                    return Err(StoreError::TurnOutcomeMaterializationRefused {
                        error: Box::new(crate::RuntimeError::new(
                            crate::RuntimeErrorCode::AcceptedTurnInputCeded,
                            format!(
                                "rows this turn claimed in an earlier execution or in its \
                                 journaled drive were reclaimed by another driver before the \
                                 turn could commit, so another turn answers them; nothing was \
                                 committed: {err}"
                            ),
                        )),
                    });
                }
                Err(err) => return Err(err),
            }
        };
        let committed_usage_delta_identities = result.committed_usage_delta_identities.clone();
        let turn_cancel_input_outcome = result.turn_cancel_input_outcome.clone();
        state.apply_persisted_commit_result(result);
        state.mark_node_ids_persisted(persisted_node_ids);
        Ok((committed_usage_delta_identities, turn_cancel_input_outcome))
    }
}

#[cfg(test)]
mod tests;
