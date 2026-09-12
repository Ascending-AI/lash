use crate::SessionId;
#[cfg(test)]
use crate::facade_support::SessionGraphFacadeOps;
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use crate::session_model::SessionHistoryRecord;
use crate::{MessageSequence, SessionReadView};

use super::RuntimeSessionState;
use super::state::{
    append_session_nodes_to_state_with_clock, boundary_operation, session_append_node_drafts,
};
use super::turn_graph_editor::TurnGraphEditor;

/// One `SessionGraphService::append_session_nodes` request recorded against a
/// running turn's commit draft.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct RecordedTurnGraphAppend {
    /// Draft namespace the nodes are minted in: the append's boundary
    /// operation storage key, so the ids handed to the caller and the ids the
    /// turn commits are the same.
    draft_namespace: String,
    nodes: Vec<crate::SessionAppendNode>,
    identity: crate::store::AppendRequestIdentity,
    outcome: crate::AppendSessionNodesOutcome,
}

#[derive(Debug)]
struct TurnGraphAppendDraftInner {
    /// Node ids on the resident active path when the turn began, plus every
    /// node minted by a recorded append: the set an ancestor requirement is
    /// resolved against.
    active_node_ids: HashSet<String>,
    leaf_node_id: Option<String>,
    recorded: Vec<RecordedTurnGraphAppend>,
    /// Prefix of `recorded` already folded into the turn's final state.
    applied: usize,
}

/// In-turn `SessionGraphService::append_session_nodes` requests on the current
/// session are queued into the turn's commit draft instead of committing on
/// their own.
///
/// Every turn-scoped `RuntimeSessionServices` instance records into the same
/// handle; turn-scoped read snapshots overlay the recorded nodes so in-turn
/// readers see them immediately. The turn draft folds the queue at the next
/// boundary it applies (`prepared_checkpoint`, `progress_boundary`, or the
/// final commit), after the messages that boundary carries, so an append is
/// ordered behind the history that existed when the boundary ran and later
/// messages parent after it. A boundary skipped because its messages are not
/// prompt-resume-safe leaves the queue untouched. Draft ids are remapped to
/// durable ids by the turn's commit like every other draft node.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct TurnGraphAppendDraft {
    inner: Arc<StdMutex<TurnGraphAppendDraftInner>>,
    clock: Arc<dyn crate::Clock>,
}

impl TurnGraphAppendDraft {
    /// Opens a draft against the resident state a physical turn starts from.
    pub(in crate::runtime) fn from_resident_state(
        state: &RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        use crate::facade_support::SessionGraphFacadeOps;
        let active_node_ids = state
            .session_graph
            .active_path_nodes()
            .into_iter()
            .map(|node| node.node_id.clone())
            .collect();
        Self {
            inner: Arc::new(StdMutex::new(TurnGraphAppendDraftInner {
                active_node_ids,
                leaf_node_id: state.session_graph.leaf_node_id.clone(),
                recorded: Vec::new(),
                applied: 0,
            })),
            clock,
        }
    }

