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

    /// Drop this session's leaf root, confirming every destructive zero against
    /// the in-memory edge rows before publishing any tombstones.
    pub(super) fn release_head_root_for_delete(&self) -> Result<(), crate::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let leaf_node_id = self
            .session_head_meta
            .lock()
            .expect("lock session head")
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.clone());
        let Some(node_id) = leaf_node_id else {
            *self.session_head_meta.lock().expect("lock session head") = None;
            return Ok(());
        };
        let graph = self.session_graph.lock().expect("lock graph");
        let mut tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .clone();
        let mut counts = self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
            .clone();
        Self::decrement_node_reference(&graph, &mut counts, &mut tombstoned, &node_id, None)?;
        drop(graph);
        *self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs") = counts;
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = tombstoned;
        *self.session_head_meta.lock().expect("lock session head") = None;
        Ok(())
    }
}
