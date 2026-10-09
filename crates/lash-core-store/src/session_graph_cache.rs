use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::session_graph::facade_ops::SessionNodeProjection;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, SessionReadModel};
use crate::session_graph_integrity::{ancestry_indices, graph_node_indices};
use crate::session_model::SessionHistoryRecord;
use crate::snapshot_index::SnapshotIndex;
use crate::{BaseRenderCache, Message, NodeId};
use lash_sansio::{AppendVec, same_history_record, same_message};

#[derive(Clone, Debug)]
pub(crate) struct NodeIdIndex(SnapshotIndex<NodeId, Option<usize>>);

impl NodeIdIndex {
    fn from_resident(by_id: HashMap<NodeId, usize>) -> Self {
        Self(SnapshotIndex::from_entries(
            by_id.into_iter().map(|(id, index)| (id, Some(index))),
        ))
    }

    pub(crate) fn get(&self, node_id: &str) -> Option<usize> {
        self.0.get(node_id).flatten()
    }

    fn insert(&mut self, node_id: NodeId, index: usize) {
        self.0.insert(node_id, Some(index));
    }

    pub(crate) fn rename(&mut self, from: &NodeId, to: NodeId, index: usize) {
        self.0.insert(from.clone(), None);
        self.0.insert(to, Some(index));
    }
}

/// The read model of the current frame: the shared sequences readers hold,
/// plus the owned tail appended nodes push into.
///
/// The next read folds the tail onto the shared sequences in place: a
/// reader's snapshot is a prefix of the same buffer, so neither an append
/// nor a held reader copies the frame (FIG-4060), and every retained read
/// model shares one frame's worth of messages (FIG-4059).
/// `prompt_render_cache` is replaced when messages fold, keeping its `Arc`
/// identity in step with the message sequence, and extends the previous
/// cache's render instead of re-rendering the frame.
#[derive(Clone, Debug)]
struct ActiveReadModel {
    active_events: AppendVec<SessionHistoryRecord>,
    active_messages: AppendVec<Message>,
    prompt_render_cache: Arc<BaseRenderCache>,
    pending_events: Vec<SessionHistoryRecord>,
    pending_messages: Vec<Message>,
}

#[derive(Debug)]
pub(crate) struct SessionGraphCache {
    pub(crate) by_id: NodeIdIndex,
    pub(crate) active_path_indices: AppendVec<usize>,
    pub(crate) active_frame_indices: AppendVec<usize>,
    /// Every child edge, as one write of the child's position under its
    /// parent's.
    children: SnapshotIndex<usize, usize>,
    /// The one memoized projection, of the active path from its last
    /// `FrameOpen` (ADR 0112 §9).
    ///
    /// Identity is the point, not only the saved work: the turn projection
    /// decides prefix agreement by comparing the shared sequence a read
    /// model handed out (`TurnGraphEditor::message_delta_if_current_preserved`,
    /// [`lash_sansio::AppendVec::ptr_eq`]), so every reader of one resident
    /// graph shares these sequences.
    ///
    /// Behind a mutex because `read_model` materializes through `&self`
    /// while `append_node` pushes through `&mut self`.
    active_read: StdMutex<ActiveReadModel>,
}

impl Clone for SessionGraphCache {
    fn clone(&self) -> Self {
        Self {
            by_id: self.by_id.clone(),
            active_path_indices: self.active_path_indices.clone(),
            active_frame_indices: self.active_frame_indices.clone(),
            children: self.children.clone(),
            active_read: StdMutex::new(
                self.active_read
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
        }
    }
}

impl SessionGraphCache {
    pub(crate) fn build(graph: &SessionGraph) -> Result<Self, crate::StoreError> {
        let by_id = graph_node_indices(graph)?;
        let mut active_path_indices =
            ancestry_indices(graph, &by_id, graph.leaf_node_id.as_deref())?;
        active_path_indices.reverse();

        let active_frame_indices = active_path_indices
            .iter()
            .copied()
            .filter(|index| {
                matches!(
                    graph.nodes[*index].payload,
                    SessionNodePayload::FrameOpen { .. }
                )
            })
            .collect::<Vec<_>>();
        let children = graph.nodes.iter().enumerate().filter_map(|(index, node)| {
            let parent = node.parent_node_id.as_ref().and_then(|id| by_id.get(id))?;
            Some((*parent, index))
        });
        let children = SnapshotIndex::from_entries(children);
        let mut cache = Self {
            by_id: NodeIdIndex::from_resident(by_id),
            active_path_indices: AppendVec::from(active_path_indices),
            active_frame_indices: AppendVec::from(active_frame_indices),
            children,
            active_read: StdMutex::new(ActiveReadModel {
                active_events: AppendVec::new(),
                active_messages: AppendVec::new(),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
                pending_events: Vec::new(),
                pending_messages: Vec::new(),
            }),
        };
        cache.rebuild_read_model(graph);
        Ok(cache)
    }

