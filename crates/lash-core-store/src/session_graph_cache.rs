use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex as StdMutex};

use crate::session_graph::facade_ops::SessionNodeProjection;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, SessionReadModel};
use crate::session_graph_integrity::{ancestry_indices, graph_node_indices};
use crate::session_model::SessionHistoryRecord;
use crate::{BaseRenderCache, Message, NodeId};

/// Bound on the shared-base append delta. While `base` is shared, inserts
/// accumulate in `appended` and every builder creation or cache detach
/// clones them; the bound rebuilds a private base before that clone cost
/// can grow toward the full resident set.
const APPENDED_FOLD_BOUND: usize = 256;

/// Resident node-id → position index shared across a graph's snapshots.
///
/// The bulk map lives behind an `Arc` so detaching a shared cache for an
/// append copies the `Arc` rather than N ids. Whenever the base map is
/// privately held, inserts — and any accumulated delta — fold straight
/// into it, keeping `appended` empty. While the base is shared, inserts
/// accumulate in `appended` (consulted first on lookup) and the fold bound
/// rebuilds a private base rather than letting the delta — and therefore
/// every index clone — grow to the whole resident set.
#[derive(Clone, Debug)]
pub(crate) struct NodeIdIndex {
    base: Arc<HashMap<NodeId, usize>>,
    appended: HashMap<NodeId, usize>,
}

impl NodeIdIndex {
    fn from_resident(by_id: HashMap<NodeId, usize>) -> Self {
        Self {
            base: Arc::new(by_id),
            appended: HashMap::new(),
        }
    }

    pub(crate) fn get(&self, node_id: &str) -> Option<usize> {
        self.appended
            .get(node_id)
            .or_else(|| self.base.get(node_id))
            .copied()
    }

    fn insert(&mut self, node_id: NodeId, index: usize) {
        if let Some(base) = Arc::get_mut(&mut self.base) {
            base.extend(self.appended.drain());
            base.insert(node_id, index);
            return;
        }
        self.appended.insert(node_id, index);
        if self.appended.len() >= APPENDED_FOLD_BOUND {
            let mut folded = (*self.base).clone();
            folded.reserve(self.appended.len());
            folded.extend(self.appended.drain());
            self.base = Arc::new(folded);
        }
    }

    fn reserve(&mut self, additional: usize) {
        self.appended.reserve(additional);
    }
}

/// The unscoped read model: materialized `Arc<Vec>`s readers hold, plus the
/// owned tail appended nodes push into.
///
/// Appends never clone the shared vecs — the next read folds the tail into
/// fresh materialized vecs once, so a turn pays the read-model copy at read
/// time, not per append. `prompt_render_cache` is replaced on materialize,
/// keeping its `Arc` identity in step with the vec identity as before.
#[derive(Clone, Debug)]
struct ActiveReadModel {
    active_events: Arc<Vec<SessionHistoryRecord>>,
    active_messages: Arc<Vec<Message>>,
    prompt_render_cache: Arc<BaseRenderCache>,
    pending_events: Vec<SessionHistoryRecord>,
    pending_messages: Vec<Message>,
}

#[derive(Debug)]
pub(crate) struct SessionGraphCache {
    pub(crate) by_id: NodeIdIndex,
    pub(crate) active_path_indices: Vec<usize>,
    /// Behind a mutex because `read_model` materializes through `&self`
    /// while `append_node` pushes through `&mut self`.
    active_read: StdMutex<ActiveReadModel>,
    /// Memoized scoped read-model answers, keyed by the frame each was
    /// projected for.
    ///
    /// Identity is the point, not the saved work: the turn projection decides
    /// prefix agreement by comparing the `Arc` a read model handed out
    /// (`TurnGraphEditor::message_delta_if_current_preserved`), so a frame
    /// projection rebuilt per call would hand the turn's two readers two
    /// equal-but-distinct `Arc`s and force the whole-window reconciliation on
    /// every boundary. Cleared whenever the active path moves.
    frame_read_model: StdMutex<BTreeMap<String, SessionReadModel>>,
}

