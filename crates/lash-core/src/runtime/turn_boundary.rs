use std::sync::Arc;

use crate::session_model::SessionHistoryRecord;
use crate::store::{GraphCommitDelta, RuntimeCommit, RuntimePersistence, StoreError};
use crate::{
    AssembledTurn, MessageSequence, PluginSession, Session, SessionPolicy, SessionReadView,
    ToolCallRecord, TurnOutcome,
};

use super::{RuntimeError, RuntimeSessionState, TurnCommitDraft, merge_ledger_entry};

mod materialize;
use materialize::*;

#[derive(Debug)]
pub(super) struct ProgressBoundaryResult {
    pub(super) protocol_events: Vec<crate::ProtocolEvent>,
}

struct ProgressBoundarySnapshot<'a> {
    policy: SessionPolicy,
    turn_index: usize,
    messages: MessageSequence,
    event_delta: Vec<SessionHistoryRecord>,
    execution_state_snapshot: Option<Option<Vec<u8>>>,
    plugins: Option<&'a PluginSession>,
}

pub(super) struct TurnBoundary {
    stage: TurnCommitStage,
    clock: Arc<dyn crate::Clock>,
    session_execution_lease: Option<crate::SessionExecutionLeaseFence>,
    operation_scope: crate::ExecutionScope,
}

/// Explicit two-phase lifecycle for a turn commit.
///
/// A pipeline starts in [`TurnCommitStage::Drafting`] while progress boundaries
/// accumulate in a mutable [`TurnCommitDraft`]. The first call that needs the
/// assembled session state transitions it (irreversibly) to
/// [`TurnCommitStage::Finalized`], and the completed turn is committed once.
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
        Self::Finalized(Box::new(FinalizedTurnCommitStage {
            state: RuntimeSessionState::default(),
        }))
    }
}

struct FinalCommitInput<'a> {
    returned_state: &'a crate::SessionSnapshot,
    tool_calls: &'a [ToolCallRecord],
    plugins: Option<&'a PluginSession>,
    execution_state_snapshot: Option<Option<Vec<u8>>>,
    store: Option<&'a (dyn RuntimePersistence + 'a)>,
    usage_deltas: &'a [crate::TokenLedgerEntry],
    outcome: &'a TurnOutcome,
    originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
    originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
    completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    interrupted_turn_input_turn_id: Option<String>,
    session_execution_lease_completion: Option<crate::SessionExecutionLeaseCompletion>,
}

enum PersistedGraphMark {
    Unchanged,
    Append(Vec<String>),
}

impl PersistedGraphMark {
    fn from_graph_commit(graph: &GraphCommitDelta) -> Self {
        match graph {
            GraphCommitDelta::Unchanged { .. } => Self::Unchanged,
            GraphCommitDelta::Append { nodes, .. } => {
                Self::Append(nodes.iter().map(|node| node.node_id.clone()).collect())
            }
        }
    }
}

impl TurnBoundary {
    #[cfg(test)]
    pub(super) fn from_state(state: RuntimeSessionState) -> Self {
        let scope = crate::ExecutionScope::turn(&state.session_id, "test-turn");
        Self::from_state_with_clock(state, Arc::new(crate::SystemClock), scope)
    }

    pub(super) fn from_state_with_clock(
        state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        operation_scope: crate::ExecutionScope,
    ) -> Self {
        let draft_clock = Arc::clone(&clock);
        Self {
            stage: TurnCommitStage::Drafting(Box::new(TurnCommitDraft::from_state_with_clock(
                state,
                draft_clock,
                operation_scope.id(),
            ))),
            clock,
            session_execution_lease: None,
            operation_scope,
        }
    }

