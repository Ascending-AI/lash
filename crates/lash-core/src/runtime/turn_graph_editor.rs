use lash_sansio::core_support::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::facade_support::{SessionGraphFacadeOps, SessionNodeProjection};
use crate::session_graph::SessionReadModel;
use crate::session_graph::build_active_read_projection;
use crate::session_model::SessionHistoryRecord;
#[cfg(test)]
use crate::store::GraphAppend;
use crate::{BaseRenderCache, Message, MessageSequence, SessionGraph, SessionNodeRecord};

#[derive(Debug)]
pub(super) struct TurnGraphEditor {
    base_graph: Arc<SessionGraph>,
    active_events: Arc<Vec<SessionHistoryRecord>>,
    active_messages: MessageSequence,
    current_frame_node_id: Option<crate::FrameNodeId>,
    append_builder: crate::session_graph::SessionGraphAppendBuilder,
    pre_turn_append_leaf: Option<String>,
    append_builder_minted_node_ids: HashSet<String>,
    appended_nodes: Vec<SessionNodeRecord>,
    appended_node_indices: HashMap<String, usize>,
    committed_node_ids: HashSet<String>,
    clock: Arc<dyn crate::Clock>,
    projection_diagnostics: Vec<ReadProjectionDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReadProjectionDiagnostic {
    pub(super) durably_appended_messages: usize,
    pub(super) observation_only_messages: usize,
    pub(super) id_mismatches: Vec<String>,
}

impl TurnGraphEditor {
    pub(super) fn new(
        base_graph: Arc<SessionGraph>,
        base_read_model: SessionReadModel,
        current_frame_node_id: Option<crate::FrameNodeId>,
        draft_namespace: &str,
        clock: Arc<dyn crate::Clock>,
        persisted_node_ids: HashSet<String>,
    ) -> Self {
        let append_builder = base_graph.append_builder_in_namespace(draft_namespace);
        let pre_turn_append_leaf = append_builder.leaf_node_id().cloned();
        let active_messages = MessageSequence::from_base(base_read_model.messages);
        Self {
            committed_node_ids: persisted_node_ids,
            base_graph,
            active_events: base_read_model.active_events,
            active_messages,
            current_frame_node_id,
            append_builder,
            pre_turn_append_leaf,
            append_builder_minted_node_ids: HashSet::new(),
            appended_nodes: Vec::new(),
            appended_node_indices: HashMap::new(),
            clock,
            projection_diagnostics: Vec::new(),
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
        self.record_append_builder_nodes(nodes);
    }

    /// The messages `next` appends to the turn's read state, or `None` when
    /// `next` rewrites the prefix this editor already holds.
    ///
    /// The answer is normally a pointer comparison: `next` and
    /// `self.active_messages` are ropes over the same `base` allocation — the
    /// read model this editor was opened on — so a preserved prefix is
    /// witnessed by rope identity and only the turn-sized delta is inspected.
    /// The content walk below is the fallback for a `next` that was rebuilt
    /// (a compaction transform, a restored sequence), and it is bounded by the
    /// active window rather than by the session's history because it compares
    /// messages directly instead of materializing JSON for each one.
    pub(super) fn message_delta_if_current_preserved(
        &self,
        next: &MessageSequence,
    ) -> Option<Vec<Message>> {
        if let Some(tail) = self.active_messages.preserved_extension_delta(next) {
            // The shared prefix is transient-filtered by construction: every
            // writer of `active_messages` (the read model it is based on,
            // `append_events`, `append_active_conversation_messages`) drops
            // transient messages before they land.
            debug_assert!(
                !self.active_messages.iter().any(Message::is_transient),
                "active read messages carry no transient message"
            );
            return Some(
                tail.iter()
                    .filter(|message| !message.is_transient())
                    .cloned()
                    .collect(),
            );
        }

        // This message-only fallback cannot use `active_read_prefix`: this
        // method must reject a shorter `next` after matching all of its
        // messages (`current.next().is_none()`), while the graph resolver
        // intentionally stops when target messages are exhausted so it can
        // retain graph nodes, including protocol nodes, for projection.
        let mut current = self.active_messages.iter();
        let mut appended = Vec::new();
        for message in next.iter().filter(|message| !message.is_transient()) {
            if let Some(current_message) = current.next() {
                if !current_message.content_equals(message) {
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
        self.record_append_builder_nodes(nodes);
        self.active_messages.extend(appendable_messages);
    }

    pub(super) fn project_active_read_state(&mut self, messages: &[Message]) {
        let active_path = self.active_path_nodes();
        let projection = build_active_read_projection(active_path.iter().copied(), messages);

        // Prompt projections may omit or rewrite any part of the readable
        // history. Durable ancestry is independent: only messages that are not
        // already on the active graph path are appended, always through the
        // graph builder's current (durable) leaf.
        //
        // Durable append selection keys on `Message.id`; ids must be unique
        // within a frame for the frame's lifetime.
        let mut durable_messages = active_path
            .iter()
            .filter_map(|node| node.message())
            .map(|message| (message.id.clone(), message))
            .collect::<HashMap<_, _>>();
        let mut appended_messages = Vec::new();
        let mut observation_only_messages = 0usize;
        let mut id_mismatches = Vec::new();
        for message in messages.iter().filter(|message| !message.is_transient()) {
            if let Some(durable_message) = durable_messages.get(&message.id) {
                observation_only_messages += 1;
                if !durable_message.content_equals(message) {
                    id_mismatches.push(message.id.clone());
                }
                continue;
            }
            durable_messages.insert(message.id.clone(), message.clone());
            appended_messages.push(message.clone());
        }
        let durably_appended_messages = appended_messages.len();
        self.append_active_conversation_messages(&appended_messages);
        self.projection_diagnostics.push(ReadProjectionDiagnostic {
            durably_appended_messages,
            observation_only_messages,
            id_mismatches,
        });

        // Keep the turn-facing read state projected without allowing that
        // projection's synthetic tail to select the durable commit parent.
        self.active_events = Arc::new(projection.active_events);
        self.active_messages = MessageSequence::from_owned(projection.active_messages);
    }

    pub(super) fn take_projection_diagnostics(&mut self) -> Vec<ReadProjectionDiagnostic> {
        std::mem::take(&mut self.projection_diagnostics)
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
        // Commit-draft append invariant: every node staged by this editor was
        // minted by its append builder, and the first one extends the leaf that
        // builder captured before the turn. Projection-only tail nodes must
        // never enter this collection or choose its parent.
        debug_assert!(
            self.appended_nodes
                .iter()
                .all(|node| self.append_builder_minted_node_ids.contains(&node.node_id))
        );
        debug_assert_eq!(
            self.appended_nodes
                .first()
                .and_then(|node| node.parent_node_id.as_ref()),
            self.appended_nodes
                .first()
                .and(self.pre_turn_append_leaf.as_ref())
        );
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
                // The base graph was validated on construction and the append builder enforces
                // unique ids plus a continuous parent chain, so this merge preserves integrity.
                SessionGraph::from_validated_nodes(nodes, leaf_node_id)
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

    /// Accept only nodes returned by `SessionGraphAppendBuilder`; this is the
    /// seam that keeps pending durable commits anchored to the pre-turn leaf.
    fn record_append_builder_nodes(&mut self, nodes: Vec<SessionNodeRecord>) {
        self.appended_node_indices.reserve(nodes.len());
        self.appended_nodes.reserve(nodes.len());
        for node in nodes {
            self.append_builder_minted_node_ids
                .insert(node.node_id.clone());
            self.appended_node_indices
                .insert(node.node_id.clone(), self.appended_nodes.len());
            self.appended_nodes.push(node);
        }
    }

    #[cfg(test)]
    fn append_unchecked_nodes(&mut self, nodes: Vec<SessionNodeRecord>) {
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
            parts: crate::shared_parts(vec![crate::Part::text(
                format!("{id}.p0"),
                text.to_string(),
                None,
            )]),
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
            .message_delta_if_current_preserved(&MessageSequence::from_owned(vec![first.clone()]))
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
                .message_delta_if_current_preserved(&MessageSequence::from_owned(vec![rewritten]))
                .is_none()
        );
    }

    /// The turn's read state and the protocol machine's message list are ropes
    /// over the same read-model `base`, so agreement on the history they share
    /// is settled by rope identity: no message on the base is ever inspected,
    /// however long the session is. The probe base carries a message whose
    /// content differs from anything a walk would accept, so a fallback that
    /// compared content would reject this delta instead of naming it.
    #[test]
    fn a_shared_read_model_base_witnesses_agreement_without_walking_history() {
        let mut graph = SessionGraph::default();
        graph.append_active_read_delta(&[message("history", "committed")]);
        let graph = Arc::new(graph);
        let base_read_model = graph.read_model();
        let base = Arc::clone(&base_read_model.messages);
        let editor = TurnGraphEditor::new(
            graph,
            base_read_model,
            None,
            "turn-graph-editor-witness-test",
            Arc::new(crate::SystemClock),
            HashSet::new(),
        );

        let next = MessageSequence::from_base_and_delta(base, vec![message("turn", "new")]);

        let delta = editor
            .message_delta_if_current_preserved(&next)
            .expect("a rope over the editor's own base preserves its prefix");
        assert_eq!(
            delta
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn"]
        );
    }

    /// A transform that rebuilds the message list drops the witness, and the
    /// content fallback reconciles it exactly as the serialize-and-compare
    /// walk it replaced did.
    #[test]
    fn a_rebuilt_message_list_falls_back_to_content_reconciliation() {
        let mut editor = editor();
        editor.append_active_conversation_messages(&[
            message("first", "one"),
            message("second", "two"),
        ]);

        let preserved = MessageSequence::from_owned(vec![
            message("first", "one"),
            message("second", "two"),
            message("third", "three"),
        ]);
        assert_eq!(
            editor
                .message_delta_if_current_preserved(&preserved)
                .expect("an owned list that preserves the prefix still reconciles")
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["third"]
        );

        let rewritten = MessageSequence::from_owned(vec![
            message("first", "one"),
            message("second", "rewritten"),
            message("third", "three"),
        ]);
        assert!(
            editor
                .message_delta_if_current_preserved(&rewritten)
                .is_none(),
            "rewriting the middle of the prefix is not an append"
        );

        let truncated = MessageSequence::from_owned(vec![message("first", "one")]);
        assert!(
            editor
                .message_delta_if_current_preserved(&truncated)
                .is_none(),
            "dropping the tail of the prefix is not an append"
        );
    }

    #[test]
    fn projected_content_mismatch_for_a_durable_id_stays_observation_only() {
        let mut editor = editor();
        let durable = message("durable", "original");
        editor.append_active_conversation_messages(std::slice::from_ref(&durable));
        let pending = editor.graph_commit();
        editor.mark_node_ids_persisted(pending.nodes.iter().map(|node| node.node_id.clone()));

        editor.project_active_read_state(&[message("durable", "projected")]);

        assert!(editor.graph_commit().nodes.is_empty());
        assert_eq!(editor.message_sequence()[0].parts[0].content, "projected");
        assert_eq!(
            editor.take_projection_diagnostics(),
            vec![ReadProjectionDiagnostic {
                durably_appended_messages: 0,
                observation_only_messages: 1,
                id_mismatches: vec!["durable".to_string()],
            }]
        );
        let durable_graph = editor.into_session_graph();
        assert_eq!(
            durable_graph.read_model().messages[0].parts[0].content,
            "original"
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
        editor.append_unchecked_nodes(vec![
            plugin_node("turn-cycle-a", "turn-cycle-b"),
            plugin_node("turn-cycle-b", "turn-cycle-a"),
        ]);
        editor
            .append_builder
            .set_leaf_node_id(Some("turn-cycle-b".to_string()));

        assert_eq!(editor.active_path_nodes().len(), 2);
    }
}
