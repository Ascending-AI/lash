//! Resident graph edits preserve projections when their indexed structure stays the same.

use super::*;
use lash_protocol_types::allocation::SourceCapacity;

impl SessionGraph {
    /// Resident positions for `node_ids`, in input order.
    ///
    /// Resolution reads the initialized cache's `by_id` when it exists and
    /// falls back to a one-off scan when it does not; it runs before
    /// `data_mut` invalidates the cache, so mutation sites walk only the
    /// resolved positions rather than the whole resident vector. Ids absent
    /// from the graph resolve to `None`.
    pub(super) fn resident_node_indices<'a>(
        &self,
        node_ids: impl IntoIterator<Item = &'a str>,
    ) -> Vec<Option<usize>> {
        let node_ids = node_ids.into_iter().collect::<Vec<_>>();
        if let Some(cache) = self.cache.get() {
            return node_ids
                .iter()
                .map(|node_id| cache.by_id.get(node_id))
                .collect();
        }
        let by_id = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.node_id.as_str(), index))
            .collect::<HashMap<_, _>>();
        node_ids
            .iter()
            .map(|node_id| by_id.get(node_id).copied())
            .collect()
    }

    /// How many leading resident nodes are durable: the rest are the
    /// pending suffix the next commit carries.
    pub fn durable_len(&self) -> usize {
        self.durable_len.min(self.nodes.len())
    }

    /// The resident nodes no commit has made durable yet, in append order.
    pub fn pending_nodes(&self) -> &[Arc<SessionNodeRecord>] {
        &self.nodes[self.durable_len()..]
    }

    /// Whether the resident node `node_id` is durable. A node that is not
    /// resident is not known to be.
    pub fn is_durable(&self, node_id: &str) -> bool {
        self.resident_node_indices([node_id])[0].is_some_and(|index| index < self.durable_len())
    }

    /// Every resident node is durable: the graph is a store's window.
    pub fn mark_all_durable(&mut self) {
        self.durable_len = self.nodes.len();
    }

    /// A landed commit made `node_ids` durable: the prefix extends through
    /// the last of them that is resident. A commit carries the pending
    /// suffix in order, so every pending node before that one is among
    /// them.
    pub fn mark_durable<'a>(&mut self, node_ids: impl IntoIterator<Item = &'a NodeId>) {
        let through = self
            .resident_node_indices(node_ids.into_iter().map(NodeId::as_str))
            .into_iter()
            .flatten()
            .max()
            .map_or(0, |index| index + 1);
        self.durable_len = self.durable_len().max(through);
    }

    /// This graph with its first `len` nodes durable: a rebuild of a graph
    /// that keeps its durable nodes, in order, ahead of its pending ones.
    pub(crate) fn with_durable_prefix(mut self, len: usize) -> Self {
        self.durable_len = len;
        self
    }

    pub fn remap_node_ids(
        &mut self,
        _session_id: &SessionId,
        mapping: &[(NodeId, NodeId)],
    ) -> Result<(), crate::StoreError> {
        if mapping.is_empty() {
            return Ok(());
        }
        self.remap_node_ids_inner(mapping)
    }

    fn remap_node_ids_inner(
        &mut self,
        mapping: &[(NodeId, NodeId)],
    ) -> Result<(), crate::StoreError> {
        // The cache indexes children by stable resident position, so a
        // parent's children resolve even in a child-before-parent layout.
        // Renaming ids does not change positions or topology.
        let budget = self.nodes.allocation_budget().clone();
        let cache = self.try_cache()?;
        let edits_bound = mapping
            .iter()
            .filter_map(|(draft, _)| cache.by_id.get(draft.as_str()))
            .map(|index| cache.children_of(index).count().saturating_add(1))
            .fold(0usize, usize::saturating_add);
        let replacement_bytes = mapping.iter().fold(0u64, |bytes, (_, derived)| {
            bytes.saturating_add(derived.len() as u64 * 4 + 128)
        });
        let id_bytes = mapping.iter().fold(0u64, |bytes, (draft, derived)| {
            bytes.saturating_add((draft.len() + derived.len()) as u64 * 4 + 128)
        });
        let _scratch = budget.reserve(
            (edits_bound as u64)
                .saturating_mul(1024)
                .saturating_add((mapping.len() as u64).saturating_mul(256))
                .saturating_add(id_bytes),
        )?;
        let positions = mapping
            .iter()
            .map(|(draft, _)| cache.by_id.get(draft.as_str()))
            .collect::<Vec<_>>();
        let derived_by_id = mapping
            .iter()
            .map(|(draft, derived)| (draft, derived))
            .collect::<HashMap<_, _>>();
        let mut edits = std::collections::BTreeMap::<usize, SessionNodeRecord>::new();
        let mut renamed = Vec::new();
        for ((draft, derived), position) in mapping.iter().zip(positions) {
            let Some(index) = position else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = edits.entry(index) {
                entry.insert(self.nodes[index].clone_for_edit_in(&budget, replacement_bytes)?);
            }
            if let Some(record) = edits.get_mut(&index) {
                record.node_id = derived.clone();
            }
            renamed.push((draft.clone(), derived.clone(), index));
        }
        for (draft, derived, index) in &renamed {
            for child_index in self.cache().children_of(*index) {
                let node = &self.nodes[child_index];
                debug_assert_eq!(node.parent_node_id.as_ref(), Some(draft));
                if let std::collections::btree_map::Entry::Vacant(entry) = edits.entry(child_index)
                {
                    entry.insert(node.clone_for_edit_in(&budget, replacement_bytes)?);
                }
                if let Some(record) = edits.get_mut(&child_index) {
                    record.parent_node_id = Some(derived.clone());
                }
            }
        }
        let leaf = self
            .leaf_node_id
            .as_ref()
            .and_then(|leaf| derived_by_id.get(leaf).map(|derived| (*derived).clone()));
        // Positions and payloads are unchanged, so the cache stays valid
        // once its id index follows the renames.
        let mut cache = self.try_cache()?.clone();
        for (draft, derived, index) in renamed {
            cache.by_id.rename(&draft, derived, index, &budget)?;
        }
        cache.withdraw_proof();
        let cache_lock = OnceLock::new();
        let _ = cache_lock.set(cache);
        let cache = Arc::new(cache_lock);
        // Buffer replacement is the final fallible operation. If its tail
        // is unseen, it rewrites in place after all reservations succeed.
        let data = &mut Arc::make_mut(&mut self.inner).data;
        replace_node_records(&mut data.nodes, edits)?;
        if let Some(leaf) = leaf {
            data.leaf_node_id = Some(leaf);
        }
        self.cache = cache;
        Ok(())
    }

    /// Applies store-realized timestamps to the nodes a commit receipt names.
    ///
    /// Only the realized records are replaced; every other resident record
    /// stays pointer-identical to every snapshot. A timestamp is not part of
    /// anything the cache indexes or projects, so the cache stays.
    pub fn apply_realized_node_timestamps(
        &mut self,
        realized: &[RealizedNodeTimestamp],
    ) -> Result<(), crate::StoreError> {
        if realized.is_empty() {
            return Ok(());
        }
        self.apply_realized_node_timestamps_inner(realized)
    }

    fn apply_realized_node_timestamps_inner(
        &mut self,
        realized: &[RealizedNodeTimestamp],
    ) -> Result<(), crate::StoreError> {
        let budget = self.nodes.allocation_budget().clone();
        let _scratch = budget.reserve(
            (realized.len() as u64)
                .saturating_mul(1024)
                .saturating_add(128),
        )?;
        let by_id = &self.try_cache()?.by_id;
        let positions = realized
            .iter()
            .map(|node| by_id.get(node.node_id.as_str()))
            .collect::<Vec<_>>();
        let mut edits = std::collections::BTreeMap::<usize, SessionNodeRecord>::new();
        for (realized, position) in realized.iter().zip(positions) {
            let Some(index) = position else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) = edits.entry(index) {
                entry.insert(self.nodes[index].clone_in(&budget)?);
            }
            if let Some(record) = edits.get_mut(&index) {
                record.timestamp = realized.timestamp;
            }
        }
        replace_node_records(&mut Arc::make_mut(&mut self.inner).data.nodes, edits)?;
        Ok(())
    }

    pub(super) fn reserve_append_capacity(
        &mut self,
        additional_nodes: usize,
        additional_messages: usize,
    ) -> Result<(), crate::StoreError> {
        if additional_nodes == 0 {
            return Ok(());
        }
        self.detach_initialized_cache_for_append();
        Arc::make_mut(&mut self.inner)
            .data
            .nodes
            .reserve(additional_nodes)?;
        if let Some(cache_lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = cache_lock.get_mut()
        {
            cache.reserve_append_capacity(additional_nodes, additional_messages)?;
        }
        Ok(())
    }

    pub(super) fn detach_initialized_cache_for_append(&mut self) {
        if Arc::get_mut(&mut self.cache).is_some() {
            return;
        }
        let Some(cache) = self.cache.get().cloned() else {
            self.invalidate_cache();
            return;
        };
        let lock = OnceLock::new();
        let _ = lock.set(cache);
        self.cache = Arc::new(lock);
    }

    /// Isolate claim-owned rendering from the store's independent window
    /// cache. Immutable source nodes and indexes stay shared; only the memoized
    /// render starts empty, so store readers cannot pin an owner's allocations.
    #[doc(hidden)]
    pub fn detach_owner_prompt_rendering(&mut self) {
        self.detach_initialized_cache_for_append();
        if let Some(lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = lock.get_mut()
        {
            cache.detach_prompt_render_cache();
        }
    }

    /// Rewrites the current frame's readable tail and moves the resident leaf
    /// while retaining historical branches and excluding transient
    /// replacement messages.
    ///
    /// The rewrite covers the active path from the nearest `FrameOpen`
    /// ancestor of the leaf, the same span [`Self::read_model`] projects, so
    /// its cost is proportional to the current frame.
    ///
    /// The resulting graph is a read projection. It must never be committed against an existing
    /// session head because the rewritten tail is not parented from that durable head.
    pub fn rewrite_active_read_tail(&mut self, messages: &[Message]) {
        let active_path = self.active_path_nodes();
        let frame_start = active_path
            .iter()
            .rposition(|node| matches!(node.payload, SessionNodePayload::FrameOpen { .. }))
            .unwrap_or(0);
        let replacement = build_active_read_replacement(
            active_path[frame_start..].iter().copied(),
            self.append_builder_in_namespace(format!(
                "unscoped-replacement:{}",
                self.leaf_node_id.as_deref().unwrap_or("root")
            )),
            messages,
            crate::SystemClock.node_timestamp(),
        );
        let data = self.data_mut();
        data.leaf_node_id = replacement.leaf_node_id;
        // A read projection: its tail is a copy of its own, so it never
        // takes the tip of the resident graph's buffer from the graph that
        // commits there.
        let mut nodes = data.nodes.to_vec();
        nodes.extend(replacement.new_tail_nodes.into_iter().map(Arc::new));
        data.nodes = AppendVec::from(nodes);
    }

    pub fn from_active_read_state(messages: &[Message]) -> Self {
        let mut graph = Self::default();
        graph.rewrite_active_read_tail(messages);
        graph
    }

    pub(crate) fn mark_conversation_reply(
        &mut self,
        index: usize,
        marker: crate::TurnReply,
    ) -> Result<bool, crate::StoreError> {
        self.mark_conversation_reply_inner(index, marker)
    }

    fn mark_conversation_reply_inner(
        &mut self,
        index: usize,
        marker: crate::TurnReply,
    ) -> Result<bool, crate::StoreError> {
        let budget = self.nodes.allocation_budget().clone();
        let mut record = self.nodes[index].clone_for_edit_in(
            &budget,
            lash_protocol_types::allocation::payload_capacity(&marker, 0),
        )?;
        let SessionNodePayload::Event {
            event: SessionHistoryRecord::Conversation(message),
        } = &mut record.payload
        else {
            return Ok(false);
        };
        message.reply_marker = Some(marker);
        let _event_copy = budget.reserve(
            message
                .owned_capacity()
                .saturating_add(std::mem::size_of::<SessionHistoryRecord>() as u64),
        )?;
        let event = SessionHistoryRecord::Conversation(message.clone());
        let mut cache = self.try_cache()?.clone();
        cache.replace_conversation_reply(&event)?;
        let cache_lock = OnceLock::new();
        let _ = cache_lock.set(cache);
        let cache = Arc::new(cache_lock);
        replace_node_records(
            &mut Arc::make_mut(&mut self.inner).data.nodes,
            [(index, record)].into(),
        )?;
        self.cache = cache;
        Ok(true)
    }
}

/// Replaces the records at `edits`' positions, from the first of them on:
/// in place when no snapshot has seen that tail (the records a commit just
/// appended), on a copy otherwise.
fn replace_node_records(
    nodes: &mut AppendVec<Arc<SessionNodeRecord>>,
    mut edits: std::collections::BTreeMap<usize, SessionNodeRecord>,
) -> Result<(), crate::StoreError> {
    let Some(start) = edits.keys().next().copied() else {
        return Ok(());
    };
    let _scratch = nodes.allocation_budget().reserve(
        ((nodes.len() - start) as u64)
            .saturating_mul(std::mem::size_of::<Arc<SessionNodeRecord>>() as u64)
            .saturating_add(128),
    )?;
    let tail = (start..nodes.len())
        .map(|index| {
            edits
                .remove(&index)
                .map_or_else(|| Arc::clone(&nodes[index]), Arc::new)
        })
        .collect::<Vec<_>>();
    nodes.replace_from(start, tail)?;
    Ok(())
}
