//! In-memory [`StoreMaintenance`](crate::store::StoreMaintenance) implementation
//! for [`InMemorySessionStore`] (tombstone/vacuum/GC).
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. This is a trait impl on the parent module's type, so no public
//! path changes.

use super::InMemorySessionStore;

#[async_trait::async_trait]
impl crate::store::StoreMaintenance for InMemorySessionStore {
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
            let mut graph = self.global_session_graph.lock().expect("lock global graph");
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
}
