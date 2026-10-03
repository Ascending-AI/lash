use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::{Arc, Mutex as StdMutex};

use crate::session_graph::facade_ops::SessionNodeProjection;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, SessionReadModel};
use crate::session_graph_integrity::{ancestry_indices, graph_node_indices};
use crate::session_model::SessionHistoryRecord;
use crate::{BaseRenderCache, Message, NodeId};
use lash_sansio::{AppendVec, same_history_record, same_message};

/// An index snapshot sees the writes preceding its length. Appends at the
/// shared tip add one write without copying entries held by older readers.
/// A writer branching from an older snapshot gets a private index of that
/// prefix; ordinary commits always advance the tip.
#[derive(Debug)]
struct SnapshotIndex<K, V> {
    data: Arc<StdMutex<IndexWrites<K, V>>>,
    len: usize,
}

#[derive(Debug)]
struct IndexWrites<K, V> {
    writes: Vec<(K, V)>,
    by_key: HashMap<K, Vec<usize>>,
    holders: BTreeMap<usize, usize>,
}

impl<K: Eq + Hash + Clone, V: Clone> SnapshotIndex<K, V> {
    fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut data = IndexWrites {
            writes: Vec::new(),
            by_key: HashMap::new(),
            holders: BTreeMap::new(),
        };
        for (key, value) in entries {
            data.push(key, value);
        }
        let len = data.writes.len();
        data.holders.insert(len, 1);
        Self {
            len,
            data: Arc::new(StdMutex::new(data)),
        }
    }

    fn get<Q: Eq + Hash + ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let data = self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let versions = data.by_key.get(key)?;
        let ordinal = versions
            .partition_point(|position| *position < self.len)
            .checked_sub(1)?;
        Some(data.writes[versions[ordinal]].1.clone())
    }

    fn insert(&mut self, key: K, value: V) {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let visible_tip = data.holders.last_key_value().map_or(0, |(len, _)| *len);
        data.truncate(visible_tip);
        if self.len < data.writes.len() {
            let private = Self::from_entries(data.writes[..self.len].iter().cloned());
            drop(data);
            *self = private;
            self.insert(key, value);
            return;
        }
        data.push(key, value);
        data.unregister(self.len);
        self.len += 1;
        *data.holders.entry(self.len).or_default() += 1;
    }

    fn reserve(&mut self, additional: usize) {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        data.writes.reserve(additional);
        data.by_key.reserve(additional);
    }
}

impl<K, V> Clone for SnapshotIndex<K, V> {
    fn clone(&self) -> Self {
        let mut data = self
            .data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *data.holders.entry(self.len).or_default() += 1;
        Self {
            data: Arc::clone(&self.data),
            len: self.len,
        }
    }
}

impl<K, V> Drop for SnapshotIndex<K, V> {
    fn drop(&mut self) {
        self.data
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unregister(self.len);
    }
}

impl<K, V> IndexWrites<K, V> {
    fn unregister(&mut self, len: usize) {
        if let Some(count) = self.holders.get_mut(&len) {
            *count -= 1;
            if *count == 0 {
                self.holders.remove(&len);
            }
        }
    }
}

impl<K: Eq + Hash + Clone, V> IndexWrites<K, V> {
    // A discarded speculative writer must not force the next writer to copy
    // the resident prefix. Only writes no remaining snapshot can see retire.
    fn truncate(&mut self, len: usize) {
        while self.writes.len() > len {
            if let Some((key, _)) = self.writes.pop()
                && let Some(versions) = self.by_key.get_mut(&key)
            {
                versions.pop();
                if versions.is_empty() {
                    self.by_key.remove(&key);
                }
            }
        }
    }

    fn push(&mut self, key: K, value: V) {
        self.by_key
            .entry(key.clone())
            .or_default()
            .push(self.writes.len());
        self.writes.push((key, value));
    }
}

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

    fn reserve(&mut self, additional: usize) {
        self.0.reserve(additional);
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
    children: SnapshotIndex<usize, AppendVec<usize>>,
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
        let mut children = HashMap::<usize, Vec<usize>>::new();
        for (index, node) in graph.nodes.iter().enumerate() {
            if let Some(parent) = node.parent_node_id.as_ref().and_then(|id| by_id.get(id)) {
                children.entry(*parent).or_default().push(index);
            }
        }
        let mut cache = Self {
            by_id: NodeIdIndex::from_resident(by_id),
            active_path_indices: AppendVec::from(active_path_indices),
            active_frame_indices: AppendVec::from(active_frame_indices),
            children: SnapshotIndex::from_entries(
                children
                    .into_iter()
                    .map(|(index, children)| (index, AppendVec::from(children))),
            ),
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

    pub(crate) fn append_node(
        &mut self,
        node_index: usize,
        node: &SessionNodeRecord,
        previous_leaf_node_id: Option<&str>,
    ) {
        if let Some(parent_index) = node
            .parent_node_id
            .as_deref()
            .and_then(|id| self.by_id.get(id))
        {
            let mut children = self.children.get(&parent_index).unwrap_or_default();
            children.push(node_index);
            self.children.insert(parent_index, children);
        }
        self.by_id.insert(node.node_id.clone(), node_index);
        let parent_matches_leaf = node.parent_node_id.as_deref() == previous_leaf_node_id;
        if !parent_matches_leaf {
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

    pub(crate) fn children_of(&self, node_index: usize) -> AppendVec<usize> {
        self.children.get(&node_index).unwrap_or_default()
    }

    pub(crate) fn reserve_append_capacity(
        &mut self,
        additional_nodes: usize,
        additional_messages: usize,
    ) {
        self.by_id.reserve(additional_nodes);
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
            Arc::ptr_eq(&initial.0.data, &index.0.data),
            "advancing commits never copy the base"
        );
        assert_eq!(initial.get("root"), Some(0));
        assert_eq!(initial.get("derived-1"), None);
        assert_eq!(index.get("draft-1"), None);
        assert_eq!(index.get("derived-1"), Some(1));
        assert_eq!(index.get("derived-1999"), Some(1999));
    }
}
