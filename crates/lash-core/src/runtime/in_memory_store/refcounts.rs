//! Transactional node-reference accounting for the in-memory store.

use std::collections::{HashMap, HashSet};

use super::InMemorySessionStore;

impl InMemorySessionStore {
    /// Decrement one cached reference and reclaim a zero-count ancestry prefix.
    ///
    /// A cached count that is too high leaks storage and is recoverable through
    /// a later scrub. A count that is too low can cascade through reachable
    /// shared history, after which vacuum makes the loss unrecoverable. Every
    /// destructive zero is therefore re-derived from the edge and root truth
    /// before its tombstone is staged.
    pub(super) fn decrement_node_reference(
        graph: &crate::SessionGraph,
        counts: &mut HashMap<String, i64>,
        tombstoned: &mut HashSet<String>,
        first_node_id: &str,
        head_leaf: Option<&str>,
    ) -> Result<(), crate::StoreError> {
        let mut node_id = first_node_id.to_string();
        loop {
            let cached_before = counts.get(&node_id).copied().unwrap_or_default();
            if cached_before <= 0 {
                let derived_children = graph
                    .nodes
                    .iter()
                    .filter(|child| {
                        !tombstoned.contains(&child.node_id)
                            && child.parent_node_id.as_deref() == Some(node_id.as_str())
                    })
                    .count() as i64;
                let derived_root = i64::from(head_leaf == Some(node_id.as_str()));
                return Err(crate::StoreError::NodeRefcountDrift {
                    node_id,
                    cached: cached_before,
                    derived: derived_children + derived_root,
                });
            }
            let cached = cached_before - 1;
            counts.insert(node_id.clone(), cached);
            if cached > 0 {
                return Ok(());
            }
            let derived_children = graph
                .nodes
                .iter()
                .filter(|child| {
                    !tombstoned.contains(&child.node_id)
                        && child.parent_node_id.as_deref() == Some(node_id.as_str())
                })
                .count() as i64;
            let derived_root = i64::from(head_leaf == Some(node_id.as_str()));
            let derived = derived_children + derived_root;
            if derived != 0 {
                return Err(crate::StoreError::NodeRefcountDrift {
                    node_id,
                    cached,
                    derived,
                });
            }
            tombstoned.insert(node_id.clone());
            let Some(parent_node_id) = graph
                .find_node(&node_id)
                .and_then(|node| node.parent_node_id.clone())
            else {
                return Ok(());
            };
            node_id = parent_node_id;
        }
    }

    /// Physically reclaim every history row owned by a deleted session.
    pub(super) fn reclaim_history_for_delete(&self, session_id: &str) {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.global_node_owners
            .lock()
            .expect("lock global in-memory node ids")
            .retain(|_, owner| owner != session_id);
        *self.session_graph.lock().expect("lock graph") = crate::SessionGraph::default();
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs") = HashMap::new();
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = HashSet::new();
        *self.session_head_meta.lock().expect("lock session head") = None;
    }
}
