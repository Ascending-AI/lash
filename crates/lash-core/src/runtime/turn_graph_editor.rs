use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::session_graph::SessionReadModel;
use crate::session_graph::build_active_read_replacement;
use crate::session_model::SessionHistoryRecord;
#[cfg(test)]
use crate::store::GraphCommitDelta;
use crate::{BaseRenderCache, Message, MessageSequence, SessionGraph, SessionNodeRecord};

#[derive(Debug)]
pub(super) struct TurnGraphEditor {
    base_graph: Arc<SessionGraph>,
    active_events: Arc<Vec<SessionHistoryRecord>>,
    active_messages: MessageSequence,
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
            None,
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
    pub(super) fn graph_commit(&self) -> GraphCommitDelta {
        let nodes = self
            .base_graph
            .nodes
            .iter()
            .chain(self.appended_nodes.iter())
            .filter(|node| !self.committed_node_ids.contains(&node.node_id))
            .cloned()
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            GraphCommitDelta::Unchanged {
                leaf_node_id: self.leaf_node_id(),
            }
        } else {
            GraphCommitDelta::Append {
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
        while let Some(node_id) = current {
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
