use lash_protocol_types::allocation::{AllocationBudget, AllocationError, SourceCapacity};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::persistent_map::PersistentMap;
use crate::session_graph::facade_ops::SessionNodeProjection;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, SessionReadModel};
use crate::session_graph_integrity::{ancestry_indices, graph_node_indices, validate_structure};
use crate::session_model::SessionHistoryRecord;
use crate::{BaseRenderCache, Message, NodeId};
use lash_protocol_types::{AppendVec, same_history_record, same_message};

/// Every resident node's position by id. Persistent: every snapshot of the
/// graph shares it, and the store's window and each runtime that adopted it
/// extend their copies independently without copying it (FIG-6889).
#[derive(Clone, Debug)]
pub(crate) struct NodeIdIndex(PersistentMap<NodeId, usize>);

impl NodeIdIndex {
    fn from_resident(
        by_id: HashMap<NodeId, usize>,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        let mut result = Self(PersistentMap::default());
        for (id, index) in by_id {
            result.0 = result
                .0
                .with_insert(id, index, budget, |key, _| key.len() as u64)?;
        }
        Ok(result)
    }

    pub(crate) fn get(&self, node_id: &str) -> Option<usize> {
        self.0.get(node_id).copied()
    }

    fn insert(
        &mut self,
        node_id: &NodeId,
        index: usize,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        let _copy = budget.reserve(node_id.len() as u64 + 32)?;
        self.0 =
            std::mem::take(&mut self.0)
                .with_insert(node_id.clone(), index, budget, |key, _| key.len() as u64)?;
        Ok(())
    }

    pub(crate) fn rename(
        &mut self,
        from: &NodeId,
        to: NodeId,
        index: usize,
        budget: &AllocationBudget,
    ) -> Result<(), AllocationError> {
        self.0 = std::mem::take(&mut self.0)
            .without(from.as_str(), budget)?
            .with_insert(to, index, budget, |key, _| key.len() as u64)?;
        Ok(())
    }
}

/// The current frame's shared sequences. Fallible appends fold immediately,
/// preserving sequence and render identity for every subsequent reader.
#[derive(Clone, Debug)]
struct ActiveReadModel {
    active_events: AppendVec<SessionHistoryRecord>,
    active_messages: AppendVec<Message>,
    assistant_ids: PersistentMap<String, usize>,
    attachments: PersistentMap<crate::AttachmentId, usize>,
    prompt_render_cache: Arc<BaseRenderCache>,
}

#[derive(Debug)]
pub(crate) struct SessionGraphCache {
    pub(crate) by_id: NodeIdIndex,
    pub(crate) active_path_indices: AppendVec<usize>,
    pub(crate) active_frame_indices: AppendVec<usize>,
    /// Every child edge: each parent's children's positions, in append
    /// order.
    children: PersistentMap<usize, AppendVec<usize>>,
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
    /// Folded by fallible edits before publication; readers only clone the shared buffers.
    active_read: ActiveReadModel,
    budget: AllocationBudget,
}

impl Clone for SessionGraphCache {
    fn clone(&self) -> Self {
        Self {
            by_id: self.by_id.clone(),
            active_path_indices: self.active_path_indices.clone(),
            active_frame_indices: self.active_frame_indices.clone(),
            children: self.children.clone(),
            proven: AtomicBool::new(self.proven()),
            active_read: self.active_read.clone(),
            budget: self.budget.clone(),
        }
    }
}

impl SessionGraphCache {
    pub(crate) fn build(graph: &SessionGraph) -> Result<Self, crate::StoreError> {
        let by_id = graph_node_indices(graph)?;
        let active_path_indices = ancestry_indices(graph, &by_id, graph.leaf_node_id.as_deref())?;
        Self::from_indices(graph, by_id, active_path_indices, false)
    }

    /// The cache of a graph that passes the full structural-integrity check
    /// ([`validate_structure`]), holding the proof.
    pub(crate) fn build_validated(graph: &SessionGraph) -> Result<Self, crate::StoreError> {
        let (by_id, active_path_indices) = validate_structure(graph)?;
        Self::from_indices(graph, by_id, active_path_indices, true)
    }

