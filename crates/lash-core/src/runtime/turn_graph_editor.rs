use lash_sansio::core_support::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::facade_support::{SessionGraphFacadeOps, SessionNodeRecordFacadeOps};
use crate::session_graph::SessionReadModel;
use crate::session_graph::build_active_read_replacement;
use crate::session_model::SessionHistoryRecord;
#[cfg(test)]
use crate::store::GraphAppend;
use crate::{BaseRenderCache, Message, MessageSequence, SessionGraph, SessionNodeRecord};

#[derive(Debug)]
pub(super) struct TurnGraphEditor {
    base_graph: Arc<SessionGraph>,
    active_events: Arc<Vec<SessionHistoryRecord>>,
    active_messages: MessageSequence,
    current_frame_node_id: Option<String>,
    append_builder: crate::session_graph::SessionGraphAppendBuilder,
    appended_nodes: Vec<SessionNodeRecord>,
    appended_node_indices: HashMap<String, usize>,
    committed_node_ids: HashSet<String>,
    clock: Arc<dyn crate::Clock>,
}

impl TurnGraphEditor {
    pub(super) fn new(
        base_graph: Arc<SessionGraph>,
        base_read_model: SessionReadModel,
        current_frame_node_id: Option<String>,
        draft_namespace: &str,
        clock: Arc<dyn crate::Clock>,
        persisted_node_ids: HashSet<String>,
    ) -> Self {
        let append_builder = base_graph.append_builder_in_namespace(draft_namespace);
        let active_messages = MessageSequence::from_base(base_read_model.messages);
        Self {
            committed_node_ids: persisted_node_ids,
            base_graph,
            active_events: base_read_model.active_events,
            active_messages,
            current_frame_node_id,
            append_builder,
            appended_nodes: Vec::new(),
            appended_node_indices: HashMap::new(),
            clock,
        }
    }

    pub(super) fn base_graph(&self) -> Arc<SessionGraph> {
        Arc::clone(&self.base_graph)
    }

    pub(super) fn message_sequence(&self) -> MessageSequence {
        self.active_messages.clone()
    }

    pub(super) fn read_model(&self) -> SessionReadModel {
        SessionReadModel {
            active_events: Arc::clone(&self.active_events),
            messages: self.active_messages.shared(),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        }
    }

    pub(super) fn append_events<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = SessionHistoryRecord>,
    {
        let events = events
            .into_iter()
            .filter(|event| match event {
                SessionHistoryRecord::Conversation(record) => {
                    let message = record.to_message();
                    !message.is_transient()
                }
                SessionHistoryRecord::Protocol(_) => true,
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            return;
        }
        let nodes = self
            .append_builder
            .append_events_at(events, self.clock.timestamp_rfc3339());
        Arc::make_mut(&mut self.active_events)
            .extend(nodes.iter().filter_map(|node| node.event().cloned()));
        self.active_messages
            .extend(nodes.iter().filter_map(|node| node.message()).collect());
        self.append_appended_nodes(nodes);
    }

    pub(super) fn message_delta_if_current_preserved<'a>(
        &self,
        next: impl IntoIterator<Item = &'a Message>,
    ) -> Option<Vec<Message>> {
        let mut current = self.active_messages.iter();
        let mut appended = Vec::new();
        for message in next.into_iter().filter(|message| !message.is_transient()) {
            if let Some(current_message) = current.next() {
                if serde_json::to_value(current_message).ok() != serde_json::to_value(message).ok()
                {
                    return None;
                }
            } else {
                appended.push(message.clone());
            }
        }
        current.next().is_none().then_some(appended)
    }

    pub(super) fn append_active_conversation_messages(&mut self, messages: &[Message]) {
        let appendable_messages = messages
            .iter()
            .filter(|message| !message.is_transient())
            .cloned()
            .collect::<Vec<_>>();
        if appendable_messages.is_empty() {
            return;
        }

        let nodes = self
            .append_builder
            .append_messages_at(appendable_messages.clone(), self.clock.timestamp_rfc3339());
        Arc::make_mut(&mut self.active_events)
            .extend(nodes.iter().filter_map(|node| node.event().cloned()));
        self.append_appended_nodes(nodes);
        self.active_messages.extend(appendable_messages);
    }

