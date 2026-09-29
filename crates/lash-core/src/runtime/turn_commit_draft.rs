use crate::SessionId;
use crate::facade_support::AgentFrameReasonFacadeOps;
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

/// Which author put the turn's one frame transition in its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameSwitchAuthor {
    /// A context-pressure hook's `OpenFrame` decision. Core opened it on the
    /// resident state before the turn ran, so the final commit carries it
    /// without opening it again.
    ContextPressure,
    /// The turn's own protocol `AgentFrameSwitch` outcome (`continue_as`),
    /// opened by the turn's final commit.
    TurnOutcome,
}

/// The switch a turn's protocol `AgentFrameSwitch` outcome asks for.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct OutcomeFrameSwitch {
    /// Stable identity of this switch within the turn.
    pub(in crate::runtime) operation_id: String,
    pub(in crate::runtime) frame_key: crate::FrameKey,
    pub(in crate::runtime) task: String,
    /// Nodes the fresh frame starts with.
    pub(in crate::runtime) initial_nodes: Vec<crate::SessionAppendNode>,
}

/// The one frame transition a running turn carries: a context-pressure frame
/// core opened before the turn ran, or the turn's own `AgentFrameSwitch`
/// outcome. Consumed by the turn's final commit, never at an intermediate
/// boundary.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct RecordedFrameSwitch {
    pub(in crate::runtime) identity: String,
    pub(in crate::runtime) frame_key: crate::FrameKey,
    author: FrameSwitchAuthor,
    reason: crate::AgentFrameReason,
    task: String,
    initial_nodes: Vec<crate::SessionAppendNode>,
    outcome: crate::OpenAgentFrameResult,
}

impl RecordedFrameSwitch {
    /// The nodes the switch's fresh frame starts with.
    pub(in crate::runtime) fn initial_nodes(&self) -> &[crate::SessionAppendNode] {
        &self.initial_nodes
    }
}

/// A context-pressure decision core applies before the turn runs: the hook
/// that made it, and the turn scope that names its writes.
pub(in crate::runtime) struct ContextPressureWrite<'a> {
    pub(in crate::runtime) session_id: &'a SessionId,
    pub(in crate::runtime) turn_scope_id: &'a str,
    pub(in crate::runtime) hook_id: &'a str,
}

impl ContextPressureWrite<'_> {
    /// The append a decision's records ride. Named by the turn and the hook,
    /// so a redriven prepare re-records the same append.
    fn records_request(
        &self,
        nodes: Vec<crate::SessionAppendNode>,
    ) -> crate::AppendSessionNodesRequest {
        crate::AppendSessionNodesRequest {
            operation_id: format!(
                "{}/context-pressure/{}/records",
                self.turn_scope_id, self.hook_id
            ),
            nodes,
            requires_ancestor_node_id: None,
        }
    }

    /// The frame an `OpenFrame` decision opens, derived by core from the
    /// turn's scope and the frame the turn is leaving.
    fn frame_key(&self, current_frame_node_id: Option<&str>) -> crate::FrameKey {
        crate::FrameKey::from_compaction_material(
            self.session_id,
            &format!("{}/context-pressure/{}", self.turn_scope_id, self.hook_id),
            current_frame_node_id.unwrap_or_default(),
        )
    }
}