impl Clone for SessionGraphCache {
    fn clone(&self) -> Self {
        Self {
            by_id: self.by_id.clone(),
            active_path_indices: self.active_path_indices.clone(),
            active_read: StdMutex::new(
                self.active_read
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
            frame_read_model: StdMutex::new(
                self.frame_read_model
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

        let mut cache = Self {
            by_id: NodeIdIndex::from_resident(by_id),
            active_path_indices,
            active_read: StdMutex::new(ActiveReadModel {
                active_events: Arc::new(Vec::new()),
                active_messages: Arc::new(Vec::new()),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
                pending_events: Vec::new(),
                pending_messages: Vec::new(),
            }),
            frame_read_model: StdMutex::new(BTreeMap::new()),
        };
        cache.rebuild_read_model(graph);
        Ok(cache)
    }

    pub(crate) fn rebuild_read_model(&mut self, graph: &SessionGraph) {
        let mut active_messages = Vec::with_capacity(self.active_path_indices.len());
        let mut active_events = Vec::with_capacity(self.active_path_indices.len());
        for idx in &self.active_path_indices {
            let node = &graph.nodes[*idx];
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message() {
                if !message.is_transient() {
                    active_messages.push(message);
                }
                continue;
            }
        }
        *self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ActiveReadModel {
            active_events: Arc::new(active_events),
            active_messages: Arc::new(active_messages),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
            pending_events: Vec::new(),
            pending_messages: Vec::new(),
        };
        self.frame_read_model = StdMutex::new(BTreeMap::new());
    }

    /// The current unscoped read model, materializing pending appends once.
    ///
    /// Each pending tail folds independently: an event-only append neither
    /// copies nor replaces the message vec or the render cache built on it,
    /// and `Arc::make_mut` extends the existing allocation in place whenever
    /// no reader still holds the vec.
    pub(crate) fn active_read_model(&self) -> SessionReadModel {
        let read = &mut *self
            .active_read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !read.pending_events.is_empty() {
            Arc::make_mut(&mut read.active_events).append(&mut read.pending_events);
        }
        if !read.pending_messages.is_empty() {
            Arc::make_mut(&mut read.active_messages).append(&mut read.pending_messages);
            read.prompt_render_cache = Arc::new(BaseRenderCache::new());
        }
        SessionReadModel {
            active_events: Arc::clone(&read.active_events),
            messages: Arc::clone(&read.active_messages),
            prompt_render_cache: Arc::clone(&read.prompt_render_cache),
        }
    }

    pub(crate) fn scoped_read_model(
        &self,
        graph: &SessionGraph,
        frame_node_id: &crate::FrameNodeId,
    ) -> SessionReadModel {
        let mut memoized = self
            .frame_read_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(read_model) = memoized.get(frame_node_id.as_str()) {
            return read_model.clone();
        }
        let read_model = self.project_scoped_read_model(graph, frame_node_id);
        memoized.insert(frame_node_id.to_string(), read_model.clone());
        read_model
    }

    fn project_scoped_read_model(
        &self,
        graph: &SessionGraph,
        frame_node_id: &crate::FrameNodeId,
    ) -> SessionReadModel {
        let mut active_messages = Vec::with_capacity(self.active_path_indices.len());
        let mut active_events = Vec::with_capacity(self.active_path_indices.len());
        let mut in_frame = false;
        for idx in &self.active_path_indices {
            let node = &graph.nodes[*idx];
            if node.node_id == frame_node_id.as_str() {
                in_frame = true;
            } else if in_frame && matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
                break;
            }
            if !in_frame {
                continue;
            }
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message() {
                if !message.is_transient() {
                    active_messages.push(message);
                }
                continue;
            }
        }
        SessionReadModel {
            active_events: Arc::new(active_events),
            messages: Arc::new(active_messages),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        }
    }

    pub(crate) fn append_node(
        &mut self,
        node_index: usize,
        node: &SessionNodeRecord,
        previous_leaf_node_id: Option<&str>,
    ) {
        self.by_id.insert(node.node_id.clone(), node_index);
        let parent_matches_leaf = node.parent_node_id.as_deref() == previous_leaf_node_id;
        if !parent_matches_leaf {
            return;
        }
        self.frame_read_model = StdMutex::new(BTreeMap::new());
        self.active_path_indices.push(node_index);
        let read = self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(event) = node.event() {
            read.pending_events.push(event.clone());
        }
        if let Some(message) = node.message()
            && !message.is_transient()
        {
            read.pending_messages.push(message);
        }
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
    use std::collections::HashMap;

    #[test]
    fn appended_delta_folds_at_the_bound_and_stays_resolvable() {
        let mut index = NodeIdIndex::from_resident(HashMap::new());
        // A shared base forces inserts into the delta; the fold bound keeps
        // the delta — and therefore every index clone — bounded.
        let held = index.clone();
        for ordinal in 0..APPENDED_FOLD_BOUND + 8 {
            index.insert(format!("n{ordinal}").into(), ordinal);
        }
        assert!(index.appended.len() < APPENDED_FOLD_BOUND);
        for ordinal in 0..APPENDED_FOLD_BOUND + 8 {
            assert_eq!(index.get(&format!("n{ordinal}")), Some(ordinal));
        }
        // The held clone's view froze at clone time.
        assert!(held.get("n0").is_none());
    }

    #[test]
    fn privately_held_base_absorbs_appends_without_a_delta() {
        let mut index = NodeIdIndex::from_resident(HashMap::new());
        for ordinal in 0..APPENDED_FOLD_BOUND * 2 {
            index.insert(format!("n{ordinal}").into(), ordinal);
        }
        assert!(index.appended.is_empty());
        assert_eq!(index.base.len(), APPENDED_FOLD_BOUND * 2);
        assert_eq!(index.get("n511"), Some(511));

        // Once a sharing clone drops, the next insert folds the accumulated
        // delta into the now-private base.
        let mut shared = NodeIdIndex::from_resident(HashMap::new());
        let held = shared.clone();
        shared.insert("a".into(), 0);
        shared.insert("b".into(), 1);
        drop(held);
        shared.insert("c".into(), 2);
        assert!(shared.appended.is_empty());
        assert_eq!(shared.get("a"), Some(0));
        assert_eq!(shared.get("c"), Some(2));
    }
}
