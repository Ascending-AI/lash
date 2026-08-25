use crate::facade_support::SessionGraphFacadeOps;
#[cfg(test)]
use crate::facade_support::SessionNodeRecordFacadeOps;
use std::sync::Arc;

use crate::session_model::SessionHistoryRecord;
use crate::store::{GraphAppend, RuntimeCommit, RuntimePersistence, StoreError};
use crate::{
    AssembledTurn, MessageSequence, PluginSession, Session, SessionPolicy, SessionReadView,
    TurnOutcome,
};

use super::turn_graph_editor::ReadProjectionDiagnostic;
use super::{RuntimeError, RuntimeErrorCode, RuntimeSessionState, TurnCommitDraft};

mod materialize;
use materialize::*;
mod accepted_commit;
pub(super) use accepted_commit::AcceptedTurnCommit;
mod execution_state;
use execution_state::*;
mod final_commit_input;
use final_commit_input::FinalCommitInput;
mod settlement;
use settlement::*;

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
        let draft_clock = Arc::clone(&clock);
        Self {
            stage: TurnCommitStage::Drafting(Box::new(TurnCommitDraft::from_state_with_clock(
                state,
                draft_clock,
                operation_scope.id(),
            ))),
            clock,
            operation_scope,
            commit_budget,
        }
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
            state.refresh_plugin_snapshots(plugins.as_ref());
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
                state.refresh_plugin_snapshots(plugins);
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
        originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
        originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
        completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
        completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
        queue_claim_generations: std::collections::HashMap<String, u64>,
        turn_input_claim_generations: std::collections::HashMap<String, u64>,
        current_session_lease_generation: Option<u64>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<String>,
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
                plugins: plugins.as_deref(),
                execution_state_update,
                agent_frame_switch_materializes,
                store: store.as_ref().map(|store| store.as_ref()),
                usage_deltas,
                outcome: &returned_turn.outcome,
                originating_queue_claims,
                originating_turn_input_claims,
                completed_queue_claims,
                completed_turn_input_claims,
                queue_claim_generations,
                turn_input_claim_generations,
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
            plugins,
            execution_state_update,
            agent_frame_switch_materializes,
            store,
            usage_deltas,
            outcome,
            originating_queue_claims,
            originating_turn_input_claims,
            completed_queue_claims,
            completed_turn_input_claims,
            queue_claim_generations,
            turn_input_claim_generations,
            current_session_lease_generation,
            enqueued_queue_batches,
            interrupted_turn_input_turn_id,
            recorded_attachment_intent_ids,
            session_execution_lease_completion,
        } = input;
        let clock = Arc::clone(&self.clock);
        let terminal_message_id = format!("m_turn_{}_assistant", self.operation_scope.id());
        let state = self.final_state_mut();
        state.apply_snapshot(returned_state);
        for delta in usage_deltas {
            crate::store::merge_token_ledger_entry_checked(
                &mut state.token_ledger,
                delta.entry.clone(),
            )?;
        }
        if let Some(plugins) = plugins {
            state.refresh_plugin_snapshots(plugins);
        }
        execution_state_update.apply(state)?;
        materialize_terminal_output(state, outcome, clock.as_ref(), &terminal_message_id);
        materialize_agent_frame_switch(
            state,
            outcome,
            clock.as_ref(),
            agent_frame_switch_materializes,
        );
        let state = self.final_state_mut();

        if let Some(store) = store {
            let graph = state.pending_graph_commit();
            let committed_attachment_ids = committed_attachment_ids(state, tool_calls);
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
                self.final_operation(),
                originating_queue_claims,
                originating_turn_input_claims,
                completed_queue_claims,
                completed_turn_input_claims,
                queue_claim_generations,
                turn_input_claim_generations,
                current_session_lease_generation,
                enqueued_queue_batches,
                interrupted_turn_input_turn_id,
                committed_attachment_ids,
                adopted_intent_rows,
                session_execution_lease_completion,
            )
            .await
        } else {
            state.discard_runtime_snapshots();
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
        operation: crate::OperationId,
        mut originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
        mut originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
        completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
        completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
        queue_claim_generations: std::collections::HashMap<String, u64>,
        turn_input_claim_generations: std::collections::HashMap<String, u64>,
        current_session_lease_generation: Option<u64>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<String>,
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
                    && let Some((_, derived)) =
                        node_id_mapping.iter().find(|(draft, _)| draft == current)
                {
                    *current = derived.clone();
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
        commit.adopted_intent_rows = adopted_intent_rows;
        if let Some(completion) = session_execution_lease_completion {
            commit = commit.releasing_session_execution_lease(completion);
        }
        commit.completed_queue_claims = completed_queue_claims;
        commit.completed_turn_input_claims = completed_turn_input_claims;
        commit.enqueued_queue_batches = enqueued_queue_batches;
        commit.interrupted_turn_input_turn_id = interrupted_turn_input_turn_id;
        let can_retry_recovered_settlement =
            current_session_lease_generation.is_some_and(|current| {
                queue_claim_generations
                    .values()
                    .chain(turn_input_claim_generations.values())
                    .any(|generation| *generation < current)
            });
        let result = if can_retry_recovered_settlement {
            loop {
                commit.validate_claim_settlement(
                    &originating_queue_claims,
                    &originating_turn_input_claims,
                )?;
                match crate::store::commit_runtime_state_verified(store, commit.clone()).await {
                    Ok(result) => break result,
                    Err(err) => {
                        let dropped = drop_superseded_recovered_queue_settlement(
                            &err,
                            &queue_claim_generations,
                            current_session_lease_generation,
                            &mut commit.completed_queue_claims,
                            &mut originating_queue_claims,
                        ) || drop_superseded_recovered_turn_input_settlement(
                            &err,
                            &turn_input_claim_generations,
                            current_session_lease_generation,
                            &mut commit.completed_turn_input_claims,
                            &mut originating_turn_input_claims,
                        );
                        if !dropped {
                            return Err(err);
                        }
                    }
                }
            }
        } else {
            commit.validate_claim_settlement(
                &originating_queue_claims,
                &originating_turn_input_claims,
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

#[cfg(test)]
mod tests {
    use lash_sansio::core_support::MessageSequenceCoreSupport;
    use lash_sansio::sync::MutexExt;

    use super::*;
    use crate::runtime::tests::helpers::{FixedAttachmentRoots, RecordingStore};
    use crate::session_model::{ConversationRecord, MessageRole, Part};
    use crate::store::SessionExecutionLeaseStore;
    use crate::{Message, SessionGraph, TokenUsage, shared_parts};
    const UNBOUNDED: crate::TurnBudget = crate::TurnBudget::Unbounded;
    fn cancelled_outcome() -> TurnOutcome {
        TurnOutcome::Stopped(crate::TurnStop::Cancelled {
            evidence: crate::TurnCancellationEvidence::internal("turn-boundary-test"),
        })
    }
    fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
        crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
    }

    fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part::text(
                format!("{id}.p0"),
                content.to_string(),
                None,
            )]),
            origin: None,
        }
    }

    fn usage_entry(source: &str, model: &str, input_tokens: i64) -> crate::TokenLedgerEntry {
        crate::TokenLedgerEntry {
            source: source.to_string(),
            model: model.to_string(),
            usage: TokenUsage {
                input_tokens,
                output_tokens: 2,
                cache_read_input_tokens: 1,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
        }
    }

    #[test]
    fn turn_draft_appends_resident_nodes_not_yet_durable() {
        let durable = text_message("durable", MessageRole::User, "already durable");
        let pending = text_message("pending", MessageRole::Assistant, "not durable yet");
        let graph = SessionGraph::from_active_read_state(&[durable, pending]);
        let durable_node_id = graph.nodes[0].node_id.clone();
        let pending_node_id = graph.nodes[1].node_id.clone();
        let mut state = state_with_graph(graph);
        let frame_node_id = state.current_frame_node_id.clone().expect("initial frame");
        state.persisted_node_ids.insert(durable_node_id);

        let draft = TurnCommitDraft::from_state_with_clock(
            state,
            Arc::new(crate::SystemClock),
            "masked-path-regression",
        );
        let GraphAppend { nodes, .. } = draft.graph_commit();
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            vec![frame_node_id.as_str(), pending_node_id.as_str()]
        );
    }

    fn attachment_ref(id: &str) -> crate::AttachmentRef {
        crate::AttachmentMeta::new(
            crate::AttachmentId::parse(id).expect("valid attachment id"),
            crate::MediaType::parse("image/png").unwrap(),
            3,
            Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
            Some("tiny".to_string()),
        )
        .as_ref()
    }

    fn test_protocol_event(kind: &str) -> crate::ProtocolEvent {
        crate::ProtocolEvent::typed(
            "test_protocol",
            serde_json::json!({
                "kind": kind,
                "payload": { "test": true },
            }),
        )
        .expect("test protocol event serializes")
    }

    fn summarize_protocol_event(event: &crate::ProtocolEvent) -> String {
        let Some(value) = event
            .decode::<serde_json::Value>("test_protocol")
            .expect("test protocol event decodes")
        else {
            return format!("protocol:{}", event.plugin_id);
        };
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        format!("protocol:{kind}")
    }

    fn persisted_event_order(graph: &SessionGraph) -> Vec<String> {
        graph
            .nodes
            .iter()
            .filter_map(|node| match node.event()? {
                crate::SessionHistoryRecord::Conversation(record) => {
                    Some(format!("message:{}", record.id))
                }
                crate::SessionHistoryRecord::Protocol(event) => {
                    Some(summarize_protocol_event(event))
                }
            })
            .collect()
    }

    fn chronological_event_order(graph: &SessionGraph) -> Vec<String> {
        let read_model = graph.read_model();
        crate::chronological::ChronologicalProjection::from_read_model(&read_model)
            .entries()
            .iter()
            .map(|entry| match &entry.payload {
                crate::chronological::ChronologicalPayload::Message(message) => {
                    format!("message:{}", message.id)
                }
                crate::chronological::ChronologicalPayload::ProtocolEvent(event) => {
                    summarize_protocol_event(event)
                }
            })
            .collect()
    }

    fn stored_graph_with_head_leaf(store: &RecordingStore) -> SessionGraph {
        let mut graph = store.session_graph.lock_recover().clone();
        graph.set_leaf_node_id(
            store
                .session_head_meta
                .lock_recover()
                .as_ref()
                .and_then(|meta| meta.leaf_node_id.clone()),
        );
        graph
    }

    fn state_with_graph(graph: SessionGraph) -> RuntimeSessionState {
        let mut state = RuntimeSessionState {
            session_id: "session-1".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED))
        };
        state.ensure_agent_frame_initialized();
        if !graph.nodes.is_empty() {
            let frame_node_id = state.current_frame_node_id.clone().expect("initial frame");
            let mut nodes = state.session_graph.nodes.clone();
            nodes.extend(graph.nodes.iter().cloned().map(|mut node| {
                if node.parent_node_id.is_none() {
                    node.parent_node_id = Some(frame_node_id.clone());
                }
                node
            }));
            state.session_graph = SessionGraph::from_nodes(nodes, graph.leaf_node_id.clone())
                .expect("turn-boundary fixture graph is valid");
            state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        }
        state
    }
    async fn leased_boundary(
        store: &RecordingStore,
        state: RuntimeSessionState,
    ) -> (TurnBoundary, crate::SessionExecutionLease) {
        crate::SessionCommitStore::admit_and_bind_session(
            store,
            &crate::SessionBinding::root(state.session_id.clone()),
        )
        .await
        .expect("admit turn-boundary test session");
        let owner = lease_owner("turn-boundary-test");
        let lease = store
            .try_claim_session_execution_lease(
                &state.session_id,
                &owner,
                "leased-boundary-executor",
                60_000,
            )
            .await
            .expect("claim test session execution lease")
            .acquired()
            .expect("test session execution lease");
        (TurnBoundary::from_state(state), lease)
    }

    #[test]
    fn agent_frame_switch_materializes_outcome_seed_without_tool_call_event() {
        let graph = SessionGraph::from_active_read_state(&[text_message(
            "u0",
            MessageRole::User,
            "old frame",
        )]);
        let mut state = state_with_graph(graph);
        state.ensure_agent_frame_initialized();
        let previous_frame_node_id = state.current_frame_node_id.clone();
        let frame_key =
            crate::FrameKey::from_caller_material("frame-2").expect("non-empty caller material");
        let seed_node = crate::SessionAppendNode::message(crate::PluginMessage::text(
            MessageRole::User,
            "seed message",
        ));
        materialize_agent_frame_switch(
            &mut state,
            &TurnOutcome::AgentFrameSwitch {
                frame_key: frame_key.clone(),
                task: "next task".to_string(),
                initial_nodes: vec![seed_node],
            },
            &crate::SystemClock,
            true,
        );
        let expected_frame_node_id =
            crate::session_graph::frame_node_id(&state.session_id, frame_key.as_str());

        assert_eq!(state.session_id, "session-1");
        assert_eq!(
            state.current_frame_node_id.as_deref(),
            Some(expected_frame_node_id.as_str())
        );
        let current = state.current_agent_frame().expect("current frame");
        assert_eq!(
            current.previous_frame_node_id.as_deref(),
            previous_frame_node_id.as_deref()
        );
        assert_eq!(
            current.reason.as_str(),
            crate::AgentFrameReason::CONTINUE_AS
        );
        let current_read = state
            .session_graph
            .read_model_for_frame(&expected_frame_node_id);
        assert_eq!(current_read.messages.len(), 1);
        assert_eq!(current_read.messages[0].parts[0].content, "seed message");
        let previous_read = state.session_graph.read_model_for_frame(
            previous_frame_node_id
                .as_deref()
                .expect("previous frame node id"),
        );
        assert_eq!(previous_read.messages.len(), 1);
        assert_eq!(previous_read.messages[0].parts[0].content, "old frame");
    }

    #[test]
    fn open_agent_frame_seeds_compaction_frame_and_is_replay_idempotent() {
        let graph = SessionGraph::from_active_read_state(&[text_message(
            "u0",
            MessageRole::User,
            "old durable frame",
        )]);
        let mut state = state_with_graph(graph);
        state.ensure_agent_frame_initialized();
        let previous_frame_node_id = state.current_frame_node_id.clone();
        let previous_frame_node_id_value = previous_frame_node_id
            .as_deref()
            .expect("current frame")
            .to_string();
        let leaf_node_id = state.session_graph.leaf_node_id.clone();
        let mut nodes = state.session_graph.nodes.clone();
        let previous = nodes
            .iter_mut()
            .find(|node| node.node_id == previous_frame_node_id_value)
            .expect("current frame node");
        let crate::SessionNodePayload::FrameOpen {
            assignment,
            protocol_turn_options,
            ..
        } = &mut previous.payload
        else {
            panic!("current frame id must identify FrameOpen");
        };
        assignment.usage_source = Some("root-assignment".to_string());
        *protocol_turn_options =
            crate::ProtocolTurnOptions::from_payload(serde_json::json!({ "mode": "test" }));
        state.protocol_turn_options = protocol_turn_options.clone();
        state.session_graph = SessionGraph::from_nodes(nodes, leaf_node_id)
            .expect("frame-compaction fixture graph is valid");
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let frame_id = "frame-compaction".to_string();
        let seed_node = crate::SessionAppendNode::message(
            crate::PluginMessage::text(MessageRole::Assistant, "Compaction summary:\nold work")
                .with_origin(crate::MessageOrigin::Plugin {
                    plugin_id: "rolling_history".to_string(),
                    transient: false,
                }),
        );

        let opened = super::super::open_agent_frame_in_state_with_clock(
            &mut state,
            crate::OpenAgentFrameRequest::new(
                frame_id.clone(),
                crate::AgentFrameReason::compaction(),
            )
            .with_initial_nodes(vec![seed_node.clone()]),
            &crate::SystemClock,
        );

        assert!(opened.opened);
        assert_eq!(
            state.current_frame_node_id.as_deref(),
            Some(opened.frame_node_id.as_str())
        );
        let current = state.current_agent_frame().expect("current frame");
        assert_eq!(current.reason.as_str(), crate::AgentFrameReason::COMPACTION);
        assert_eq!(
            current.previous_frame_node_id.as_deref(),
            previous_frame_node_id.as_deref()
        );
        assert_eq!(
            current.assignment.usage_source.as_deref(),
            Some("root-assignment")
        );
        assert_eq!(
            current.protocol_turn_options.payload,
            serde_json::json!({ "mode": "test" })
        );

        let current_read = state
            .session_graph
            .read_model_for_frame(&opened.frame_node_id);
        assert_eq!(current_read.messages.len(), 1);
        assert_eq!(
            current_read.messages[0].parts[0].content,
            "Compaction summary:\nold work"
        );
        assert!(matches!(
            current_read.messages[0].origin.as_ref(),
            Some(crate::MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == "rolling_history"
        ));

        let previous_read = state.session_graph.read_model_for_frame(
            previous_frame_node_id
                .as_deref()
                .expect("previous frame node id"),
        );
        assert_eq!(previous_read.messages.len(), 1);
        assert_eq!(
            previous_read.messages[0].parts[0].content,
            "old durable frame"
        );

        let replay = super::super::open_agent_frame_in_state_with_clock(
            &mut state,
            crate::OpenAgentFrameRequest::new(
                frame_id.clone(),
                crate::AgentFrameReason::compaction(),
            )
            .with_initial_nodes(vec![seed_node]),
            &crate::SystemClock,
        );
        assert!(!replay.opened);
        let replay_read = state
            .session_graph
            .read_model_for_frame(&replay.frame_node_id);
        assert_eq!(replay_read.messages.len(), 1);
    }

    #[test]
    fn reopening_a_previous_frame_switches_back_and_materializes_new_seed_nodes() {
        let clock = crate::SystemClock;
        let mut state = RuntimeSessionState {
            session_id: "frame-switch-back".to_string(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED))
        };
        state.ensure_agent_frame_initialized_with_clock(&clock);
        let frame_a = super::super::open_agent_frame_in_state_with_clock(
            &mut state,
            crate::OpenAgentFrameRequest::new("frame-a", crate::AgentFrameReason::new("frame-a")),
            &clock,
        );
        assert!(frame_a.opened);
        let frame_b = super::super::open_agent_frame_in_state_with_clock(
            &mut state,
            crate::OpenAgentFrameRequest::new("frame-b", crate::AgentFrameReason::new("frame-b")),
            &clock,
        );
        assert!(frame_b.opened);

        let switched = super::super::open_agent_frame_in_state_with_clock(
            &mut state,
            crate::OpenAgentFrameRequest::new("frame-a", crate::AgentFrameReason::new("frame-a"))
                .with_initial_nodes(vec![crate::SessionAppendNode::message(
                    crate::PluginMessage::text(MessageRole::Assistant, "new frame-a seed"),
                )]),
            &clock,
        );

        assert!(switched.opened);
        assert_eq!(switched.frame_node_id, frame_a.frame_node_id);
        assert_eq!(
            state.current_frame_node_id.as_deref(),
            Some(frame_a.frame_node_id.as_str())
        );
        assert_eq!(switched.initial_node_ids.len(), 1);
        assert_ne!(
            state.session_graph.leaf_node_id.as_deref(),
            Some(frame_b.frame_node_id.as_str())
        );
        assert_eq!(
            state
                .session_graph
                .nearest_frame_node_id(state.session_graph.leaf_node_id.as_deref()),
            Some(frame_a.frame_node_id.as_str())
        );
    }

    #[tokio::test]
    async fn progress_boundaries_accumulate_protocol_events_in_the_draft() {
        let user = text_message("u0", MessageRole::User, "hello");
        let assistant = text_message("a0", MessageRole::Assistant, "hi");
        let mut pipeline = TurnBoundary::from_state(state_with_graph(SessionGraph::default()));
        pipeline
            .prepared_checkpoint(
                SessionPolicy::new(UNBOUNDED),
                0,
                &MessageSequence::from_base(vec![user.clone()].into()),
                None,
            )
            .await
            .expect("prepare checkpoint in memory");
        let protocol_event =
            crate::ProtocolEvent::typed("test_protocol", serde_json::json!({"step": "started"}))
                .expect("protocol event serializes");
        let event_delta = vec![crate::SessionHistoryRecord::Protocol(protocol_event)];

        let boundary = pipeline
            .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
                policy: SessionPolicy::new(UNBOUNDED),
                turn_index: 1,
                messages: MessageSequence::from_base(vec![user, assistant].into()),
                event_delta,
                execution_state_update: ExecutionStateUpdate::Clean,
                plugins: None,
            })
            .await
            .expect("progress boundary");

        assert_eq!(boundary.protocol_events.len(), 1);
        assert_eq!(pipeline.state().turn_index, 1);
        assert_eq!(pipeline.state().head_revision, 0);
    }

    #[tokio::test]
    async fn final_commit_persists_the_complete_turn_tail_once() {
        let user = text_message("u0", MessageRole::User, "hello");
        let assistant = text_message("a0", MessageRole::Assistant, "hi");
        let trajectory = test_protocol_event("trajectory");
        let store = RecordingStore::default();
        let (mut pipeline, _lease) =
            leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
        pipeline
            .prepared_checkpoint(
                SessionPolicy::new(UNBOUNDED),
                0,
                &MessageSequence::from_base(vec![user.clone()].into()),
                None,
            )
            .await
            .expect("prepare checkpoint in memory");
        pipeline
            .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
                policy: SessionPolicy::new(UNBOUNDED),
                turn_index: 1,
                messages: MessageSequence::from_base(vec![user, assistant.clone()].into()),
                event_delta: vec![
                    crate::SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                        assistant,
                    )),
                    crate::SessionHistoryRecord::Protocol(trajectory),
                ],
                execution_state_update: ExecutionStateUpdate::Clean,
                plugins: None,
            })
            .await
            .expect("progress boundary");
        assert_eq!(*store.runtime_commit_count.lock_recover(), 0);

        let returned_state = pipeline.export_state_for_assembly();
        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                tool_calls: &[],
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect("final commit");

        assert_eq!(*store.runtime_commit_count.lock_recover(), 1);
        let stored_graph = stored_graph_with_head_leaf(&store);
        let expected = vec!["message:u0", "message:a0", "protocol:trajectory"];
        assert_eq!(persisted_event_order(&stored_graph), expected);
        assert_eq!(chronological_event_order(&stored_graph), expected);
    }

    #[tokio::test]
    async fn final_commit_rejects_a_turn_tail_over_the_node_budget_before_store_mutation() {
        let messages = (0..crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT)
            .map(|index| {
                text_message(
                    &format!("message-{index}"),
                    MessageRole::Assistant,
                    &format!("step {index}"),
                )
            })
            .collect::<Vec<_>>();
        let store = RecordingStore::default();
        let (mut pipeline, _lease) =
            leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
        pipeline
            .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
                policy: SessionPolicy::new(UNBOUNDED),
                turn_index: 1,
                messages: MessageSequence::from_base(messages.into()),
                event_delta: Vec::new(),
                execution_state_update: ExecutionStateUpdate::Clean,
                plugins: None,
            })
            .await
            .expect("build the oversized turn tail in memory");

        let returned_state = pipeline.export_state_for_assembly();
        let error = pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                tool_calls: &[],
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect_err("the final append must enforce the transaction node budget");

        assert!(matches!(
            error,
            StoreError::CommitNodeBudgetExceeded {
                node_count,
                max_nodes,
            } if node_count == crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT + 1
                && max_nodes == crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT
        ));
        assert_eq!(*store.runtime_commit_count.lock_recover(), 0);
        assert!(store.raw_graph_nodes_for_testing().is_empty());
    }
    #[test]
    fn committed_attachment_ids_merge_tool_outputs_with_message_refs() {
        let tool_ref = attachment_ref("tool-output");
        let mut state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
        let message = crate::Message {
            id: "message".to_string(),
            role: crate::MessageRole::User,
            parts: std::sync::Arc::new(vec![crate::Part::attachment_part(
                "message.p0".to_string(),
                String::new(),
                Some(crate::session_model::message::PartAttachment {
                    source: crate::AttachmentSource::stored(attachment_ref("message-ref")),
                }),
            )]),
            origin: None,
        };
        state.session_graph = crate::SessionGraph::from_active_read_state(&[message]);
        let tool_calls = vec![crate::ToolCallRecord {
            call_id: Some("call-1".to_string()),
            tool: "make_attachment".to_string(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(tool_ref),
            )),
            duration_ms: 1,
        }];
        let ids = committed_attachment_ids(&state, &tool_calls);
        assert_eq!(
            ids,
            vec![
                crate::AttachmentId::parse("message-ref").expect("valid attachment id"),
                crate::AttachmentId::parse("tool-output").expect("valid attachment id"),
            ]
        );
    }
    #[tokio::test]
    async fn replayed_exec_tool_output_is_a_gc_root_without_pending_or_message_refs() {
        let backend = crate::InMemoryAttachmentStore::new();
        let attachment = crate::AttachmentStore::put(
            &backend,
            vec![1, 2, 3],
            crate::AttachmentCreateMeta::new(
                crate::MediaType::parse("image/png").unwrap(),
                Some(crate::AttachmentTypeMetadata::image(Some(1), Some(1))),
                Some("replayed-only".to_string()),
            ),
        )
        .await
        .expect("put attachment bytes");
        let tool_calls = vec![crate::ToolCallRecord {
            call_id: Some("replayed-exec-call".to_string()),
            tool: "executor_state_only".to_string(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(attachment.clone()),
            )),
            duration_ms: 1,
        }];
        let state = RuntimeSessionState::new(crate::SessionPolicy::new(UNBOUNDED));
        let committed = committed_attachment_ids(&state, &tool_calls);
        assert_eq!(committed, vec![attachment.id.clone()]);

        let roots = FixedAttachmentRoots(committed.into_iter().collect());
        let report = crate::reclaim_unreferenced_attachments(
            &roots,
            &backend,
            crate::AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: crate::EmptyRootSetPolicy::Refuse,
            },
        )
        .await
        .expect("grace-period GC");

        assert_eq!(report.reclaimed_count, 0);
        assert_eq!(
            crate::AttachmentStore::get(&backend, &attachment.id)
                .await
                .expect("replayed exec attachment survives GC")
                .bytes,
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn final_commit_merges_usage_and_updates_persisted_graph_count() {
        let graph =
            SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "hello")]);
        let usage_entries = vec![
            usage_entry("child", "gpt", 5),
            usage_entry("turn", "gpt", 17),
        ];
        let store = RecordingStore::default();
        let (mut pipeline, _lease) = leased_boundary(&store, state_with_graph(graph.clone())).await;
        let usage = crate::store::RuntimeUsageDelta::for_operation(
            &pipeline.final_operation(),
            &usage_entries,
        )
        .expect("stage test usage");
        let returned_state = pipeline.export_state_for_assembly();

        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Replace(
                    crate::plugin::ExecutionStateSnapshot::from_root(Some(b"runtime".to_vec())),
                ),
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &usage,
                outcome: &cancelled_outcome(),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect("commit");

        assert_eq!(store.usage_deltas.lock_recover().len(), 2);
        assert_eq!(pipeline.state_mut().token_ledger.len(), 2);
        assert!(pipeline.state_mut().execution_state_snapshot().is_none());
        assert!(pipeline.state_mut().head_revision > 0);
    }

    #[tokio::test]
    async fn recovered_final_commit_drops_only_the_peer_superseded_queue_row() {
        let store = RecordingStore::default();
        let graph = SessionGraph::from_active_read_state(&[text_message(
            "u0",
            MessageRole::User,
            "recovered content",
        )]);
        let state = state_with_graph(graph);
        let (mut pipeline, predecessor_lease) = leased_boundary(&store, state).await;
        let batch = crate::QueuedWorkStore::enqueue_queued_work(
            &store,
            crate::QueuedWorkBatchDraft::new(
                "session-1",
                crate::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::QueuedWorkPayload::agent_frame_task(
                    "fig905-frame",
                    "peer-owned row",
                    None,
                )],
            ),
        )
        .await
        .expect("enqueue FIG-905 row");
        let predecessor_claim = crate::QueuedWorkStore::claim_ready_queued_work(
            &store,
            "session-1",
            &predecessor_lease.fence(),
            &predecessor_lease.owner,
            crate::QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim predecessor row")
        .claim()
        .expect("predecessor claim exists");
        assert_eq!(predecessor_claim.batches[0].batch_id, batch.batch_id);
        store
            .release_session_execution_lease(&predecessor_lease.completion())
            .await
            .expect("release crashed predecessor lease");

        let peer_owner = lease_owner("fig905-peer");
        let peer_lease = store
            .try_claim_session_execution_lease(
                "session-1",
                &peer_owner,
                "recovered-final-commit-drops-only-the-peer-superseded-queue-row-executor",
                60_000,
            )
            .await
            .expect("claim peer lease")
            .acquired()
            .expect("peer lease acquired");
        let peer_claim = crate::QueuedWorkStore::claim_ready_queued_work(
            &store,
            "session-1",
            &peer_lease.fence(),
            &peer_owner,
            crate::QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("peer reclaims row")
        .claim()
        .expect("peer claim exists");
        store
            .release_session_execution_lease(&peer_lease.completion())
            .await
            .expect("release peer lease without settling its row");

        let recovery_owner = lease_owner("fig905-recovery");
        let recovery_lease = store
            .try_claim_session_execution_lease(
                "session-1",
                &recovery_owner,
                "recovered-final-commit-drops-only-the-peer-superseded-queue-row-executor-2",
                60_000,
            )
            .await
            .expect("claim recovery lease")
            .acquired()
            .expect("recovery lease acquired");
        let returned_state = pipeline.export_state_for_assembly();
        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                tool_calls: &[],
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                originating_queue_claims: vec![predecessor_claim.completion()],
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: vec![predecessor_claim.completion()],
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::iter::once((
                    predecessor_claim.claim_id.clone(),
                    predecessor_claim.session_lease_generation,
                ))
                .collect(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: Some(recovery_lease.fencing_token),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: Some(recovery_lease.completion()),
            })
            .await
            .expect("recovered commit drops stale settlement and reaches terminal state");

        let queued = crate::QueuedWorkStore::list_queued_work(&store, "session-1")
            .await
            .expect("list peer-owned row");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].batch_id, peer_claim.batches[0].batch_id);
        let raw = store.raw_queued_work_for_testing();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].5, Some(peer_claim.session_lease_generation));
    }

    #[tokio::test]
    async fn final_commit_rejects_claim_derived_content_without_settlement() {
        let graph = SessionGraph::from_active_read_state(&[text_message(
            "claimed-input",
            MessageRole::User,
            "claimed content",
        )]);
        let queue_origin = crate::QueuedWorkCompletion {
            session_id: "session-1".to_string(),
            claim_id: "queue-claim".to_string(),
            lease_token: "queue-token".to_string(),
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["queue-batch".to_string()],
            },
        };
        let turn_input_origin = crate::TurnInputCompletion {
            session_id: "session-1".to_string(),
            claim: Some(crate::TurnInputSettlementClaim {
                claim_id: "turn-input-claim".to_string(),
                lease_token: "turn-input-token".to_string(),
            }),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["turn-input".to_string()],
                applications: Vec::new(),
            },
        };
        let store = RecordingStore::default();
        let (mut queue_pipeline, queue_lease) =
            leased_boundary(&store, state_with_graph(graph.clone())).await;
        let queue_state = queue_pipeline.export_state_for_assembly();
        let queue_err = queue_pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &queue_state,
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                tool_calls: &[],
                originating_queue_claims: vec![queue_origin],
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect_err("queue-derived content requires claim settlement");
        assert!(matches!(
            queue_err,
            StoreError::UnsettledQueuedWorkClaim { ref claim_id, .. }
                if claim_id == "queue-claim"
        ));
        store
            .release_session_execution_lease(&queue_lease.completion())
            .await
            .expect("release queue-case execution lease");

        let (mut input_pipeline, _lease) = leased_boundary(&store, state_with_graph(graph)).await;
        let input_state = input_pipeline.export_state_for_assembly();
        let input_err = input_pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &input_state,
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: vec![turn_input_origin],
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect_err("turn-input-derived content requires claim settlement");
        assert!(matches!(
            input_err,
            StoreError::UnsettledTurnInputClaim { ref claim_id, .. }
                if claim_id == "turn-input-claim"
        ));
        assert_eq!(
            *store.runtime_commit_count.lock_recover(),
            0,
            "invalid commits must be rejected before reaching persistence"
        );
    }

    #[tokio::test]
    async fn no_store_final_commit_discards_snapshots_without_touching_graph_or_usage() {
        let graph =
            SessionGraph::from_active_read_state(&[text_message("u0", MessageRole::User, "hello")]);
        let usage = vec![usage_entry("turn", "model", 5)];
        let mut state = state_with_graph(graph.clone());
        state.token_ledger = usage.clone();
        state.set_tool_state_snapshot(Some(crate::ToolState::default()));
        state.set_plugin_snapshot(Some(crate::PluginSessionSnapshot::default()));
        state.set_execution_state_snapshot(Some(b"runtime".to_vec()));
        let mut pipeline = TurnBoundary::from_state(state);
        let returned_state = pipeline.export_state_for_assembly();

        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                plugins: None,
                execution_state_update: ExecutionStateUpdate::Clean,
                agent_frame_switch_materializes: false,
                store: None,
                usage_deltas: &[],
                outcome: &cancelled_outcome(),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                queue_claim_generations: std::collections::HashMap::new(),
                turn_input_claim_generations: std::collections::HashMap::new(),
                current_session_lease_generation: None,
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                recorded_attachment_intent_ids: Default::default(),
                session_execution_lease_completion: None,
            })
            .await
            .expect("no-store commit");

        let state = pipeline.state_mut();
        assert_eq!(state.session_graph.nodes.len(), graph.nodes.len() + 1);
        assert_eq!(state.token_ledger.len(), usage.len());
        assert!(state.tool_state_snapshot().is_none());
        assert!(state.plugin_snapshot().is_none());
        assert!(state.execution_state_snapshot().is_none());
    }
}