    /// Records an append and answers it the way a durable append would: a
    /// replayed identity returns the first outcome, a reused operation id
    /// with a different request is a typed conflict, and an ancestor that is
    /// not on the active path is a stale branch.
    pub(in crate::runtime) fn record(
        &self,
        session_id: &SessionId,
        request: &crate::AppendSessionNodesRequest,
    ) -> Result<crate::AppendSessionNodesOutcome, crate::PluginError> {
        let operation =
            boundary_operation(session_id, &request.operation_id, "append-session-nodes");
        let draft_namespace = operation
            .storage_key()
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let identity = crate::RuntimeTurnCommitStamp::append_session_nodes(
            operation,
            request.requires_ancestor_node_id.as_deref(),
            &request.nodes,
        )
        .map_err(|err| crate::PluginError::Session(err.to_string()))?
        .append_request_identity;
        let mut inner = self.inner.lock_recover();
        if let Some(existing) = inner
            .recorded
            .iter()
            .find(|recorded| recorded.draft_namespace == draft_namespace)
        {
            if existing.identity == identity {
                return Ok(existing.outcome.clone());
            }
            return Err(crate::PluginError::AppendOperationIdentityConflict {
                session_id: SessionId::from(session_id.to_string()),
                operation_key: draft_namespace,
            });
        }
        if let Some(required_node_id) = request.requires_ancestor_node_id.as_deref()
            && !inner.active_node_ids.contains(required_node_id)
        {
            return Ok(crate::AppendSessionNodesOutcome::StaleBranch {
                required_node_id: required_node_id.to_string(),
            });
        }
        let node_ids = (0..request.nodes.len() as u64)
            .map(|ordinal| crate::session_graph::draft_node_id(&draft_namespace, ordinal))
            .collect::<Vec<_>>();
        if let Some(leaf) = node_ids.last() {
            inner.leaf_node_id = Some(leaf.clone());
        }
        inner.active_node_ids.extend(node_ids.iter().cloned());
        let outcome = crate::AppendSessionNodesOutcome::Appended {
            node_ids,
            leaf_node_id: inner.leaf_node_id.clone().unwrap_or_default(),
        };
        inner.recorded.push(RecordedTurnGraphAppend {
            draft_namespace,
            nodes: request.nodes.clone(),
            identity,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Overlays every recorded append on a turn-scoped read snapshot.
    pub(in crate::runtime) fn overlay_on_read_state(&self, state: &mut RuntimeSessionState) {
        let recorded = self.inner.lock_recover().recorded.clone();
        apply_recorded_appends(state, &recorded, self.clock.as_ref());
    }

    /// Folds the appends recorded since the previous fold into `state`, after
    /// whatever nodes `state` already holds.
    pub(in crate::runtime) fn fold_into_final_state(&self, state: &mut RuntimeSessionState) {
        let pending = self.take_pending();
        apply_recorded_appends(state, &pending, self.clock.as_ref());
    }

    /// Drains the appends recorded since the previous fold.
    fn take_pending(&self) -> Vec<RecordedTurnGraphAppend> {
        let mut inner = self.inner.lock_recover();
        let pending = inner.recorded[inner.applied..].to_vec();
        inner.applied = inner.recorded.len();
        pending
    }
}

fn apply_recorded_appends(
    state: &mut RuntimeSessionState,
    appends: &[RecordedTurnGraphAppend],
    clock: &dyn crate::Clock,
) {
    for append in appends {
        let node_ids = append_session_nodes_to_state_with_clock(
            state,
            &append.nodes,
            &append.draft_namespace,
            clock,
        );
        debug_assert!(
            matches!(
                &append.outcome,
                crate::AppendSessionNodesOutcome::Appended { node_ids: recorded, .. }
                    if *recorded == node_ids
            ),
            "a recorded in-turn append mints the ids it answered with"
        );
    }
}

#[derive(Debug)]
pub(super) struct TurnCommitDraft {
    graph: TurnGraphEditor,
    state: RuntimeSessionState,
    graph_appends: TurnGraphAppendDraft,
}

impl TurnCommitDraft {
    #[cfg(test)]
    pub(super) fn from_state_with_clock(
        state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        draft_namespace: &str,
    ) -> Self {
        let graph_appends = TurnGraphAppendDraft::from_resident_state(&state, Arc::clone(&clock));
        Self::from_state_with_graph_appends(state, clock, draft_namespace, graph_appends)
    }

    pub(super) fn from_state_with_graph_appends(
        mut state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        draft_namespace: &str,
        graph_appends: TurnGraphAppendDraft,
    ) -> Self {
        state.ensure_agent_frame_initialized_with_clock(clock.as_ref());
        let base_graph = Arc::new(std::mem::take(&mut state.session_graph));
        let base_read_model = base_graph
            .read_model(state.current_frame_node_id.as_ref())
            .expect("runtime current frame must resolve in its validated session graph");
        let persisted_node_ids = std::mem::take(&mut state.persisted_node_ids);
        let graph = TurnGraphEditor::new(
            base_graph,
            base_read_model,
            state.current_frame_node_id.clone(),
            draft_namespace,
            clock,
            persisted_node_ids,
        );
        Self {
            graph,
            state,
            graph_appends,
        }
    }

    pub(super) fn state_mut(&mut self) -> &mut RuntimeSessionState {
        &mut self.state
    }

    pub(super) fn state(&self) -> &RuntimeSessionState {
        &self.state
    }

    pub(super) fn active_events(&self) -> Arc<Vec<SessionHistoryRecord>> {
        self.graph.read_model().active_events
    }

    pub(super) fn message_sequence(&self) -> MessageSequence {
        self.graph.message_sequence()
    }

    pub(super) fn take_projection_diagnostics(
        &mut self,
    ) -> Vec<super::turn_graph_editor::ReadProjectionDiagnostic> {
        self.graph.take_projection_diagnostics()
    }

    /// Applies one boundary: the boundary's messages first, then every
    /// in-turn append queued since the previous boundary.
    pub(super) fn apply_prepared_messages(&mut self, messages: &MessageSequence) {
        self.apply_message_projection(messages);
        self.fold_pending_graph_appends();
    }

    fn fold_pending_graph_appends(&mut self) {
        for append in self.graph_appends.take_pending() {
            let drafts = session_append_node_drafts(&append.nodes, &append.draft_namespace);
            let node_ids = self
                .graph
                .append_node_drafts_in_namespace(&append.draft_namespace, drafts);
            debug_assert!(
                matches!(
                    &append.outcome,
                    crate::AppendSessionNodesOutcome::Appended { node_ids: recorded, .. }
                        if *recorded == node_ids
                ),
                "a queued in-turn append mints the ids it answered with"
            );
        }
    }

    pub(super) fn append_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SessionHistoryRecord>,
    {
        self.graph.append_events(events);
    }

    pub(super) fn read_view(
        &self,
        policy: crate::SessionPolicy,
        turn_index: usize,
        protocol_turn_options: crate::ProtocolTurnOptions,
        messages: MessageSequence,
    ) -> Result<SessionReadView, crate::SessionGraphScopeError> {
        SessionReadView::derived_from_persisted_state(
            &self.state,
            policy,
            turn_index,
            protocol_turn_options,
            self.graph.base_graph(),
            messages,
        )
    }

    pub(super) fn finalize_turn_read_state(
        &mut self,
        new_messages: MessageSequence,
        cancelled: bool,
    ) {
        let projected_messages =
            (new_messages.is_empty() && cancelled).then(|| self.graph.message_sequence());
        let projected_messages = projected_messages.as_ref().unwrap_or(&new_messages);

        if let Some(appended_messages) = self
            .graph
            .message_delta_if_current_preserved(projected_messages)
        {
            self.graph
                .append_active_conversation_messages(&appended_messages);
            return;
        }

        self.graph
            .project_active_read_state(projected_messages.as_slice());
    }

    pub(super) fn into_final_state(mut self) -> RuntimeSessionState {
        // The final commit is the last boundary: appends queued since the
        // previous one extend the leaf the editor produced, never choose it.
        self.fold_pending_graph_appends();
        self.state.persisted_node_ids = self.graph.persisted_node_ids();
        self.state.session_graph = self.graph.into_session_graph();
        self.state.refresh_current_frame_projection();
        self.state
    }

    #[cfg(test)]
    pub(super) fn graph_commit(&self) -> crate::store::GraphAppend {
        self.graph.graph_commit()
    }

    pub(super) fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.graph.mark_node_ids_persisted(node_ids);
    }

    pub(super) fn remap_node_ids(&mut self, session_id: &SessionId, mapping: &[(String, String)]) {
        self.graph.remap_node_ids(session_id, mapping);
        if let Some(current) = self.state.current_frame_node_id.as_mut()
            && let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current.as_str())
        {
            *current = crate::FrameNodeId::new(derived.clone())
                .expect("derived graph node identities are non-empty");
        }
    }

    fn apply_message_projection(&mut self, messages: &MessageSequence) {
        if let Some(appended_messages) = self.graph.message_delta_if_current_preserved(messages) {
            self.graph
                .append_active_conversation_messages(&appended_messages);
        } else {
            self.graph.project_active_read_state(messages.as_slice());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::facade_support::SessionNodeProjection;
    use crate::{
        AgentFrameReason, AppendSessionNodesOutcome, AppendSessionNodesRequest, Message,
        MessageRole, OpenAgentFrameRequest, Part, RuntimeSessionState, SessionAppendNode,
        SessionNodePayload, shared_parts,
    };

    fn plugin_append(operation_id: &str, bodies: &[&str]) -> AppendSessionNodesRequest {
        AppendSessionNodesRequest {
            operation_id: operation_id.to_string(),
            nodes: bodies
                .iter()
                .map(|body| SessionAppendNode::plugin("test.draft", serde_json::json!(body)))
                .collect(),
            requires_ancestor_node_id: None,
        }
    }

    fn appended_ids(outcome: &AppendSessionNodesOutcome) -> Vec<String> {
        match outcome {
            AppendSessionNodesOutcome::Appended { node_ids, .. } => node_ids.clone(),
            other => panic!("expected an appended outcome, got {other:?}"),
        }
    }

    fn seeded_state(session_id: &SessionId) -> RuntimeSessionState {
        let clock = crate::SystemClock;
        let mut state = RuntimeSessionState {
            session_id: SessionId::from(session_id.to_string()),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        state.ensure_agent_frame_initialized_with_clock(&clock);
        state.append_active_conversation_messages_with_clock(
            &[text_message("durable", "durable request")],
            &clock,
        );
        state.persisted_node_ids.extend(
            state
                .session_graph
                .nodes
                .iter()
                .map(|node| node.node_id.clone()),
        );
        state
    }

    #[test]
    fn recorded_appends_answer_like_durable_appends() {
        let state = seeded_state(&SessionId::from("draft-answers"));
        let durable_leaf = state.session_graph.leaf_node_id.clone().expect("leaf");
        let draft = TurnGraphAppendDraft::from_resident_state(&state, Arc::new(crate::SystemClock));

        let first = draft
            .record(
                &SessionId::from("draft-answers"),
                &plugin_append("op-a", &["a0", "a1"]),
            )
            .expect("first append");
        let first_ids = appended_ids(&first);
        assert_eq!(first_ids.len(), 2);
        assert!(matches!(
            &first,
            AppendSessionNodesOutcome::Appended { leaf_node_id, .. } if *leaf_node_id == first_ids[1]
        ));

        // Same identity replays the first answer; a reused id with another
        // request is the typed conflict the store would raise.
        let replayed = draft
            .record(
                &SessionId::from("draft-answers"),
                &plugin_append("op-a", &["a0", "a1"]),
            )
            .expect("replayed append");
        assert_eq!(appended_ids(&replayed), first_ids);
        assert!(matches!(
            draft.record(
                &SessionId::from("draft-answers"),
                &plugin_append("op-a", &["changed"])
            ),
            Err(crate::PluginError::AppendOperationIdentityConflict { .. })
        ));

        // Ancestors resolve against the resident active path plus nodes the
        // draft itself minted.
        let mut on_recorded = plugin_append("op-b", &["b0"]);
        on_recorded.requires_ancestor_node_id = Some(first_ids[0].clone());
        assert!(matches!(
            draft
                .record(&SessionId::from("draft-answers"), &on_recorded)
                .expect("append on recorded ancestor"),
            AppendSessionNodesOutcome::Appended { .. }
        ));
        let mut on_durable = plugin_append("op-c", &["c0"]);
        on_durable.requires_ancestor_node_id = Some(durable_leaf);
        assert!(matches!(
            draft
                .record(&SessionId::from("draft-answers"), &on_durable)
                .expect("append on durable ancestor"),
            AppendSessionNodesOutcome::Appended { .. }
        ));
        let mut off_path = plugin_append("op-d", &["d0"]);
        off_path.requires_ancestor_node_id = Some("not-on-the-active-path".to_string());
        assert!(matches!(
            draft.record(&SessionId::from("draft-answers"), &off_path).expect("stale branch answer"),
            AppendSessionNodesOutcome::StaleBranch { required_node_id }
                if required_node_id == "not-on-the-active-path"
        ));
    }

    #[test]
    fn queued_appends_fold_at_the_next_boundary_after_its_messages() {
        let state = seeded_state(&SessionId::from("draft-fold"));
        let clock: Arc<dyn crate::Clock> = Arc::new(crate::SystemClock);
        let appends = TurnGraphAppendDraft::from_resident_state(&state, Arc::clone(&clock));
        let mut draft =
            TurnCommitDraft::from_state_with_graph_appends(state, clock, "turn-1", appends.clone());
        let durable = text_message("durable", "durable request");
        let request = text_message("request", "turn request");
        let reply = text_message("reply", "turn reply");

        // Boundary one carries the request; the append recorded afterwards
        // waits for boundary two.
        draft.apply_prepared_messages(&MessageSequence::from_owned(vec![
            durable.clone(),
            request.clone(),
        ]));
        let before_reply = appended_ids(
            &appends
                .record(
                    &SessionId::from("draft-fold"),
                    &plugin_append("before-reply", &["mid"]),
                )
                .expect("append after boundary one"),
        );

        // Read overlays show the queued node before any boundary folds it.
        let mut read_state = draft.state().clone();
        appends.overlay_on_read_state(&mut read_state);
        assert!(
            read_state
                .session_graph
                .find_node(&before_reply[0])
                .is_some()
        );

        // Boundary two carries the reply: the queued append lands after it,
        // and the append recorded after boundary two waits for the final one.
        draft.apply_prepared_messages(&MessageSequence::from_owned(vec![
            durable.clone(),
            request.clone(),
            reply.clone(),
        ]));
        let after_reply = appended_ids(
            &appends
                .record(
                    &SessionId::from("draft-fold"),
                    &plugin_append("after-reply", &["late"]),
                )
                .expect("append after boundary two"),
        );
        draft.finalize_turn_read_state(
            MessageSequence::from_owned(vec![durable, request, reply]),
            false,
        );
        let mut state = draft.into_final_state();
        let finalize_hook = appended_ids(
            &appends
                .record(
                    &SessionId::from("draft-fold"),
                    &plugin_append("finalize-hook", &["hook"]),
                )
                .expect("finalize-hook append"),
        );
        appends.fold_into_final_state(&mut state);

        let path = state
            .session_graph
            .active_path_nodes()
            .into_iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        let message_index = |message_id: &str| {
            path.iter()
                .position(|id| {
                    state
                        .session_graph
                        .find_node(id)
                        .and_then(|node| node.message())
                        .is_some_and(|message| message.id == message_id)
                })
                .unwrap_or_else(|| panic!("message {message_id} is on the active path"))
        };
        let request_index = message_index("request");
        let reply_index = message_index("reply");
        assert_eq!(reply_index, request_index + 1);
        assert_eq!(
            &path[reply_index + 1..],
            &[
                before_reply[0].clone(),
                after_reply[0].clone(),
                finalize_hook[0].clone(),
            ],
            "each append lands at the boundary after it was queued, behind the messages that boundary carries"
        );
        assert_eq!(
            state.session_graph.leaf_node_id,
            Some(finalize_hook[0].clone())
        );
        let pending = state.pending_graph_commit();
        assert_eq!(
            pending
                .nodes
                .iter()
                .filter(|node| matches!(node.payload, SessionNodePayload::Plugin { .. }))
                .count(),
            3,
            "every queued append is committed exactly once"
        );
    }

    fn text_message(id: &str, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), text.to_string(), None)]),
            origin: None,
        }
    }

    #[test]
    fn prompt_projection_appends_new_messages_from_the_durable_leaf() {
        let clock = crate::SystemClock;
        let mut state = RuntimeSessionState {
            session_id: SessionId::from("frame-replacement"),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
        };
        state.ensure_agent_frame_initialized_with_clock(&clock);
        state.append_active_conversation_messages_with_clock(
            &[text_message("old-root", "old root")],
            &clock,
        );
        let opened = super::super::state::open_agent_frame_in_state_with_clock(
            &mut state,
            OpenAgentFrameRequest::new(
                crate::FrameKey::from_caller_material("compacted")
                    .expect("non-empty frame material"),
                AgentFrameReason::compaction(),
            ),
            &clock,
        )
        .expect("open a new compaction frame");
        assert!(opened.opened);
        state.append_active_conversation_messages_with_clock(
            &[text_message("seed", "frame seed")],
            &clock,
        );
        let durable_leaf = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("durable frame leaf");
        state.persisted_node_ids.extend(
            state
                .session_graph
                .nodes
                .iter()
                .map(|node| node.node_id.clone()),
        );

        let mut draft =
            TurnCommitDraft::from_state_with_clock(state, Arc::new(clock), "replacement");
        draft.finalize_turn_read_state(
            MessageSequence::from_owned(vec![
                text_message("replacement", "replacement"),
                text_message("seed", "frame seed"),
            ]),
            false,
        );
        let commit = draft.graph_commit();
        assert_eq!(commit.nodes.len(), 1);
        assert_eq!(
            commit.nodes[0].parent_node_id.as_deref(),
            Some(durable_leaf.as_str())
        );
        assert_eq!(
            draft
                .message_sequence()
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["replacement", "seed"]
        );

        let state = draft.into_final_state();

        assert_eq!(
            state.current_frame_node_id.as_deref(),
            Some(opened.frame_node_id.as_str())
        );
        assert_eq!(
            state
                .session_graph
                .nearest_frame_node_id(state.session_graph.leaf_node_id.as_deref()),
            Some(opened.frame_node_id.as_str())
        );
        let read = state
            .read_model()
            .expect("test runtime frame scope resolves");
        assert_eq!(
            read.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["seed", "replacement"]
        );
    }
}
