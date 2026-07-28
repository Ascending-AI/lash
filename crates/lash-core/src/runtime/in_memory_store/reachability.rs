//! Reachability-derived graph retirement for the in-memory store.

use std::collections::{HashMap, HashSet};

use super::InMemorySessionStore;

impl InMemorySessionStore {
    pub(super) fn live_child_counts(
        graph: &crate::SessionGraph,
        tombstoned: &HashSet<String>,
    ) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for child in graph
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
        {
            if let Some(parent_node_id) = &child.parent_node_id {
                *counts.entry(parent_node_id.clone()).or_default() += 1;
            }
        }
        counts
    }

    /// Reclaim the ancestry prefix that has no live child, session-head root,
    /// or explicit anchor. The child-count map is derived once from the graph
    /// inside the surrounding write transaction and updated only as this
    /// destructive decision tombstones nodes.
    pub(super) fn reclaim_unreachable_ancestry(
        graph: &crate::SessionGraph,
        live_child_counts: &mut HashMap<String, usize>,
        tombstoned: &mut HashSet<String>,
        first_node_id: &str,
        session_heads: &HashMap<String, Option<String>>,
        anchored_node_ids: &HashSet<String>,
    ) {
        let mut node_id = first_node_id.to_string();
        loop {
            let is_live = graph.find_node(&node_id).is_some() && !tombstoned.contains(&node_id);
            let has_child = live_child_counts.get(&node_id).copied().unwrap_or_default() > 0;
            let has_head = session_heads
                .values()
                .any(|leaf| leaf.as_deref() == Some(node_id.as_str()));
            let is_anchored = anchored_node_ids.contains(&node_id);
            if !is_live || has_child || has_head || is_anchored {
                return;
            }
            tombstoned.insert(node_id.clone());
            let Some(parent_node_id) = graph
                .find_node(&node_id)
                .and_then(|node| node.parent_node_id.clone())
            else {
                return;
            };
            if let Some(count) = live_child_counts.get_mut(&parent_node_id) {
                *count = count.saturating_sub(1);
            }
            node_id = parent_node_id;
        }
    }

    /// Drop one session-head root and reclaim only nodes no longer reachable
    /// from a live child, another session head, or an explicit anchor.
    pub(super) fn reclaim_history_for_delete(
        &self,
        session_id: &str,
    ) -> Result<(), crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let mut heads = self
            .global_session_heads
            .lock()
            .expect("lock global session heads")
            .clone();
        let graph = self
            .global_session_graph
            .lock()
            .expect("lock global graph")
            .clone();
        let mut owners = self
            .global_node_owners
            .lock()
            .expect("lock global node owners")
            .clone();
        let mut tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .clone();
        let anchors = self
            .node_anchors
            .lock()
            .expect("lock node anchors")
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut live_child_counts = Self::live_child_counts(&graph, &tombstoned);

        if let Some(leaf) = heads.remove(session_id).flatten() {
            Self::reclaim_unreachable_ancestry(
                &graph,
                &mut live_child_counts,
                &mut tombstoned,
                &leaf,
                &heads,
                &anchors,
            );
        }

        let candidates = owners
            .iter()
            .filter(|(node_id, owner)| {
                owner.as_str() == session_id
                    && !tombstoned.contains(*node_id)
                    && live_child_counts.get(*node_id).copied().unwrap_or_default() == 0
                    && !heads
                        .values()
                        .any(|leaf| leaf.as_deref() == Some(node_id.as_str()))
                    && !anchors.contains(*node_id)
            })
            .map(|(node_id, _)| node_id.clone())
            .collect::<Vec<_>>();
        for node_id in candidates {
            Self::reclaim_unreachable_ancestry(
                &graph,
                &mut live_child_counts,
                &mut tombstoned,
                &node_id,
                &heads,
                &anchors,
            );
        }

        let reclaimed = tombstoned.clone();
        let nodes = graph
            .nodes
            .iter()
            .filter(|node| !reclaimed.contains(&node.node_id))
            .cloned()
            .collect();
        owners.retain(|node_id, _| !reclaimed.contains(node_id));
        tombstoned.clear();

        *self
            .global_session_heads
            .lock()
            .expect("lock global session heads") = heads;
        *self.global_session_graph.lock().expect("lock global graph") =
            crate::SessionGraph::from_nodes(nodes, None);
        *self
            .global_node_owners
            .lock()
            .expect("lock global node owners") = owners;
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = tombstoned;
        *self.session_graph.lock().expect("lock graph") = crate::SessionGraph::default();
        *self.session_head_meta.lock().expect("lock session head") = None;
        Ok(())
    }
}