    fn from_indices(
        graph: &SessionGraph,
        by_id: HashMap<NodeId, usize>,
        mut active_path_indices: Vec<usize>,
        proven: bool,
    ) -> Result<Self, crate::StoreError> {
        let budget = graph.nodes.allocation_budget();
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
                append_child(&mut children, *parent, index, budget)?;
            }
        }
        let mut cache = Self {
            by_id: NodeIdIndex::from_resident(by_id, budget)?,
            active_path_indices: admitted_sequence(active_path_indices, budget)?,
            active_frame_indices: admitted_sequence(active_frame_indices, budget)?,
            children,
            budget: budget.clone(),
            proven: AtomicBool::new(proven),
            active_read: ActiveReadModel {
                active_events: AppendVec::try_with_capacity(0, budget)?,
                active_messages: AppendVec::try_with_capacity(0, budget)?,
                assistant_ids: PersistentMap::default(),
                attachments: PersistentMap::default(),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
            },
        };
        cache.rebuild_read_model(graph)?;
        Ok(cache)
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

    pub(crate) fn rebuild_read_model(
        &mut self,
        graph: &SessionGraph,
    ) -> Result<(), crate::StoreError> {
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
        let budget = &self.budget;
        // Cold construction includes the next append's spare slots. The
        // retained path must not detach a full frame on its first edit.
        let capacity = frame_path.len().saturating_mul(2).max(4);
        let mut active_messages = AppendVec::try_with_capacity(capacity, budget)?;
        let mut active_events = AppendVec::try_with_capacity(capacity, budget)?;
        let mut assistant_ids = PersistentMap::default();
        let mut attachments = PersistentMap::default();
        for idx in frame_path {
            let node = &graph.nodes[*idx];
            if let Some(event) = node.event() {
                active_events.push_cloned_adopting(event, |_, _| false)?;
            }
            let _message_copy = match node.event() {
                Some(SessionHistoryRecord::Conversation(record)) => budget.reserve(
                    record
                        .owned_capacity()
                        .saturating_add(std::mem::size_of::<Message>() as u64),
                )?,
                _ => Default::default(),
            };
            if let Some(message) = node.message()
                && !message.is_transient()
            {
                if message.role == crate::MessageRole::Assistant {
                    insert_first(
                        &mut assistant_ids,
                        &message.id,
                        active_messages.len(),
                        budget,
                    )?;
                }
                count_attachments(&mut attachments, &message, budget)?;
                active_messages.push(message)?;
            }
        }
        self.active_read = ActiveReadModel {
            active_events,
            active_messages,
            assistant_ids,
            attachments,
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        };
        Ok(())
    }

    /// The same active-frame projection owns both the message sequence and
    /// its assistant-id membership. Rebuilds and frame changes replace both.
    pub(crate) fn contains_active_assistant_message_id_before(&self, id: &str, end: usize) -> bool {
        self.active_read
            .assistant_ids
            .get(id)
            .is_some_and(|index| *index < end)
    }

    /// Read the already-folded projection without allocating source capacity.
    pub(crate) fn active_read_model(&self) -> SessionReadModel {
        let read = &self.active_read;
        SessionReadModel {
            active_events: read.active_events.clone(),
            messages: read.active_messages.clone(),
            prompt_render_cache: Arc::clone(&read.prompt_render_cache),
        }
    }

    pub(crate) fn replace_conversation_reply(
        &mut self,
        event: &SessionHistoryRecord,
    ) -> Result<(), AllocationError> {
        let SessionHistoryRecord::Conversation(conversation) = event else {
            return Ok(());
        };
        let message = conversation.to_message();
        let read = &mut self.active_read;
        // The marked reply is one of the newest records (FIG-6889).
        if let Some(index) = read.active_events.iter().rposition(|existing| matches!(
            existing, SessionHistoryRecord::Conversation(existing) if existing.id == conversation.id
        )) {
            let mut tail = read.active_events[index..].to_vec();
            tail[0] = event.clone();
            read.active_events.replace_from(index, tail)?;
        }
        if let Some(index) = read
            .active_messages
            .iter()
            .rposition(|existing| existing.id == message.id)
        {
            let old = read.active_messages[index].clone();
            uncount_attachments(&mut read.attachments, &old, &self.budget)?;
            count_attachments(&mut read.attachments, &message, &self.budget)?;
            let mut tail = read.active_messages[index..].to_vec();
            tail[0] = message.clone();
            read.active_messages.replace_from(index, tail)?;
        }
        Ok(())
    }

    pub(crate) fn append_node(
        &mut self,
        node_index: usize,
        node: &SessionNodeRecord,
        extends_leaf: bool,
    ) -> Result<(), AllocationError> {
        if let Some(parent_index) = node
            .parent_node_id
            .as_deref()
            .and_then(|id| self.by_id.get(id))
        {
            append_child(&mut self.children, parent_index, node_index, &self.budget)?;
        }
        self.by_id.insert(&node.node_id, node_index, &self.budget)?;
        if !extends_leaf {
            return Ok(());
        }
        self.active_path_indices.push(node_index)?;
        if matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
            self.active_frame_indices.push(node_index)?;
        }
        let budget = &self.budget;
        let read = &mut self.active_read;
        if matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
            // A pending `FrameOpen` moves the projection's start to the new
            // frame before its commit (ADR 0112 §9).
            *read = ActiveReadModel {
                active_events: AppendVec::try_with_capacity(0, budget)?,
                active_messages: AppendVec::try_with_capacity(0, budget)?,
                assistant_ids: PersistentMap::default(),
                attachments: PersistentMap::default(),
                prompt_render_cache: Arc::new(BaseRenderCache::new()),
            };
        }
        if let Some(event) = node.event() {
            read.active_events
                .push_cloned_adopting(event, same_history_record)?;
        }
        let _message_copy = match node.event() {
            Some(SessionHistoryRecord::Conversation(record)) => budget.reserve(
                record
                    .owned_capacity()
                    .saturating_add(std::mem::size_of::<Message>() as u64),
            )?,
            _ => Default::default(),
        };
        if let Some(message) = node.message()
            && !message.is_transient()
        {
            if message.role == crate::MessageRole::Assistant {
                let index = read.active_messages.len();
                insert_first(&mut read.assistant_ids, &message.id, index, budget)?;
            }
            count_attachments(&mut read.attachments, &message, &self.budget)?;
            let prefix_len = read.active_messages.len();
            read.active_messages.push_adopting(message, same_message)?;
            read.prompt_render_cache = Arc::new(BaseRenderCache::extending(
                Arc::clone(&read.prompt_render_cache),
                prefix_len,
            ));
        }
        Ok(())
    }

    pub(crate) fn detach_prompt_render_cache(&mut self) {
        self.active_read.prompt_render_cache = Arc::new(BaseRenderCache::new());
    }

    pub(crate) fn attachment_ids(&self) -> Vec<crate::AttachmentId> {
        let read = &self.active_read;
        let mut ids = Vec::new();
        read.attachments.for_each(|id, _| ids.push(id.clone()));
        ids.sort();
        ids
    }

    pub(crate) fn children_of(&self, node_index: usize) -> impl Iterator<Item = usize> + '_ {
        self.children
            .get(&node_index)
            .into_iter()
            .flat_map(|indices| indices.iter().copied())
    }

    pub(crate) fn reserve_append_capacity(
        &mut self,
        additional_nodes: usize,
        additional_messages: usize,
    ) -> Result<(), AllocationError> {
        self.active_path_indices.reserve(additional_nodes)?;
        self.active_frame_indices.reserve(additional_nodes)?;
        self.active_read.active_events.reserve(additional_nodes)?;
        self.active_read
            .active_messages
            .reserve(additional_messages)?;
        Ok(())
    }
}