#[derive(Debug)]
struct TurnGraphAppendDraftInner {
    /// Node ids on the resident active path when the turn began, plus every
    /// node minted by a recorded append: the set an ancestor requirement is
    /// resolved against.
    active_node_ids: HashSet<crate::NodeId>,
    leaf_node_id: Option<crate::NodeId>,
    recorded: Vec<RecordedTurnGraphAppend>,
    /// The turn's one frame transition (FIG-3303): a context-pressure frame
    /// opened before the turn ran, or the turn's own switch outcome.
    frame_switch: Option<RecordedFrameSwitch>,
    /// Prefix of `recorded` already folded into the turn's final state.
    applied: usize,
    /// Prefix of `recorded` folded into the resident state itself when a
    /// context-pressure frame opened before the turn ran; read overlays built
    /// from that state must not apply it twice.
    resident: usize,
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
                frame_switch: None,
                applied: 0,
                resident: 0,
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
        if let Some(required_node_id) = request.requires_ancestor_node_id.as_ref()
            && !inner.active_node_ids.contains(required_node_id)
        {
            return Ok(crate::AppendSessionNodesOutcome::StaleBranch {
                required_node_id: required_node_id.clone(),
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
            leaf_node_id: inner
                .leaf_node_id
                .clone()
                .unwrap_or_else(|| crate::NodeId::new(String::new())),
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
        let recorded = {
            let inner = self.inner.lock_recover();
            inner.recorded[inner.resident..].to_vec()
        };
        apply_recorded_appends(state, &recorded, self.clock.as_ref());
    }

    /// Records a context-pressure hook's `Record` decision: its nodes join
    /// the current frame at the turn's first boundary, after the turn's own
    /// input.
    pub(in crate::runtime) fn record_context_pressure_records(
        &self,
        write: &ContextPressureWrite<'_>,
        nodes: Vec<crate::SessionAppendNode>,
    ) -> Result<(), crate::RuntimeError> {
        self.record(write.session_id, &write.records_request(nodes))
            .map(|_| ())
            .map_err(context_pressure_write_error)
    }

    /// Applies a context-pressure hook's `OpenFrame` decision on the resident
    /// state, before the turn runs (ADR 0001, ADR 0112 §9).
    ///
    /// A frame is the context window. The decision's records are appended to
    /// the frame being left, then core opens the frame with its seed nodes
    /// under a key it derives from the turn's scope and the current frame,
    /// exactly as an explicit `open_agent_frame` does between turns. The turn
    /// then runs in the new frame and its own messages follow the seed.
    /// Nothing commits here and nothing reloads: the turn's commit carries the
    /// records and the frame. The slot keeps the transition under the slot's
    /// one-switch rule, and the final commit does not open the frame again.
    pub(in crate::runtime) fn open_context_pressure_frame_before_turn(
        &self,
        state: &mut RuntimeSessionState,
        write: &ContextPressureWrite<'_>,
        records: Vec<crate::SessionAppendNode>,
        task: String,
        seed: Vec<crate::SessionAppendNode>,
    ) -> Result<crate::OpenAgentFrameResult, crate::RuntimeError> {
        if !records.is_empty() {
            self.record(write.session_id, &write.records_request(records))
                .map_err(context_pressure_write_error)?;
        }
        let mut inner = self.inner.lock_recover();
        if let Some(recorded) = inner.frame_switch.as_ref() {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::AgentFrameSwitchAuthorConflict,
                format!(
                    "turn `{session_id}` already carries frame transition `{recorded_id}`; refusing the context-pressure frame of `{hook_id}` — a turn opens at most one frame",
                    session_id = write.session_id,
                    recorded_id = recorded.identity,
                    hook_id = write.hook_id,
                ),
            ));
        }
        debug_assert_eq!(
            inner.applied, 0,
            "a context-pressure frame opens before the turn's first boundary"
        );
        let frame_key = write.frame_key(state.current_frame_node_id.as_deref());
        let reason = crate::AgentFrameReason::compaction();
        let pending = inner.recorded.clone();
        apply_recorded_appends(state, &pending, self.clock.as_ref());
        inner.applied = pending.len();
        inner.resident = pending.len();
        let result = crate::runtime::state::open_agent_frame_in_state_with_clock(
            state,
            crate::OpenAgentFrameRequest::new(frame_key.clone(), reason.clone())
                .with_initial_nodes(seed.clone()),
            self.clock.as_ref(),
        )?;
        inner
            .active_node_ids
            .insert(crate::NodeId::from(result.frame_node_id.as_str()));
        inner
            .active_node_ids
            .extend(result.initial_node_ids.iter().cloned());
        inner.leaf_node_id = state.session_graph.leaf_node_id.clone();
        inner.frame_switch = Some(RecordedFrameSwitch {
            identity: format!(
                "{}/context-pressure/{}/frame",
                write.turn_scope_id, write.hook_id
            ),
            frame_key,
            author: FrameSwitchAuthor::ContextPressure,
            reason,
            task,
            initial_nodes: seed,
            outcome: result.clone(),
        });
        Ok(result)
    }