    pub(crate) fn rebuild_read_model(&mut self, graph: &SessionGraph) {
        let frame_start = self
            .active_frame_indices
            .last()
            .and_then(|frame_index| {
                self.active_path_indices
                    .iter()
                    .position(|index| index == frame_index)
            })
            .unwrap_or(0);
        let frame_path = &self.active_path_indices[frame_start..];
        let mut active_messages = Vec::with_capacity(frame_path.len());
        let mut active_events = Vec::with_capacity(frame_path.len());
        for idx in frame_path {
            let node = &graph.nodes[*idx];
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message()
                && !message.is_transient()
            {
                active_messages.push(message);
            }
        }
        *self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ActiveReadModel {
            active_events: AppendVec::from(active_events),
            active_messages: AppendVec::from(active_messages),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
            pending_events: Vec::new(),
            pending_messages: Vec::new(),
        };
    }

    /// The current frame's read model, materializing pending appends once.
    ///
    /// Each pending tail folds independently: an event-only append neither
    /// replaces the message sequence nor the render cache built on it. A
    /// fold appends to the shared sequences, adopting what another holder
    /// of their buffers (a turn's read state) already appended.
    pub(crate) fn active_read_model(&self) -> SessionReadModel {
        let read = &mut *self
            .active_read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !read.pending_events.is_empty() {
            let pending = std::mem::take(&mut read.pending_events);
            read.active_events
                .extend_adopting(pending, same_history_record);
        }
        if !read.pending_messages.is_empty() {
            let prefix_len = read.active_messages.len();
            let pending = std::mem::take(&mut read.pending_messages);
            read.active_messages.extend_adopting(pending, same_message);
            read.prompt_render_cache = Arc::new(BaseRenderCache::extending(
                Arc::clone(&read.prompt_render_cache),
                prefix_len,
            ));
        }
        SessionReadModel {
            active_events: read.active_events.clone(),
            messages: read.active_messages.clone(),
            prompt_render_cache: Arc::clone(&read.prompt_render_cache),
        }
    }

    pub(crate) fn replace_conversation_reply(&mut self, event: &SessionHistoryRecord) {
        let SessionHistoryRecord::Conversation(conversation) = event else {
            return;
        };
        let message = conversation.to_message();
        let read = self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(index) = read.active_events.iter().position(|existing| matches!(
            existing, SessionHistoryRecord::Conversation(existing) if existing.id == conversation.id
        )) {
            let mut tail = read.active_events[index..].to_vec();
            tail[0] = event.clone();
            read.active_events.replace_from(index, tail);
        }
        if let Some(index) = read
            .active_messages
            .iter()
            .position(|existing| existing.id == message.id)
        {
            let mut tail = read.active_messages[index..].to_vec();
            tail[0] = message.clone();
            read.active_messages.replace_from(index, tail);
        }
        for pending in &mut read.pending_events {
            if matches!(pending, SessionHistoryRecord::Conversation(existing) if existing.id == conversation.id)
            {
                *pending = event.clone();
            }
        }
        for pending in &mut read.pending_messages {
            if pending.id == message.id {
                *pending = message.clone();
            }
        }
    }

    pub(crate) fn append_node(
        &mut self,
        node_index: usize,
        node: &SessionNodeRecord,
        extends_leaf: bool,
    ) {
        if let Some(parent_index) = node
            .parent_node_id
            .as_deref()
            .and_then(|id| self.by_id.get(id))
        {
            self.children.insert(parent_index, node_index);
        }
        self.by_id.insert(node.node_id.clone(), node_index);
        if !extends_leaf {
            return;
        }
        self.active_path_indices.push(node_index);
        if matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
            self.active_frame_indices.push(node_index);
        }
        let read = self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
            // A pending `FrameOpen` moves the projection's start to the new
            // frame before its commit (ADR 0112 §9).
            *read = ActiveReadModel {
                active_events: AppendVec::new(),
                active_messages: AppendVec::new(),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
                pending_events: Vec::new(),
                pending_messages: Vec::new(),
            };
        }
        if let Some(event) = node.event() {
            read.pending_events.push(event.clone());
        }
        if let Some(message) = node.message()
            && !message.is_transient()
        {
            read.pending_messages.push(message);
        }
    }

    pub(crate) fn children_of(&self, node_index: usize) -> Vec<usize> {
        self.children.all(&node_index)
    }

    pub(crate) fn reserve_append_capacity(
        &mut self,
        additional_nodes: usize,
        additional_messages: usize,
    ) {
        self.active_path_indices.reserve(additional_nodes);
        if additional_messages > 0 {
            self.active_read
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pending_messages
                .reserve(additional_messages);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_snapshots_keep_their_version_across_appends_and_renames() {
        let mut index = NodeIdIndex::from_resident(HashMap::from([("root".into(), 0)]));
        let initial = index.clone();
        for ordinal in 1..2000 {
            let draft = NodeId::fixture(format!("draft-{ordinal}"));
            index.insert(draft.clone(), ordinal);
            let before_rename = index.clone();
            index.rename(
                &draft,
                NodeId::fixture(format!("derived-{ordinal}")),
                ordinal,
            );
            assert_eq!(before_rename.get(draft.as_str()), Some(ordinal));
            assert_eq!(before_rename.get(&format!("derived-{ordinal}")), None);
        }
        assert!(
            initial.0.shares_writes_with(&index.0),
            "advancing commits never copy the base"
        );
        assert_eq!(initial.get("root"), Some(0));
        assert_eq!(initial.get("derived-1"), None);
        assert_eq!(index.get("draft-1"), None);
        assert_eq!(index.get("derived-1"), Some(1));
        assert_eq!(index.get("derived-1999"), Some(1999));
    }
}
