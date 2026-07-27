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
        session_heads: &HashMap<String, Option<String>>,
        anchored_node_ids: &HashSet<String>,
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
                let derived_root = session_heads
                    .values()
                    .filter(|leaf| leaf.as_deref() == Some(node_id.as_str()))
                    .count() as i64
                    + i64::from(anchored_node_ids.contains(&node_id));
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
            let derived_root = session_heads
                .values()
                .filter(|leaf| leaf.as_deref() == Some(node_id.as_str()))
                .count() as i64
                + i64::from(anchored_node_ids.contains(&node_id));
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

    /// Drop one session-head root and reclaim only the ancestry that becomes
    /// unreachable. Node `session_id` is producer provenance, not lifecycle
    /// ownership once forks share a prefix.
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
        let mut counts = self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs")
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

        let leaf = heads.remove(session_id).flatten();
        if let Some(leaf) = leaf {
            Self::decrement_node_reference(
                &graph,
                &mut counts,
                &mut tombstoned,
                &leaf,
                &heads,
                &anchors,
            )?;
        }

        let zero_ref_nodes = owners
            .iter()
            .filter(|(node_id, owner)| {
                owner.as_str() == session_id
                    && !tombstoned.contains(*node_id)
                    && counts.get(*node_id).copied().unwrap_or_default() == 0
            })
            .map(|(node_id, _)| node_id.clone())
            .collect::<Vec<_>>();
        for node_id in zero_ref_nodes {
            let derived_children = graph
                .nodes
                .iter()
                .filter(|child| {
                    !tombstoned.contains(&child.node_id)
                        && child.parent_node_id.as_deref() == Some(node_id.as_str())
                })
                .count() as i64;
            let derived_roots = heads
                .values()
                .filter(|leaf| leaf.as_deref() == Some(node_id.as_str()))
                .count() as i64
                + i64::from(anchors.contains(&node_id));
            let derived = derived_children + derived_roots;
            let cached = counts.get(&node_id).copied().unwrap_or_default();
            if cached != derived {
                return Err(crate::StoreError::NodeRefcountDrift {
                    node_id,
                    cached,
                    derived,
                });
            }
            if derived != 0 {
                continue;
            }
            tombstoned.insert(node_id.clone());
            if let Some(parent_node_id) = graph
                .find_node(&node_id)
                .and_then(|node| node.parent_node_id.as_ref())
            {
                Self::decrement_node_reference(
                    &graph,
                    &mut counts,
                    &mut tombstoned,
                    parent_node_id,
                    &heads,
                    &anchors,
                )?;
            }
        }

        let reclaimed = tombstoned.clone();
        let nodes = graph
            .nodes
            .iter()
            .filter(|node| !reclaimed.contains(&node.node_id))
            .cloned()
            .collect();
        owners.retain(|node_id, _| !reclaimed.contains(node_id));
        counts.retain(|node_id, _| !reclaimed.contains(node_id));
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
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs") = counts;
        *self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes") = tombstoned;
        *self.session_graph.lock().expect("lock graph") = crate::SessionGraph::default();
        *self.session_head_meta.lock().expect("lock session head") = None;
        Ok(())
    }
}