    /// Records the turn's protocol `AgentFrameSwitch` outcome and answers it
    /// the way the turn's materialization will.
    ///
    /// This slot is the turn's single owning source of truth for "this commit
    /// opens this frame" (FIG-3303), so a turn carries at most one switch and
    /// the final commit has exactly one application site:
    ///
    /// - recording the same switch again (same frame key, same seed nodes) is
    ///   a replay and answers the first outcome unchanged;
    /// - a second record naming another frame key, a context-pressure frame
    ///   included, or the same key with other seed nodes, is refused.
    ///
    /// A switch naming the already-current frame answers `opened = false` with
    /// no seed ids and no fold work. Every refusal is
    /// [`crate::RuntimeErrorCode::AgentFrameSwitchAuthorConflict`].
    pub(in crate::runtime) fn record_outcome_frame_switch(
        &self,
        session_id: &SessionId,
        current_frame_node_id: Option<&str>,
        request: &OutcomeFrameSwitch,
    ) -> Result<crate::OpenAgentFrameResult, crate::RuntimeError> {
        let conflict = |message: String| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::AgentFrameSwitchAuthorConflict,
                message,
            )
        };
        let frame_node_id =
            crate::session_graph::frame_node_id(session_id, request.frame_key.as_str());
        let mut inner = self.inner.lock_recover();
        if let Some(recorded) = &inner.frame_switch {
            if recorded.frame_key != request.frame_key {
                return Err(conflict(format!(
                    "turn `{session_id}` already carries agent-frame switch `{recorded_id}` ({task}) to `{target:?}`; refusing `{operation_id}` to `{key:?}` — one turn materializes at most one switch",
                    recorded_id = recorded.identity,
                    task = recorded.task,
                    target = recorded.frame_key,
                    operation_id = request.operation_id,
                    key = request.frame_key
                )));
            }
            if recorded.initial_nodes != request.initial_nodes {
                return Err(conflict(format!(
                    "turn `{session_id}` already carries agent-frame switch `{recorded_id}` to `{target:?}` with different initial nodes; refusing `{operation_id}` — a second record of one switch must name the same seed nodes",
                    recorded_id = recorded.identity,
                    target = recorded.frame_key,
                    operation_id = request.operation_id
                )));
            }
            return Ok(recorded.outcome.clone());
        }
        let outcome = if current_frame_node_id == Some(frame_node_id.as_str()) {
            crate::OpenAgentFrameResult {
                frame_node_id: frame_node_id.clone().into_inner(),
                opened: false,
                initial_node_ids: Vec::new(),
            }
        } else {
            crate::OpenAgentFrameResult {
                frame_node_id: frame_node_id.clone().into_inner(),
                opened: true,
                initial_node_ids: (0..request.initial_nodes.len() as u64)
                    .map(|ordinal| {
                        crate::session_graph::draft_node_id(request.frame_key.as_str(), ordinal)
                    })
                    .collect(),
            }
        };
        inner.frame_switch = Some(RecordedFrameSwitch {
            identity: request.operation_id.clone(),
            frame_key: request.frame_key.clone(),
            author: FrameSwitchAuthor::TurnOutcome,
            reason: crate::AgentFrameReason::continue_as(),
            task: request.task.clone(),
            initial_nodes: request.initial_nodes.clone(),
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Folds the appends recorded since the previous fold into `state`, after
    /// whatever nodes `state` already holds, then materializes the turn's
    /// switch outcome, if it recorded one.
    ///
    /// This is the only place a turn's final commit opens the frame it
    /// switches to (FIG-3303): the frame is opened once, with the seed nodes
    /// the outcome was already answered with, and the typed refusal a switch
    /// raises reaches the caller with its own code. A context-pressure frame
    /// already opened before the turn ran and is not opened again.
    pub(in crate::runtime) fn fold_into_final_state(
        &self,
        state: &mut RuntimeSessionState,
    ) -> Result<(), crate::RuntimeError> {
        let (pending, frame_switch) = {
            let mut inner = self.inner.lock_recover();
            let pending = inner.recorded[inner.applied..].to_vec();
            inner.applied = inner.recorded.len();
            let frame_switch = inner.frame_switch.take();
            (pending, frame_switch)
        };
        apply_recorded_appends(state, &pending, self.clock.as_ref());
        let Some(recorded) =
            frame_switch.filter(|recorded| recorded.author == FrameSwitchAuthor::TurnOutcome)
        else {
            return Ok(());
        };
        let request =
            crate::OpenAgentFrameRequest::new(recorded.frame_key.clone(), recorded.reason.clone())
                .with_initial_nodes(recorded.initial_nodes.clone());
        let result = crate::runtime::state::open_agent_frame_in_state_with_clock(
            state,
            request,
            self.clock.as_ref(),
        )?;
        // The answer the outcome was already given and the commit it rides
        // must name the same frame node and the same seed nodes; anything else
        // means seed nodes were dropped on the way into the commit.
        debug_assert_eq!(result.frame_node_id, recorded.outcome.frame_node_id);
        debug_assert_eq!(result.initial_node_ids, recorded.outcome.initial_node_ids);
        Ok(())
    }

    /// The frame switch recorded but not yet folded, if any. Read before the
    /// final fold: the final commit clears protocol execution when this
    /// materializes, exactly as an in-frame-switching outcome would.
    pub(in crate::runtime) fn pending_frame_switch(&self) -> Option<RecordedFrameSwitch> {
        self.inner.lock_recover().frame_switch.clone()
    }

    /// Drains the appends recorded since the previous fold.
    fn take_pending(&self) -> Vec<RecordedTurnGraphAppend> {
        let mut inner = self.inner.lock_recover();
        let pending = inner.recorded[inner.applied..].to_vec();
        inner.applied = inner.recorded.len();
        pending
    }
}

fn context_pressure_write_error(error: crate::PluginError) -> crate::RuntimeError {
    crate::RuntimeError::new(
        crate::RuntimeErrorCode::ContextPrepareTurn,
        format!("context-pressure decision could not be recorded: {error}"),
    )
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
        let base_read_model = base_graph.read_model();
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
    ) -> SessionReadView {
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
        self.state.persisted_node_ids = self.graph.take_persisted_node_ids();
        self.state.session_graph = self.graph.into_session_graph();
        self.state.refresh_current_frame_projection();
        self.state
    }

    #[cfg(test)]
    pub(super) fn graph_commit(&self) -> crate::store::GraphAppend {
        self.graph.graph_commit()
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

    fn appended_ids(outcome: &AppendSessionNodesOutcome) -> Vec<crate::NodeId> {
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

    /// ADR 0112 §14.7: the turn editor opens on the resident state's read
    /// model itself, so its base shares the state's `Arc`s and a rope over
    /// that base is settled by identity, without walking the history.
    #[test]
    fn the_turn_editor_opens_on_the_states_own_read_model() {
        let state = seeded_state(&SessionId::from("draft-shared-base"));
        let model = state.read_model();
        let draft = TurnCommitDraft::from_state_with_clock(
            state,
            Arc::new(crate::SystemClock),
            "draft-shared-base",
        );
        let base = draft.graph.read_model();
        assert!(Arc::ptr_eq(&model.messages, &base.messages));
        assert!(Arc::ptr_eq(&model.active_events, &base.active_events));

        let next = MessageSequence::from_base_and_delta(
            Arc::clone(&model.messages),
            vec![text_message("turn", "this turn")],
        );
        let delta = draft
            .graph
            .message_delta_if_current_preserved(&next)
            .expect("a rope over the state's own messages preserves its prefix");
        assert_eq!(
            delta
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn"]
        );
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
        off_path.requires_ancestor_node_id = Some("not-on-the-active-path".into());
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
        let _ = appends.fold_into_final_state(&mut state);

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
                .nodes()
                .iter()
                .filter(|node| matches!(node.payload, SessionNodePayload::Plugin { .. }))
                .count(),
            3,
            "every queued append is committed exactly once"
        );
    }

    /// FIG-4110: a context-pressure `OpenFrame` decision opens before the
    /// turn runs. Its records stay in the frame it leaves, the turn's
    /// messages follow the seed in the new frame, read overlays do not apply
    /// the folded records twice, and the final commit neither reopens the
    /// frame nor opens another one.
    #[test]
    fn a_context_pressure_frame_opens_before_the_turn_and_is_carried_once() {
        let session_id = SessionId::from("draft-pressure-frame");
        let mut state = seeded_state(&session_id);
        let old_frame = state.current_frame_node_id.clone();
        let clock: Arc<dyn crate::Clock> = Arc::new(crate::SystemClock);
        let appends = TurnGraphAppendDraft::from_resident_state(&state, Arc::clone(&clock));
        let write = ContextPressureWrite {
            session_id: &session_id,
            turn_scope_id: "turn-pressure",
            hook_id: "test.pressure",
        };
        let opened = appends
            .open_context_pressure_frame_before_turn(
                &mut state,
                &write,
                vec![SessionAppendNode::plugin(
                    "test.record",
                    serde_json::json!("record"),
                )],
                "context-pressure compaction".to_string(),
                vec![SessionAppendNode::message(
                    crate::PluginMessage::text(MessageRole::Assistant, "summary").with_id("seed"),
                )],
            )
            .expect("open the pressure frame before the turn");
        assert!(opened.opened);
        assert_eq!(
            state
                .current_frame_node_id
                .as_ref()
                .map(|frame| frame.as_str()),
            Some(opened.frame_node_id.as_str())
        );
        let record = state
            .session_graph
            .nodes
            .iter()
            .find(|node| matches!(node.payload, SessionNodePayload::Plugin { .. }))
            .expect("the record is on the resident graph")
            .node_id
            .clone();
        assert_eq!(
            state
                .session_graph
                .nearest_frame_node_id(Some(record.as_str()))
                .map(crate::NodeId::as_str),
            old_frame.as_ref().map(|frame| frame.as_str()),
            "the record stays in the frame it leaves"
        );
        // Re-deriving the frame from the same turn scope names the same key.
        assert_eq!(
            crate::session_graph::frame_node_id(
                &session_id,
                write
                    .frame_key(old_frame.as_ref().map(|frame| frame.as_str()))
                    .as_str()
            )
            .as_str(),
            opened.frame_node_id.as_str()
        );
        let mut read_state = state.clone();
        appends.overlay_on_read_state(&mut read_state);
        assert_eq!(
            read_state.session_graph.nodes.len(),
            state.session_graph.nodes.len(),
            "a read overlay does not apply the folded record again"
        );
        let mut draft = TurnCommitDraft::from_state_with_graph_appends(
            state,
            clock,
            "turn-pressure",
            appends.clone(),
        );
        let request = text_message("request", "turn request");
        draft.apply_prepared_messages(&MessageSequence::from_owned(vec![request.clone()]));
        draft.finalize_turn_read_state(MessageSequence::from_owned(vec![request]), false);
        let mut state = draft.into_final_state();
        appends
            .fold_into_final_state(&mut state)
            .expect("the final fold carries the opened frame");
        let frames = state
            .session_graph
            .nodes
            .iter()
            .filter(|node| matches!(node.payload, SessionNodePayload::FrameOpen { .. }))
            .count();
        assert_eq!(frames, 2, "the frame opened exactly once");
        let read = state.read_model();
        assert_eq!(
            read.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["seed", "request"],
            "the turn's messages follow the seed in the new frame"
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
        assert_eq!(commit.nodes().len(), 1);
        assert_eq!(
            commit.nodes()[0].parent_node_id.as_deref(),
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
                .nearest_frame_node_id(state.session_graph.leaf_node_id.as_deref())
                .map(crate::NodeId::as_str),
            Some(opened.frame_node_id.as_str())
        );
        let read = state.read_model();
        assert_eq!(
            read.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["seed", "replacement"]
        );
    }
}
