use super::turn_graph_editor::ReadProjectionDiagnostic;
use super::{
    RuntimeError, RuntimeErrorCode, RuntimeSessionState, TurnCommitDraft, TurnGraphAppendDraft,
};
use crate::TurnId;
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
type FinalCommitResult = Result<
    (
        Vec<crate::QueuedWorkBatch>,
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
    stage: TurnCommitStage,
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

impl TurnCommitStage {
    /// Throwaway value used to move out of `&mut self` during finalization.
    fn placeholder() -> Self {
        let state = RuntimeSessionState::new(SessionPolicy::new(crate::TurnBudget::Unbounded));
        Self::Finalized(Box::new(FinalizedTurnCommitStage { state }))
    }
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
            stage: TurnCommitStage::Drafting(Box::new(
                TurnCommitDraft::from_state_with_graph_appends(
                    state,
                    draft_clock,
                    operation_scope.id(),
                    graph_appends.clone(),
                ),
            )),
            clock,
            operation_scope,
            commit_budget,
            graph_appends,
            protocol_terminal_output: materialize::ProtocolTerminalOutput::default(),
        }
    }

    /// Records the ids of the assistant messages the protocol driver appended
    /// after its final model call: the reply the driver materialized itself.
    pub(super) fn record_protocol_terminal_output(
        &mut self,
        message_ids: impl IntoIterator<Item = String>,
    ) {
        self.protocol_terminal_output.record(message_ids);
    }

    pub(super) fn graph_appends(&self) -> &TurnGraphAppendDraft {
        &self.graph_appends
    }

    pub(super) fn state_mut(&mut self) -> &mut RuntimeSessionState {
        match &mut self.stage {
            TurnCommitStage::Drafting(draft) => draft.state_mut(),
            TurnCommitStage::Finalized(finalized) => &mut finalized.state,
        }
    }
    pub(super) fn state(&self) -> &RuntimeSessionState {
        match &self.stage {
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
        current_session_lease_generation: Option<u64>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<TurnId>,
        recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseAuthority>,
    ) -> Result<AcceptedTurnCommit, StoreError> {
        let agent_frame_switch_materializes = match &returned_turn.outcome {
            TurnOutcome::AgentFrameSwitch { frame_key, .. } => agent_frame_switch_materializes(
                &self.state().session_id,
                frame_key,
                self.state().current_frame_node_id.as_deref(),
            ),
            _ => false,
        };
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
                current_session_lease_generation,
                enqueued_queue_batches,
                interrupted_turn_input_turn_id,
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
        let enqueued_queue_batches = commit_result?;
        returned_turn.state = self.final_state_mut().to_snapshot();
        returned_turn.turn_cancel_input_outcome = enqueued_queue_batches.2;
        Ok(AcceptedTurnCommit::new(
            enqueued_queue_batches.0,
            enqueued_queue_batches.1,
        ))
    }

    pub(super) fn into_final_state(self) -> RuntimeSessionState {
        match self.stage {
            TurnCommitStage::Drafting(draft) => (*draft).into_final_state(),
            TurnCommitStage::Finalized(finalized) => finalized.state,
        }
    }

    fn draft_ref(&self) -> &TurnCommitDraft {
        match &self.stage {
            TurnCommitStage::Drafting(draft) => draft.as_ref(),
            TurnCommitStage::Finalized(_) => {
                panic!("turn commit draft is unavailable after final state materialization")
            }
        }
    }

    fn draft_mut(&mut self) -> &mut TurnCommitDraft {
        match &mut self.stage {
            TurnCommitStage::Drafting(draft) => draft.as_mut(),
            TurnCommitStage::Finalized(_) => {
                panic!("turn commit draft is unavailable after final state materialization")
            }
        }
    }

    fn final_state_mut(&mut self) -> &mut RuntimeSessionState {
        self.stage = match std::mem::replace(&mut self.stage, TurnCommitStage::placeholder()) {
            TurnCommitStage::Drafting(draft) => {
                TurnCommitStage::Finalized(Box::new(FinalizedTurnCommitStage {
                    state: (*draft).into_final_state(),
                }))
            }
            finalized => finalized,
        };
        match &mut self.stage {
            TurnCommitStage::Finalized(finalized) => &mut finalized.state,
            TurnCommitStage::Drafting(_) => unreachable!("stage was just finalized"),
        }
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
            current_session_lease_generation,
            enqueued_queue_batches,
            interrupted_turn_input_turn_id,
            recorded_attachment_intent_ids,
            session_execution_lease_completion,
        } = input;
        let clock = Arc::clone(&self.clock);
        let graph_appends = self.graph_appends.clone();
        let protocol_terminal_output = self.protocol_terminal_output.clone();
        let turn_id = crate::TurnId::from(self.operation_scope.id());
        let terminal_message_id = format!("m_turn_{turn_id}_assistant");
        let state = self.final_state_mut();
        state.apply_snapshot(returned_state);
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
        materialize_agent_frame_switch(
            state,
            outcome,
            clock.as_ref(),
            agent_frame_switch_materializes,
        )
        .map_err(|error| StoreError::TurnOutcomeMaterializationRefused {
            error: Box::new(error),
        })?;
        // Appends recorded after finalization (finalize-turn hooks) land here,
        // after everything the turn materialized.
        graph_appends.fold_into_final_state(state);
        let state = self.final_state_mut();

        if let Some(store) = store {
            let graph = state.pending_graph_commit();
            let committed_attachment_ids = committed_attachment_ids(state, tool_calls, omitted);
            let adopted_intent_rows = committed_attachment_ids
                .iter()
                .cloned()
                .chain(recorded_attachment_intent_ids)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                .try_into()
                .unwrap_or(u64::MAX);
            self.apply_commit(
                store,
                graph,
                usage_deltas,
                failure_evidence,
                self.final_operation(),
                claim_settlement,
                current_session_lease_generation,
                enqueued_queue_batches,
                interrupted_turn_input_turn_id,
                committed_attachment_ids,
                adopted_intent_rows,
                session_execution_lease_completion,
            )
            .await
        } else {
            // No store will ever rehydrate this commit: the accepted execution
            // stays resident for the next same-frame restore (FIG-2521).
            state.discard_runtime_snapshots_retaining_accepted_execution();
            Ok((
                Vec::new(),
                usage_deltas
                    .iter()
                    .map(|delta| delta.identity.clone())
                    .collect(),
                Default::default(),
            ))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_commit(
        &mut self,
        store: &(dyn RuntimePersistence + '_),
        mut graph: GraphAppend,
        usage_deltas: &[crate::store::RuntimeUsageDelta],
        failure_evidence: &[crate::TurnFailureEvidence],
        operation: crate::OperationId,
        mut claim_settlement: TurnClaimSettlement,
        current_session_lease_generation: Option<u64>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<TurnId>,
        committed_attachment_ids: Vec<crate::AttachmentId>,
        adopted_intent_rows: u64,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseAuthority>,
    ) -> FinalCommitResult {
        let session_id = self.state().session_id.clone();
        let node_id_mapping = graph.derive_node_ids(&session_id, &operation)?;
        match &mut self.stage {
            TurnCommitStage::Drafting(draft) => draft.remap_node_ids(&session_id, &node_id_mapping),
            TurnCommitStage::Finalized(finalized) => {
                finalized
                    .state
                    .session_graph
                    .remap_node_ids(&session_id, &node_id_mapping);
                if let Some(current) = finalized.state.current_frame_node_id.as_mut()
                    && let Some((_, derived)) = node_id_mapping
                        .iter()
                        .find(|(draft, _)| draft == current.as_str())
                {
                    *current = crate::FrameNodeId::new(derived.clone())
                        .expect("derived graph node identities are non-empty");
                }
                finalized.state.agent_frames = finalized
                    .state
                    .session_graph
                    .agent_frame_records(&session_id);
            }
        }
        let commit_budget = self.commit_budget;
        let state = self.state_mut();
        let persisted_node_ids = graph
            .nodes
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
        if let Some(completion) = session_execution_lease_completion {
            commit = commit.releasing_session_execution_lease(completion);
        }
        commit.completed_queue_claims = claim_settlement.queued.completions.clone();
        commit.completed_turn_input_claims = claim_settlement.turn_inputs.completions.clone();
        commit.enqueued_queue_batches = enqueued_queue_batches;
        commit.interrupted_turn_input_turn_id = interrupted_turn_input_turn_id;
        let can_retry_recovered_settlement =
            claim_settlement.has_recovered(current_session_lease_generation);
        let result = if can_retry_recovered_settlement {
            // Each retry can remove one stale row. Permit at most one retry
            // per original row, followed by the final commit attempt.
            let mut retry_budget = RecoveredSettlementBudget(
                commit
                    .completed_queue_claims
                    .iter()
                    .map(|claim| claim.batch_ids.len())
                    .sum::<usize>()
                    .saturating_add(
                        commit
                            .completed_turn_input_claims
                            .iter()
                            .map(|claim| claim.input_ids.len())
                            .sum::<usize>(),
                    ),
            );
            loop {
                commit.validate_claim_settlement(
                    claim_settlement.queued.originating(),
                    claim_settlement.turn_inputs.originating(),
                )?;
                match crate::store::commit_runtime_state_verified(store, commit.clone()).await {
                    Ok(result) => break result,
                    Err(err) => {
                        if !retry_budget.consume() {
                            return Err(err);
                        }
                        let dropped = claim_settlement
                            .drop_superseded(&err, current_session_lease_generation);
                        commit.completed_queue_claims = claim_settlement.queued.completions.clone();
                        commit.completed_turn_input_claims =
                            claim_settlement.turn_inputs.completions.clone();
                        if !dropped {
                            return Err(err);
                        }
                    }
                }
            }
        } else {
            commit.validate_claim_settlement(
                claim_settlement.queued.originating(),
                claim_settlement.turn_inputs.originating(),
            )?;
            crate::store::commit_runtime_state_verified(store, commit).await?
        };
        let enqueued_queue_batches = result.enqueued_queue_batches.clone();
        let committed_usage_delta_identities = result.committed_usage_delta_identities.clone();
        let turn_cancel_input_outcome = result.turn_cancel_input_outcome.clone();
        state.apply_persisted_commit_result(result);
        state.mark_node_ids_persisted(persisted_node_ids.clone());
        if let TurnCommitStage::Drafting(draft) = &mut self.stage {
            draft.mark_node_ids_persisted(persisted_node_ids);
        }
        Ok((
            enqueued_queue_batches,
            committed_usage_delta_identities,
            turn_cancel_input_outcome,
        ))
    }
}

/// A recovered settlement can retry once per originally claimed row.
struct RecoveredSettlementBudget(usize);

impl RecoveredSettlementBudget {
    fn consume(&mut self) -> bool {
        let Some(remaining) = self.0.checked_sub(1) else {
            return false;
        };
        self.0 = remaining;
        true
    }
}

#[cfg(test)]
mod tests;