    pub(super) fn replace_active_read_state(&mut self, messages: &[Message]) {
        let active_path = self.active_path_nodes();
        let replacement = build_active_read_replacement(
            active_path,
            self.append_builder.existing_node_ids(),
            self.append_builder.draft_namespace(),
            messages,
            self.clock.timestamp_rfc3339(),
        );
        self.append_builder.register_existing_node_ids(
            replacement
                .new_tail_nodes
                .iter()
                .map(|node| node.node_id.as_str()),
        );
        self.append_appended_nodes(replacement.new_tail_nodes);
        self.append_builder
            .set_leaf_node_id(replacement.leaf_node_id.clone());
        self.active_events = Arc::new(replacement.active_events);
        self.active_messages = MessageSequence::from_owned(replacement.active_messages);
    }

    #[cfg(test)]
    pub(super) fn graph_commit(&self) -> GraphAppend {
        let nodes = self
            .base_graph
            .nodes
            .iter()
            .chain(self.appended_nodes.iter())
            .filter(|node| !self.committed_node_ids.contains(&node.node_id))
            .cloned()
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            GraphAppend {
                nodes: Vec::new(),
                leaf_node_id: self.leaf_node_id(),
            }
        } else {
            GraphAppend {
                nodes,
                leaf_node_id: self.leaf_node_id(),
            }
        }
    }

    pub(super) fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.committed_node_ids.extend(node_ids);
    }

    pub(super) fn persisted_node_ids(&self) -> HashSet<String> {
        self.committed_node_ids.clone()
    }

    pub(super) fn remap_node_ids(&mut self, session_id: &str, mapping: &[(String, String)]) {
        if mapping.is_empty() {
            return;
        }
        let by_old = mapping.iter().cloned().collect::<HashMap<_, _>>();
        Arc::make_mut(&mut self.base_graph).remap_node_ids(session_id, mapping);
        for node in &mut self.appended_nodes {
            if let Some(derived) = by_old.get(&node.node_id) {
                node.node_id = derived.clone();
            }
            if let Some(parent) = node.parent_node_id.as_mut()
                && let Some(derived) = by_old.get(parent)
            {
                *parent = derived.clone();
            }
        }
        self.appended_node_indices = self
            .appended_node_indices
            .drain()
            .map(|(id, index)| (by_old.get(&id).cloned().unwrap_or(id), index))
            .collect();
        self.append_builder.remap_node_ids(mapping);
    }

    pub(super) fn into_session_graph(self) -> SessionGraph {
        let leaf_node_id = self.leaf_node_id();
        if self.appended_nodes.is_empty() {
            return Arc::try_unwrap(self.base_graph).unwrap_or_else(|graph| graph.as_ref().clone());
        }
        match Arc::try_unwrap(self.base_graph) {
            Ok(mut graph) => {
                graph.extend_node_records(self.appended_nodes);
                graph.set_leaf_node_id(leaf_node_id);
                graph
            }
            Err(base_graph) => {
                let mut nodes =
                    Vec::with_capacity(base_graph.nodes.len() + self.appended_nodes.len());
                nodes.extend(base_graph.nodes.iter().cloned());
                nodes.extend(self.appended_nodes);
                SessionGraph::from_nodes(nodes, leaf_node_id)
            }
        }
    }

    fn leaf_node_id(&self) -> Option<String> {
        self.append_builder.leaf_node_id().cloned()
    }

    fn active_path_nodes(&self) -> Vec<&SessionNodeRecord> {
        let mut path = Vec::new();
        let mut current = self.leaf_node_id();
        let mut remaining = self.base_graph.nodes.len() + self.appended_nodes.len();
        while let Some(node_id) = current {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            let Some(node) = self
                .appended_node_indices
                .get(node_id.as_str())
                .and_then(|idx| self.appended_nodes.get(*idx))
                .or_else(|| self.base_graph.find_node(node_id.as_str()))
            else {
                break;
            };
            path.push(node);
            current = node.parent_node_id.clone();
        }
        path.reverse();
        if let Some(frame_node_id) = self.current_frame_node_id.as_deref() {
            let Some(frame_index) = path.iter().position(|node| node.node_id == frame_node_id)
            else {
                return Vec::new();
            };
            path.drain(..frame_index);
        }
        path
    }

    fn append_appended_nodes(&mut self, nodes: Vec<SessionNodeRecord>) {
        self.appended_node_indices.reserve(nodes.len());
        self.appended_nodes.reserve(nodes.len());
        for node in nodes {
            self.appended_node_indices
                .insert(node.node_id.clone(), self.appended_nodes.len());
            self.appended_nodes.push(node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> Message {
        Message {
            id: id.to_string(),
            role: crate::MessageRole::User,
            parts: crate::shared_parts(vec![crate::Part {
                id: format!("{id}.p0"),
                kind: crate::PartKind::Text,
                content: text.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: crate::PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }
    }

    fn editor() -> TurnGraphEditor {
        let graph = Arc::new(SessionGraph::default());
        TurnGraphEditor::new(
            Arc::clone(&graph),
            graph.read_model(),
            None,
            "turn-graph-editor-test",
            Arc::new(crate::SystemClock),
            HashSet::new(),
        )
    }

    #[test]
    fn append_commit_and_persistence_marking_follow_the_same_graph_path() {
        let mut editor = editor();
        let first = message("first", "one");
        let delta = editor
            .message_delta_if_current_preserved([&first])
            .expect("empty editor accepts appended message");
        assert_eq!(
            delta
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first"]
        );

        editor.append_active_conversation_messages(&[first]);
        let pending = editor.graph_commit();
        assert_eq!(pending.nodes.len(), 1);
        assert_eq!(pending.leaf_node_id, Some(pending.nodes[0].node_id.clone()));
        editor.mark_node_ids_persisted(pending.nodes.iter().map(|node| node.node_id.clone()));
        assert!(editor.graph_commit().nodes.is_empty());

        let graph = editor.into_session_graph();
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.read_model().messages.len(), 1);
    }

    #[test]
    fn message_delta_rejects_rewriting_the_existing_prefix() {
        let first = message("first", "one");
        let mut editor = editor();
        editor.append_active_conversation_messages(std::slice::from_ref(&first));
        let rewritten = message("first", "changed");
        assert!(
            editor
                .message_delta_if_current_preserved([&rewritten])
                .is_none()
        );
    }

    #[test]
    fn active_path_walk_is_bounded_on_a_parent_cycle() {
        const CHILD_ENV: &str = "LASH_FIG843_TURN_GRAPH_PATH_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            active_path_walk_is_bounded_scenario();
            return;
        }

        crate::test_watchdog::assert_exact_test_completes(
            "runtime::turn_graph_editor::tests::active_path_walk_is_bounded_on_a_parent_cycle",
            CHILD_ENV,
            "turn-graph path cycle check",
        );
    }

    fn active_path_walk_is_bounded_scenario() {
        let plugin_node = |node_id: &str, parent_node_id: &str| SessionNodeRecord {
            node_id: node_id.to_string(),
            parent_node_id: Some(parent_node_id.to_string()),
            timestamp: "2026-07-31T00:00:00Z".to_string(),
            payload: crate::SessionNodePayload::Plugin {
                plugin_type: "turn-graph-cycle-test".to_string(),
                body: crate::session_graph::SharedJsonValue::new(serde_json::json!({
                    "node": node_id
                })),
            },
        };
        let mut editor = editor();
        editor.append_appended_nodes(vec![
            plugin_node("turn-cycle-a", "turn-cycle-b"),
            plugin_node("turn-cycle-b", "turn-cycle-a"),
        ]);
        editor
            .append_builder
            .set_leaf_node_id(Some("turn-cycle-b".to_string()));

        assert_eq!(editor.active_path_nodes().len(), 2);
    }
}
