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
    ) -> Self {
        let base_graph = Arc::new(std::mem::take(&mut state.session_graph));
        let base_read_model = base_graph.read_model_for_agent_frame(
            &state.current_agent_frame_id,
            state
                .current_agent_frame()
                .map(|frame| frame.previous_frame_id.is_none())
                .unwrap_or(true),
        );
        let graph = TurnGraphEditor::new(
            base_graph,
            base_read_model,
            state.current_agent_frame_id.clone(),
            clock,
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
        self.state.session_graph = self.graph.into_session_graph();
        self.state
    }

    pub(super) fn graph_commit(
        &self,
        graph_replace_required: bool,
    ) -> crate::store::GraphCommitDelta {
        self.graph.graph_commit(graph_replace_required)
    }

    pub(super) fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.graph.mark_node_ids_persisted(node_ids);
    }

    pub(super) fn replace_persisted_node_ids<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.graph.replace_persisted_node_ids(node_ids);
    }

    pub(super) fn remap_node_ids(&mut self, mapping: &[(String, String)]) {
        self.graph.remap_node_ids(mapping);
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