fn count_attachments(
    counts: &mut PersistentMap<crate::AttachmentId, usize>,
    message: &Message,
    budget: &AllocationBudget,
) -> Result<(), AllocationError> {
    for part in message.parts.iter() {
        for reference in part
            .attachments()
            .chain(part.retained_outputs().map(|retained| &retained.reference))
        {
            let count = counts.get(&reference.id).copied().unwrap_or(0) + 1;
            *counts = std::mem::take(counts).with_insert(
                reference.id.clone(),
                count,
                budget,
                |_, _| 0,
            )?;
        }
    }
    Ok(())
}

fn uncount_attachments(
    counts: &mut PersistentMap<crate::AttachmentId, usize>,
    message: &Message,
    budget: &AllocationBudget,
) -> Result<(), AllocationError> {
    for part in message.parts.iter() {
        for reference in part
            .attachments()
            .chain(part.retained_outputs().map(|retained| &retained.reference))
        {
            match counts.get(&reference.id).copied() {
                Some(1) => {
                    *counts = std::mem::take(counts).without(&reference.id, budget)?;
                }
                Some(count) => {
                    *counts = std::mem::take(counts).with_insert(
                        reference.id.clone(),
                        count - 1,
                        budget,
                        |_, _| 0,
                    )?;
                }
                None => unreachable!("a projected message owns its attachment occurrences"),
            }
        }
    }
    Ok(())
}

fn admitted_sequence<T: Clone + lash_protocol_types::allocation::SourceCapacity>(
    values: Vec<T>,
    budget: &AllocationBudget,
) -> Result<AppendVec<T>, AllocationError> {
    let mut sequence = AppendVec::try_with_capacity(values.len().saturating_mul(2).max(4), budget)?;
    sequence.extend(values)?;
    Ok(sequence)
}

fn append_child(
    children: &mut PersistentMap<usize, AppendVec<usize>>,
    parent: usize,
    index: usize,
    budget: &AllocationBudget,
) -> Result<(), AllocationError> {
    let mut indices = match children.get(&parent) {
        Some(indices) => indices.clone(),
        None => AppendVec::try_with_capacity(1, budget)?,
    };
    indices.push(index)?;
    *children = std::mem::take(children).with_insert(parent, indices, budget, |_, _| 0)?;
    Ok(())
}

fn insert_first(
    ids: &mut PersistentMap<String, usize>,
    id: &str,
    index: usize,
    budget: &AllocationBudget,
) -> Result<(), AllocationError> {
    if ids.get(id).is_some_and(|first| *first <= index) {
        return Ok(());
    }
    let _copy = budget.reserve(id.len() as u64 + 32)?;
    *ids = std::mem::take(ids)
        .with_insert(id.to_owned(), index, budget, |key, _| key.capacity() as u64)?;
    Ok(())
}
