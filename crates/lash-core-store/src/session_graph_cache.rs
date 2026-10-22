use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::BaseRenderCache;
use crate::Message;
use crate::persistent_map::PersistentMap;
use crate::session_graph::facade_ops::SessionNodeProjection;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, SessionReadModel};
use crate::session_graph_integrity::{ancestry_indices, graph_node_indices, validate_structure};
use crate::session_model::SessionHistoryRecord;
use lash_protocol_types::{AppendVec, same_history_record, same_message};

use lash_vocabulary::identity::NodeId;

/// Every resident node's position by id. Persistent: every snapshot of the
/// graph shares it, and the store's window and each runtime that adopted it
/// extend their copies independently without copying it (FIG-6889).
#[derive(Clone, Debug)]
pub(crate) struct NodeIdIndex(PersistentMap<NodeId, usize>);

impl NodeIdIndex {
    fn from_resident(by_id: HashMap<NodeId, usize>) -> Self {
        Self(by_id.into_iter().collect())
    }

    pub(crate) fn get(&self, node_id: &str) -> Option<usize> {
        self.0.get(node_id).copied()
    }

    fn insert(&mut self, node_id: NodeId, index: usize) {
        self.0.insert(node_id, index);
    }

    pub(crate) fn rename(&mut self, from: &NodeId, to: NodeId, index: usize) {
        self.0.remove(from.as_str());
        self.0.insert(to, index);
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
    assistant_ids: PersistentMap<String, usize>,
    prompt_render_cache: Arc<BaseRenderCache>,
    pending_events: Vec<SessionHistoryRecord>,
    pending_messages: Vec<Message>,
}

#[derive(Debug)]
pub(crate) struct SessionGraphCache {
    pub(crate) by_id: NodeIdIndex,
    pub(crate) active_path_indices: AppendVec<usize>,
    pub(crate) active_frame_indices: AppendVec<usize>,
    /// Every child edge: each parent's children's positions, in append
    /// order.
    children: PersistentMap<usize, Vec<usize>>,
    /// The graph passed the full structural-integrity check, and every
    /// append since kept it (FIG-6889): an append whose id is fresh and
    /// whose parent is resident keeps it, any other edit withdraws it.
    proven: AtomicBool,
    /// The one memoized projection, of the active path from its last
    /// `FrameOpen` (ADR 0112 §9).
    ///
    /// Identity is the point, not only the saved work: the turn projection
    /// decides prefix agreement by comparing the shared sequence a read
    /// model handed out (`TurnGraphEditor::message_delta_if_current_preserved`,
    /// [`lash_protocol_types::AppendVec::ptr_eq`]), so every reader of one resident
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
            proven: AtomicBool::new(self.proven()),
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
        let active_path_indices = ancestry_indices(graph, &by_id, graph.leaf_node_id.as_deref())?;
        Ok(Self::from_indices(graph, by_id, active_path_indices, false))
    }

    /// The cache of a graph that passes the full structural-integrity check
    /// ([`validate_structure`]), holding the proof.
    pub(crate) fn build_validated(graph: &SessionGraph) -> Result<Self, crate::StoreError> {
        let (by_id, active_path_indices) = validate_structure(graph)?;
        Ok(Self::from_indices(graph, by_id, active_path_indices, true))
    }

    fn from_indices(
        graph: &SessionGraph,
        by_id: HashMap<NodeId, usize>,
        mut active_path_indices: Vec<usize>,
        proven: bool,
    ) -> Self {
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
        let mut children = PersistentMap::default();
        for (index, node) in graph.nodes.iter().enumerate() {
            if let Some(parent) = node.parent_node_id.as_ref().and_then(|id| by_id.get(id)) {
                children.upsert(*parent, vec![index], |held: &mut Vec<usize>, _| {
                    held.push(index);
                });
            }
        }
        let mut cache = Self {
            by_id: NodeIdIndex::from_resident(by_id),
            active_path_indices: AppendVec::from(active_path_indices),
            active_frame_indices: AppendVec::from(active_frame_indices),
            children,
            proven: AtomicBool::new(proven),
            active_read: StdMutex::new(ActiveReadModel {
                active_events: AppendVec::new(),
                active_messages: AppendVec::new(),
                assistant_ids: PersistentMap::default(),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
                pending_events: Vec::new(),
                pending_messages: Vec::new(),
            }),
        };
        cache.rebuild_read_model(graph);
        cache
    }

    /// Whether the graph holds the structural-integrity proof.
    pub(crate) fn proven(&self) -> bool {
        self.proven.load(Ordering::Acquire)
    }

    /// Record that the graph just passed the full check.
    pub(crate) fn prove(&self) {
        self.proven.store(true, Ordering::Release);
    }

    /// Withdraw the proof: the next check runs in full.
    pub(crate) fn withdraw_proof(&mut self) {
        *self.proven.get_mut() = false;
    }

    /// Whether appending a node with `node_id` under `parent` keeps the
    /// proof. The id must be fresh and the parent resident (or absent, a
    /// new root), and the id must not be the anchor's external parent, the
    /// one edge a proven graph leaves dangling: then no resident node names
    /// the new one, so no cycle or dangling edge can form.
    pub(crate) fn append_keeps_proof(
        &self,
        node_id: &NodeId,
        parent: Option<&NodeId>,
        external_parent: Option<&NodeId>,
    ) -> bool {
        self.by_id.get(node_id).is_none()
            && parent.is_none_or(|parent| self.by_id.get(parent).is_some())
            && external_parent != Some(node_id)
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
        let mut assistant_ids = PersistentMap::default();
        for idx in frame_path {
            let node = &graph.nodes[*idx];
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message()
                && !message.is_transient()
            {
                if message.role == crate::MessageRole::Assistant {
                    assistant_ids.upsert(
                        message.id.clone(),
                        active_messages.len(),
                        |first, index| {
                            *first = (*first).min(index);
                        },
                    );
                }
                active_messages.push(message);
            }
        }
        *self
            .active_read
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ActiveReadModel {
            active_events: AppendVec::from(active_events),
            active_messages: AppendVec::from(active_messages),
            assistant_ids,
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
            pending_events: Vec::new(),
            pending_messages: Vec::new(),
        };
    }

    /// The same active-frame projection owns both the message sequence and
    /// its assistant-id membership. Rebuilds and frame changes replace both.
    pub(crate) fn contains_active_assistant_message_id_before(&self, id: &str, end: usize) -> bool {
        self.active_read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .assistant_ids
            .get(id)
            .is_some_and(|index| *index < end)
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
        // The marked reply is one of the newest records (FIG-6889).
        if let Some(index) = read.active_events.iter().rposition(|existing| matches!(
            existing, SessionHistoryRecord::Conversation(existing) if existing.id == conversation.id
        )) {
            let mut tail = read.active_events[index..].to_vec();
            tail[0] = event.clone();
            read.active_events.replace_from(index, tail);
        }
        if let Some(index) = read
            .active_messages
            .iter()
            .rposition(|existing| existing.id == message.id)
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
            self.children
                .upsert(parent_index, vec![node_index], |held, _| {
                    held.push(node_index)
                });
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
                assistant_ids: PersistentMap::default(),
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
            if message.role == crate::MessageRole::Assistant {
                let index = read.active_messages.len() + read.pending_messages.len();
                read.assistant_ids
                    .upsert(message.id.clone(), index, |first, index| {
                        *first = (*first).min(index);
                    });
            }
            read.pending_messages.push(message);
        }
    }

    pub(crate) fn children_of(&self, node_index: usize) -> Vec<usize> {
        self.children.get(&node_index).cloned().unwrap_or_default()
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
