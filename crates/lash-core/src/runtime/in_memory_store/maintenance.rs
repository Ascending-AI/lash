//! In-memory [`StoreMaintenance`](crate::store::StoreMaintenance) implementation
//! for [`InMemorySessionStore`] (tombstone/vacuum/GC).
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. This is a trait impl on the parent module's type, so no public
//! path changes.

use super::InMemorySessionStore;

#[async_trait::async_trait]
impl crate::store::StoreMaintenance for InMemorySessionStore {
    async fn tombstone_nodes(&self, ids: &[String]) -> Result<(), crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        self.tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes")
            .extend(ids.iter().cloned());
        Ok(())
    }

    async fn vacuum(&self) -> Result<crate::store::VacuumReport, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let ids = {
            let mut tombstoned = self
                .tombstoned_node_ids
                .lock()
                .expect("lock tombstoned nodes");
            std::mem::take(&mut *tombstoned)
        };
        let removed_node_count = if ids.is_empty() {
            0
        } else {
            let mut graph = self.session_graph.lock().expect("lock graph");
            let before = graph.nodes.len();
            let leaf_node_id = graph
                .leaf_node_id
                .clone()
                .filter(|leaf| !ids.contains(leaf));
            let nodes = graph
                .nodes
                .iter()
                .filter(|node| !ids.contains(&node.node_id))
                .cloned()
                .collect::<Vec<_>>();
            let removed_node_count = before.saturating_sub(nodes.len());
            *graph = crate::SessionGraph::from_nodes(nodes, leaf_node_id);
            self.global_node_owners
                .lock()
                .expect("lock global in-memory node ids")
                .retain(|node_id, _| !ids.contains(node_id));
            removed_node_count
        };
        let mut pending = self
            .pending_turn_inputs
            .lock()
            .expect("lock pending turn input");
        let before = pending.len();
        pending.retain(|entry| {
            !matches!(
                entry.input.state,
                crate::TurnInputState::Cancelled | crate::TurnInputState::Completed
            )
        });
        Ok(crate::store::VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count: before.saturating_sub(pending.len()),
        })
    }

    async fn gc_unreachable(&self) -> Result<crate::store::GcReport, crate::store::StoreError> {
        Ok(crate::store::GcReport::default())
    }

    async fn verify_node_refcounts(
        &self,
    ) -> Result<crate::store::NodeRefcountVerification, crate::store::StoreError> {
        let _transaction = self
            .write_transaction
            .lock()
            .expect("lock in-memory write transaction");
        let graph = self.session_graph.lock().expect("lock graph");
        let tombstoned = self
            .tombstoned_node_ids
            .lock()
            .expect("lock tombstoned nodes");
        let head_leaf = self
            .session_head_meta
            .lock()
            .expect("lock session head")
            .as_ref()
            .and_then(|meta| meta.leaf_node_id.as_deref())
            .map(ToOwned::to_owned);
        let cached = self
            .incoming_node_refs
            .lock()
            .expect("lock incoming node refs");
        let live_nodes = graph
            .nodes
            .iter()
            .filter(|node| !tombstoned.contains(&node.node_id))
            .collect::<Vec<_>>();
        for node in &live_nodes {
            let derived_children = live_nodes
                .iter()
                .filter(|child| child.parent_node_id.as_deref() == Some(node.node_id.as_str()))
                .count() as i64;
            let derived_root = i64::from(head_leaf.as_deref() == Some(node.node_id.as_str()));
            let derived = derived_children + derived_root;
            let cached = cached.get(&node.node_id).copied().unwrap_or_default();
            if cached != derived {
                return Err(crate::store::StoreError::NodeRefcountDrift {
                    node_id: node.node_id.clone(),
                    cached,
                    derived,
                });
            }
        }
        Ok(crate::store::NodeRefcountVerification {
            checked_node_count: live_nodes.len(),
        })
    }
}