    pub(super) fn with_session_execution_lease(
        mut self,
        lease: Option<crate::SessionExecutionLeaseFence>,
    ) -> Self {
        self.session_execution_lease = lease;
        self
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
        session: Option<&mut Session>,
    ) -> Result<(), StoreError> {
        if !crate::messages_are_prompt_resume_safe(messages.iter()) {
            return Ok(());
        }

        self.apply_prepared_messages(messages);
        let plugins = session
            .as_deref()
            .map(|session| Arc::clone(session.plugins()));
        let execution_state_snapshot = match session {
            Some(session) => Self::snapshot_dirty_execution_state(session).await,
            None => None,
        };
        let state = self.draft_mut().state_mut();
        state.policy = policy;
        state.turn_index = turn_index;
        if let Some(execution_state_snapshot) = execution_state_snapshot {
            state.set_execution_state_snapshot(execution_state_snapshot);
        }
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

        let execution_state_snapshot = Self::snapshot_dirty_execution_state(session).await;
        let plugins = Arc::clone(session.plugins());
        self.progress_boundary_with_snapshot(ProgressBoundarySnapshot {
            policy,
            turn_index,
            messages,
            event_delta,
            execution_state_snapshot,
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
            execution_state_snapshot,
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
            if let Some(execution_state_snapshot) = execution_state_snapshot {
                state.set_execution_state_snapshot(execution_state_snapshot);
            }
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
        usage_deltas: &[crate::TokenLedgerEntry],
        originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
        originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
        completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
        completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<String>,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseCompletion>,
    ) -> Result<Vec<crate::QueuedWorkBatch>, RuntimeError> {
        let (store, plugins, execution_state_snapshot) = match session {
            Some(session) => {
                let store = session.history_store();
                let execution_state_snapshot = Self::snapshot_dirty_execution_state(session).await;
                let plugins = Arc::clone(session.plugins());
                (store, Some(plugins), execution_state_snapshot)
            }
            None => (None, None, None),
        };
        let enqueued_queue_batches = self
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_turn.state,
                tool_calls: &returned_turn.tool_calls,
                plugins: plugins.as_deref(),
                execution_state_snapshot,
                store: store.as_ref().map(|store| store.as_ref()),
                usage_deltas,
                outcome: &returned_turn.outcome,
                originating_queue_claims,
                originating_turn_input_claims,
                completed_queue_claims,
                completed_turn_input_claims,
                enqueued_queue_batches,
                interrupted_turn_input_turn_id,
                session_execution_lease_completion,
            })
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        returned_turn.state = self.final_state_mut().to_snapshot();
        Ok(enqueued_queue_batches)
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
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        let FinalCommitInput {
            returned_state,
            tool_calls,
            plugins,
            execution_state_snapshot,
            store,
            usage_deltas,
            outcome,
            originating_queue_claims,
            originating_turn_input_claims,
            completed_queue_claims,
            completed_turn_input_claims,
            enqueued_queue_batches,
            interrupted_turn_input_turn_id,
            session_execution_lease_completion,
        } = input;
        let clock = Arc::clone(&self.clock);
        let terminal_message_id = format!("m_turn_{}_assistant", self.operation_scope.id());
        let state = self.final_state_mut();
        state.apply_snapshot(returned_state);
        for entry in usage_deltas.iter().cloned() {
            merge_ledger_entry(&mut state.token_ledger, entry);
        }
        if let Some(plugins) = plugins {
            state.refresh_plugin_snapshots(plugins);
        }
        if let Some(execution_state_snapshot) = execution_state_snapshot {
            state.set_execution_state_snapshot(execution_state_snapshot);
        }
        materialize_terminal_output(state, outcome, clock.as_ref(), &terminal_message_id);
        materialize_agent_frame_switch(state, outcome, clock.as_ref());
        let state = self.final_state_mut();

        if let Some(store) = store {
            let graph = state.pending_graph_commit();
            let committed_attachment_ids = committed_attachment_ids(state, tool_calls);
            self.apply_commit(
                store,
                graph,
                usage_deltas,
                Some(crate::OperationId::new(
                    self.operation_scope.clone(),
                    "final",
                )),
                originating_queue_claims,
                originating_turn_input_claims,
                completed_queue_claims,
                completed_turn_input_claims,
                enqueued_queue_batches,
                interrupted_turn_input_turn_id,
                committed_attachment_ids,
                session_execution_lease_completion,
            )
            .await
        } else {
            state.discard_runtime_snapshots();
            Ok(Vec::new())
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_commit(
        &mut self,
        store: &(dyn RuntimePersistence + '_),
        mut graph: GraphCommitDelta,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: Option<crate::OperationId>,
        originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
        originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
        completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
        completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
        enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
        interrupted_turn_input_turn_id: Option<String>,
        committed_attachment_ids: Vec<crate::AttachmentId>,
        session_execution_lease_completion: Option<crate::SessionExecutionLeaseCompletion>,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError> {
        let session_execution_lease = self.session_execution_lease.clone();
        if let Some(operation) = &operation {
            let session_id = self.state().session_id.clone();
            let node_id_mapping = graph.derive_node_ids(&session_id, operation)?;
            match &mut self.stage {
                TurnCommitStage::Drafting(draft) => {
                    draft.remap_node_ids(&session_id, &node_id_mapping)
                }
                TurnCommitStage::Finalized(finalized) => finalized
                    .state
                    .session_graph
                    .remap_node_ids(&session_id, &node_id_mapping),
            }
        }
        let state = self.state_mut();
        let mark = PersistedGraphMark::from_graph_commit(&graph);
        let mut commit =
            RuntimeCommit::persisted_state_with_graph_commit(state, graph, usage_deltas)
                .with_committed_attachments(committed_attachment_ids);
        if let Some(lease) = session_execution_lease {
            commit = commit.with_session_execution_lease(lease);
        }
        if let Some(completion) = session_execution_lease_completion {
            commit = commit.releasing_session_execution_lease(completion);
        }
        commit.completed_queue_claims = completed_queue_claims;
        commit.completed_turn_input_claims = completed_turn_input_claims;
        commit
            .validate_claim_settlement(&originating_queue_claims, &originating_turn_input_claims)?;
        commit.enqueued_queue_batches = enqueued_queue_batches;
        commit.interrupted_turn_input_turn_id = interrupted_turn_input_turn_id;
        if let Some(operation) = operation {
            let turn_commit_hash = commit.turn_commit_hash()?;
            commit.turn_commit = Some(crate::RuntimeTurnCommitStamp::new(
                commit.session_id.clone(),
                operation,
                turn_commit_hash,
            ));
        }
        let result = crate::store::commit_runtime_state_verified(store, commit).await?;
        let enqueued_queue_batches = result.enqueued_queue_batches.clone();
        state.apply_persisted_commit_result(result);
        let persisted_node_ids = match &mark {
            PersistedGraphMark::Unchanged => Vec::new(),
            PersistedGraphMark::Append(node_ids) => node_ids.clone(),
        };
        state.mark_node_ids_persisted(persisted_node_ids);
        if let TurnCommitStage::Drafting(draft) = &mut self.stage {
            match mark {
                PersistedGraphMark::Unchanged => {}
                PersistedGraphMark::Append(node_ids) => {
                    draft.mark_node_ids_persisted(node_ids);
                }
            }
        }
        Ok(enqueued_queue_batches)
    }

    async fn snapshot_dirty_execution_state(session: &mut Session) -> Option<Option<Vec<u8>>> {
        let code_executor = session.plugins().code_executor()?;
        if !code_executor.execution_state_dirty() {
            return None;
        }
        let session_id = session.session_id().to_string();
        match code_executor
            .snapshot_execution_state(crate::plugin::ProtocolSessionContext::new(
                session,
                &session_id,
            ))
            .await
        {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                tracing::warn!("failed to snapshot dirty execution state: {err}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::helpers::RecordingStore;
    use crate::session_model::{ConversationRecord, MessageRole, Part, PartKind, PruneState};
    use crate::store::SessionExecutionLeaseStore;
    use crate::{Message, SessionGraph, TokenUsage, shared_parts};

    struct FixedAttachmentRoots(std::collections::BTreeSet<crate::AttachmentId>);

    #[async_trait::async_trait]
    impl crate::AttachmentRootSet for FixedAttachmentRoots {
        async fn live_attachment_refs(
            &self,
            _intent_grace_cutoff_epoch_ms: u64,
        ) -> Result<std::collections::BTreeSet<crate::AttachmentId>, crate::StoreError> {
            Ok(self.0.clone())
        }
    }

    fn lease_owner(owner_id: &str) -> crate::LeaseOwnerIdentity {
        crate::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
    }

    fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: content.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
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
        state.persisted_node_ids.insert(durable_node_id);

        let draft = TurnCommitDraft::from_state_with_clock(
            state,
            Arc::new(crate::SystemClock),
            "masked-path-regression",
        );
        let GraphCommitDelta::Append { nodes, .. } = draft.graph_commit() else {
            panic!("the non-durable resident node must remain appendable");
        };
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            vec![pending_node_id.as_str()]
        );
    }

    fn attachment_ref(id: &str) -> crate::AttachmentRef {
        crate::AttachmentMeta::new(
            crate::AttachmentId::new(id),
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
        let mut graph = store.session_graph.lock().expect("lock graph").clone();
        graph.set_leaf_node_id(
            store
                .session_head_meta
                .lock()
                .expect("lock head meta")
                .as_ref()
                .and_then(|meta| meta.leaf_node_id.clone()),
        );
        graph
    }

    fn state_with_graph(graph: SessionGraph) -> RuntimeSessionState {
        RuntimeSessionState {
            session_id: "session-1".to_string(),
            session_graph: graph,
            ..RuntimeSessionState::default()
        }
    }

    async fn leased_boundary(
        store: &RecordingStore,
        state: RuntimeSessionState,
    ) -> (TurnBoundary, crate::SessionExecutionLease) {
        let owner = lease_owner("turn-boundary-test");
        let lease = store
            .try_claim_session_execution_lease(&state.session_id, &owner, 60_000)
            .await
            .expect("claim test session execution lease")
            .acquired()
            .expect("test session execution lease");
        (
            TurnBoundary::from_state(state).with_session_execution_lease(Some(lease.fence())),
            lease,
        )
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
        let previous_frame_id = state.current_agent_frame_id.clone();
        let frame_id = "frame-2".to_string();
        let seed_node = crate::SessionAppendNode::message(crate::PluginMessage::text(
            MessageRole::User,
            "seed message",
        ));
        materialize_agent_frame_switch(
            &mut state,
            &TurnOutcome::AgentFrameSwitch {
                frame_id: frame_id.clone(),
                task: "next task".to_string(),
                initial_nodes: vec![seed_node],
            },
            &crate::SystemClock,
        );

        assert_eq!(state.session_id, "session-1");
        assert_eq!(state.current_agent_frame_id, frame_id);
        let current = state.current_agent_frame().expect("current frame");
        assert_eq!(
            current.previous_frame_id.as_deref(),
            Some(previous_frame_id.as_str())
        );
        assert_eq!(
            current.reason.as_str(),
            crate::AgentFrameReason::CONTINUE_AS
        );
        let current_read = state
            .session_graph
            .read_model_for_agent_frame(&frame_id, false);
        assert_eq!(current_read.messages.len(), 1);
        assert_eq!(current_read.messages[0].parts[0].content, "seed message");
        let previous_read = state
            .session_graph
            .read_model_for_agent_frame(&previous_frame_id, true);
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
        let previous_frame_id = state.current_agent_frame_id.clone();
        let previous = state
            .current_agent_frame_mut()
            .expect("current frame before compaction");
        previous.assignment.usage_source = Some("root-assignment".to_string());
        previous.protocol_turn_options =
            crate::ProtocolTurnOptions::from_payload(serde_json::json!({ "mode": "test" }));

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
        assert_eq!(state.current_agent_frame_id, frame_id);
        let current = state.current_agent_frame().expect("current frame");
        assert_eq!(current.reason.as_str(), crate::AgentFrameReason::COMPACTION);
        assert_eq!(
            current.previous_frame_id.as_deref(),
            Some(previous_frame_id.as_str())
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
            .read_model_for_agent_frame(&frame_id, false);
        assert_eq!(current_read.messages.len(), 1);
        assert_eq!(
            current_read.messages[0].parts[0].content,
            "Compaction summary:\nold work"
        );
        assert!(matches!(
            current_read.messages[0].origin.as_ref(),
            Some(crate::MessageOrigin::Plugin { plugin_id, .. }) if plugin_id == "rolling_history"
        ));

        let previous_read = state
            .session_graph
            .read_model_for_agent_frame(&previous_frame_id, true);
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
            .read_model_for_agent_frame(&frame_id, false);
        assert_eq!(replay_read.messages.len(), 1);
    }

    #[tokio::test]
    async fn progress_boundaries_update_the_draft_without_store_mutation() {
        let user = text_message("u0", MessageRole::User, "hello");
        let assistant = text_message("a0", MessageRole::Assistant, "hi");
        let store = RecordingStore::default();
        let (mut pipeline, _lease) =
            leased_boundary(&store, state_with_graph(SessionGraph::default())).await;
        pipeline
            .prepared_checkpoint(
                SessionPolicy::default(),
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
                policy: SessionPolicy::default(),
                turn_index: 1,
                messages: MessageSequence::from_base(vec![user, assistant].into()),
                event_delta,
                execution_state_snapshot: None,
                plugins: None,
            })
            .await
            .expect("progress boundary");

        assert_eq!(boundary.protocol_events.len(), 1);
        assert_eq!(pipeline.state().turn_index, 1);
        assert_eq!(
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
            0
        );
        assert!(store.raw_graph_nodes_for_testing().is_empty());
        assert!(pipeline.state().head_revision.is_none());
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
                SessionPolicy::default(),
                0,
                &MessageSequence::from_base(vec![user.clone()].into()),
                None,
            )
            .await
            .expect("prepare checkpoint in memory");
        pipeline
            .progress_boundary_with_snapshot(ProgressBoundarySnapshot {
                policy: SessionPolicy::default(),
                turn_index: 1,
                messages: MessageSequence::from_base(vec![user, assistant.clone()].into()),
                event_delta: vec![
                    crate::SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                        assistant,
                    )),
                    crate::SessionHistoryRecord::Protocol(trajectory),
                ],
                execution_state_snapshot: None,
                plugins: None,
            })
            .await
            .expect("progress boundary");
        assert_eq!(
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
            0
        );

        let returned_state = pipeline.export_state_for_assembly();
        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                tool_calls: &[],
                plugins: None,
                execution_state_snapshot: None,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                session_execution_lease_completion: None,
            })
            .await
            .expect("final commit");

        assert_eq!(
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
            1
        );
        let stored_graph = stored_graph_with_head_leaf(&store);
        let expected = vec!["message:u0", "message:a0", "protocol:trajectory"];
        assert_eq!(persisted_event_order(&stored_graph), expected);
        assert_eq!(chronological_event_order(&stored_graph), expected);
    }

    #[tokio::test]
    async fn final_commit_rejects_a_turn_tail_over_the_node_budget_before_store_mutation() {
        let messages = (0..=crate::RuntimeCommit::MAX_COMMIT_NODE_COUNT)
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
                policy: SessionPolicy::default(),
                turn_index: 1,
                messages: MessageSequence::from_base(messages.into()),
                event_delta: Vec::new(),
                execution_state_snapshot: None,
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
                execution_state_snapshot: None,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
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
        assert_eq!(
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
            0
        );
        assert!(store.raw_graph_nodes_for_testing().is_empty());
    }
    #[test]
    fn committed_attachment_ids_merge_tool_outputs_with_message_refs() {
        let tool_ref = attachment_ref("tool-output");
        let mut state = RuntimeSessionState::default();
        let message = crate::Message {
            id: "message".to_string(),
            role: crate::MessageRole::User,
            parts: std::sync::Arc::new(vec![crate::Part {
                id: "message.p0".to_string(),
                kind: crate::PartKind::Attachment,
                content: String::new(),
                attachment: Some(crate::session_model::message::PartAttachment {
                    source: crate::AttachmentSource::stored(attachment_ref("message-ref")),
                }),
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: crate::PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        };
        state.session_graph = crate::SessionGraph::from_active_read_state(&[message]);
        let tool_calls = vec![crate::ToolCallRecord {
            call_id: Some("call-1".to_string()),
            tool: "make_attachment".to_string(),
            args: serde_json::json!({}),
            output: crate::ToolCallOutput::success(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(tool_ref),
            )),
            duration_ms: 1,
        }];
        let ids = committed_attachment_ids(&state, &tool_calls);
        assert_eq!(
            ids,
            vec![
                crate::AttachmentId::new("message-ref"),
                crate::AttachmentId::new("tool-output"),
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
            output: crate::ToolCallOutput::success(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(attachment.clone()),
            )),
            duration_ms: 1,
        }];
        let committed = committed_attachment_ids(&RuntimeSessionState::default(), &tool_calls);
        assert_eq!(committed, vec![attachment.id.clone()]);

        let roots = FixedAttachmentRoots(committed.into_iter().collect());
        let report = crate::reclaim_unreferenced_attachments(&roots, &backend, 0)
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
        let usage = vec![
            usage_entry("child", "gpt", 5),
            usage_entry("turn", "gpt", 17),
        ];
        let store = RecordingStore::default();
        let (mut pipeline, _lease) = leased_boundary(&store, state_with_graph(graph.clone())).await;
        let returned_state = pipeline.export_state_for_assembly();

        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                plugins: None,
                execution_state_snapshot: Some(Some(b"runtime".to_vec())),
                store: Some(&store),
                usage_deltas: &usage,
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                session_execution_lease_completion: None,
            })
            .await
            .expect("commit");

        assert_eq!(
            store.usage_deltas.lock().expect("lock usage deltas").len(),
            2
        );
        assert_eq!(pipeline.state_mut().token_ledger.len(), 2);
        assert!(pipeline.state_mut().execution_state_snapshot().is_none());
        assert!(pipeline.state_mut().head_revision.is_some());
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
            batch_ids: vec!["queue-batch".to_string()],
        };
        let turn_input_origin = crate::TurnInputCompletion {
            session_id: "session-1".to_string(),
            claim_id: "turn-input-claim".to_string(),
            lease_token: "turn-input-token".to_string(),
            input_ids: vec!["turn-input".to_string()],
            applications: Vec::new(),
        };
        let store = RecordingStore::default();
        let (mut queue_pipeline, _lease) =
            leased_boundary(&store, state_with_graph(graph.clone())).await;
        let queue_state = queue_pipeline.export_state_for_assembly();
        let queue_err = queue_pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &queue_state,
                plugins: None,
                execution_state_snapshot: None,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                tool_calls: &[],
                originating_queue_claims: vec![queue_origin],
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                session_execution_lease_completion: None,
            })
            .await
            .expect_err("queue-derived content requires claim settlement");
        assert!(matches!(
            queue_err,
            StoreError::UnsettledQueuedWorkClaim { ref claim_id, .. }
                if claim_id == "queue-claim"
        ));

        let (mut input_pipeline, _lease) = leased_boundary(&store, state_with_graph(graph)).await;
        let input_state = input_pipeline.export_state_for_assembly();
        let input_err = input_pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &input_state,
                plugins: None,
                execution_state_snapshot: None,
                store: Some(&store),
                usage_deltas: &[],
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: vec![turn_input_origin],
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
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
            *store
                .runtime_commit_count
                .lock()
                .expect("lock runtime commit count"),
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
        state.tool_state_snapshot = Some(crate::ToolState::default());
        state.plugin_snapshot = Some(crate::PluginSessionSnapshot::default());
        state.execution_state_snapshot = Some(b"runtime".to_vec());
        let mut pipeline = TurnBoundary::from_state(state);
        let returned_state = pipeline.export_state_for_assembly();

        pipeline
            .final_commit_with_snapshots(FinalCommitInput {
                returned_state: &returned_state,
                plugins: None,
                execution_state_snapshot: None,
                store: None,
                usage_deltas: &[],
                outcome: &TurnOutcome::Stopped(crate::TurnStop::Cancelled),
                tool_calls: &[],
                originating_queue_claims: Vec::new(),
                originating_turn_input_claims: Vec::new(),
                completed_queue_claims: Vec::new(),
                completed_turn_input_claims: Vec::new(),
                enqueued_queue_batches: Vec::new(),
                interrupted_turn_input_turn_id: None,
                session_execution_lease_completion: None,
            })
            .await
            .expect("no-store commit");

        let state = pipeline.state_mut();
        assert_eq!(state.session_graph.nodes.len(), graph.nodes.len());
        assert_eq!(state.token_ledger.len(), usage.len());
        assert!(state.tool_state_snapshot.is_none());
        assert!(state.plugin_snapshot.is_none());
        assert!(state.execution_state_snapshot.is_none());
    }
}
