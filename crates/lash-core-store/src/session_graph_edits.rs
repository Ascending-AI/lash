//! Resident graph edits preserve projections when their indexed structure stays the same.

use super::*;

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

    pub fn remap_node_ids(&mut self, _session_id: &SessionId, mapping: &[(NodeId, NodeId)]) {
        if mapping.is_empty() {
            return;
        }
        // The cache indexes children by stable resident position, so a
        // parent's children resolve even in a child-before-parent layout.
        // Renaming ids does not change positions or topology.
        let positions = self.resident_node_indices(mapping.iter().map(|(draft, _)| draft.as_str()));
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
            edits
                .entry(index)
                .or_insert_with(|| self.nodes[index].as_ref().clone())
                .node_id = derived.clone();
            renamed.push((draft.clone(), derived.clone(), index));
        }
        for (draft, derived, index) in &renamed {
            for child_index in self.cache().children_of(*index) {
                let node = &self.nodes[child_index];
                debug_assert_eq!(node.parent_node_id.as_ref(), Some(draft));
                edits
                    .entry(child_index)
                    .or_insert_with(|| node.as_ref().clone())
                    .parent_node_id = Some(derived.clone());
            }
        }
        let leaf = self
            .leaf_node_id
            .as_ref()
            .and_then(|leaf| derived_by_id.get(leaf).map(|derived| (*derived).clone()));
        // Positions and payloads are unchanged, so the cache stays valid
        // once its id index follows the renames.
        self.detach_initialized_cache_for_append();
        if let Some(cache_lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = cache_lock.get_mut()
        {
            for (draft, derived, index) in renamed {
                cache.by_id.rename(&draft, derived, index);
            }
        } else {
            self.invalidate_cache();
        }
        let data = Arc::make_mut(&mut self.inner);
        replace_node_records(&mut data.nodes, edits);
        if let Some(leaf) = leaf {
            data.leaf_node_id = Some(leaf);
        }
    }

    /// Applies store-realized timestamps to the nodes a commit receipt names.
    ///
    /// Only the realized records are replaced; every other resident record
    /// stays pointer-identical to every snapshot. A timestamp is not part of
    /// anything the cache indexes or projects, so the cache stays.
    pub fn apply_realized_node_timestamps(&mut self, realized: &[RealizedNodeTimestamp]) {
        if realized.is_empty() {
            return;
        }
        let positions =
            self.resident_node_indices(realized.iter().map(|node| node.node_id.as_str()));
        let mut edits = std::collections::BTreeMap::<usize, SessionNodeRecord>::new();
        for (realized, position) in realized.iter().zip(positions) {
            let Some(index) = position else {
                continue;
            };
            edits
                .entry(index)
                .or_insert_with(|| self.nodes[index].as_ref().clone())
                .timestamp = realized.timestamp;
        }
        replace_node_records(&mut Arc::make_mut(&mut self.inner).nodes, edits);
    }

    pub(super) fn reserve_append_capacity(
        &mut self,
        additional_nodes: usize,
        additional_messages: usize,
    ) {
        if additional_nodes == 0 {
            return;
        }
        self.detach_initialized_cache_for_append();
        Arc::make_mut(&mut self.inner)
            .nodes
            .reserve(additional_nodes);
        if let Some(cache_lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = cache_lock.get_mut()
        {
            cache.reserve_append_capacity(additional_nodes, additional_messages);
        }
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
    ) -> bool {
        let mut record = self.nodes[index].as_ref().clone();
        let SessionNodePayload::Event {
            event: SessionHistoryRecord::Conversation(message),
        } = &mut record.payload
        else {
            return false;
        };
        message.reply_marker = Some(marker);
        let event = SessionHistoryRecord::Conversation(message.clone());
        self.detach_initialized_cache_for_append();
        if let Some(lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = lock.get_mut()
        {
            cache.replace_conversation_reply(&event);
        }
        replace_node_records(
            &mut Arc::make_mut(&mut self.inner).nodes,
            [(index, record)].into(),
        );
        true
    }
}

/// Replaces the records at `edits`' positions, from the first of them on:
/// in place when no snapshot has seen that tail (the records a commit just
/// appended), on a copy otherwise.
fn replace_node_records(
    nodes: &mut AppendVec<Arc<SessionNodeRecord>>,
    mut edits: std::collections::BTreeMap<usize, SessionNodeRecord>,
) {
    let Some(start) = edits.keys().next().copied() else {
        return;
    };
    let tail = (start..nodes.len())
        .map(|index| {
            edits
                .remove(&index)
                .map_or_else(|| Arc::clone(&nodes[index]), Arc::new)
        })
        .collect::<Vec<_>>();
    nodes.replace_from(start, tail);
}
