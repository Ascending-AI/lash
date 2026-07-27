use std::sync::Arc;

use crate::session_model::SessionHistoryRecord;
use crate::{MessageSequence, SessionReadView};

use super::RuntimeSessionState;
use super::turn_graph_editor::TurnGraphEditor;

#[derive(Debug)]
pub(super) struct TurnCommitDraft {
    graph: TurnGraphEditor,
    state: RuntimeSessionState,
}

impl TurnCommitDraft {
    pub(super) fn from_state_with_clock(
        mut state: RuntimeSessionState,
        clock: Arc<dyn crate::Clock>,
        draft_namespace: &str,
    ) -> Self {
        state.ensure_agent_frame_initialized_with_clock(clock.as_ref());
        let base_graph = Arc::new(std::mem::take(&mut state.session_graph));
        let base_read_model = state.current_frame_node_id.as_deref().map_or_else(
            || base_graph.read_model(),
            |frame_node_id| base_graph.read_model_for_frame(frame_node_id),
        );
        let persisted_node_ids = std::mem::take(&mut state.persisted_node_ids);
        let graph = TurnGraphEditor::new(
            base_graph,
            base_read_model,
            state.current_frame_node_id.clone(),
            draft_namespace,
            clock,
            persisted_node_ids,
        );
        Self { graph, state }
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

    pub(super) fn apply_prepared_messages(&mut self, messages: &MessageSequence) {
        self.apply_message_projection(messages);
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
            (new_messages.is_empty() && cancelled).then(|| self.graph.message_sequence().shared());
        let appended_messages = if let Some(projected_messages) = projected_messages.as_ref() {
            self.graph
                .message_delta_if_current_preserved(projected_messages.iter())
        } else {
            self.graph
                .message_delta_if_current_preserved(new_messages.iter())
        };

        if let Some(appended_messages) = appended_messages {
            self.graph
                .append_active_conversation_messages(&appended_messages);
            return;
        }

        let projected_messages = projected_messages.unwrap_or_else(|| new_messages.shared());
        self.graph
            .replace_active_read_state(projected_messages.as_slice());
    }

    pub(super) fn into_final_state(mut self) -> RuntimeSessionState {
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

    pub(super) fn remap_node_ids(&mut self, session_id: &str, mapping: &[(String, String)]) {
        self.graph.remap_node_ids(session_id, mapping);
        if let Some(current) = self.state.current_frame_node_id.as_mut()
            && let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current)
        {
            *current = derived.clone();
        }
    }

    fn apply_message_projection(&mut self, messages: &MessageSequence) {
        if let Some(appended_messages) = self
            .graph
            .message_delta_if_current_preserved(messages.iter())
        {
            self.graph
                .append_active_conversation_messages(&appended_messages);
        } else {
            let read_messages = messages.shared();
            self.graph
                .replace_active_read_state(read_messages.as_slice());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        AgentFrameReason, Message, MessageRole, OpenAgentFrameRequest, Part, PartKind, PruneState,
        RuntimeSessionState, shared_parts,
    };

    fn text_message(id: &str, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: text.to_string(),
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

    #[test]
    fn replacement_cannot_move_the_leaf_behind_the_current_frame() {
        let clock = crate::SystemClock;
        let mut state = RuntimeSessionState {
            session_id: "frame-replacement".to_string(),
            ..RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized_with_clock(&clock);
        state.append_active_conversation_messages_with_clock(
            &[text_message("old-root", "old root")],
            &clock,
        );
        let opened = super::super::state::open_agent_frame_in_state_with_clock(
            &mut state,
            OpenAgentFrameRequest::new("compacted", AgentFrameReason::compaction()),
            &clock,
        );
        assert!(opened.opened);
        state.append_active_conversation_messages_with_clock(
            &[text_message("seed", "frame seed")],
            &clock,
        );

        let mut draft =
            TurnCommitDraft::from_state_with_clock(state, Arc::new(clock), "replacement");
        draft.finalize_turn_read_state(
            MessageSequence::from_owned(vec![text_message("replacement", "replacement")]),
            false,
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
        let read = state.read_model();
        assert_eq!(read.messages.len(), 1);
        assert_eq!(read.messages[0].id, "replacement");
    }
}
