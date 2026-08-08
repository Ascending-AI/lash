//! In-memory [`StoreMaintenance`](crate::store::StoreMaintenance) implementation
//! for [`InMemorySessionStore`] (tombstone/vacuum/GC).
//!
//! Split from `runtime/in_memory_store.rs` to keep it under the file-size
//! budget. This is a trait impl on the parent module's type, so no public
//! path changes.

use super::InMemorySessionStore;
use lash_sansio::sync::MutexExt;

#[async_trait::async_trait]
impl crate::store::StoreMaintenance for InMemorySessionStore {
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        _session_id: &str,
    ) -> Result<bool, crate::store::StoreError> {
        Ok(false)
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        _session_id: &str,
    ) -> Result<Vec<(String, String)>, crate::store::StoreError> {
        Ok(Vec::new())
    }

    async fn vacuum(&self) -> Result<crate::store::VacuumReport, crate::store::StoreError> {
        // `deleted_session_ids` is deliberately exempt: it is permanent
        // identity evidence that prevents reuse after all other state is gone.
        let _transaction = self.write_transaction.lock_recover();
        let ids = {
            let mut tombstoned = self.tombstoned_node_ids.lock_recover();
            std::mem::take(&mut *tombstoned)
        };
        let removed_node_count = if ids.is_empty() {
            0
        } else {
            let mut graph = self.global_session_graph.lock_recover();
            let before = graph.nodes.len();
            let nodes = graph
                .nodes
                .iter()
                .filter(|node| !ids.contains(&node.node_id))
                .cloned()
                .collect::<Vec<_>>();
            let removed_node_count = before.saturating_sub(nodes.len());
            *graph = crate::SessionGraph::from_nodes(nodes, None)?;
            self.global_node_owners
                .lock_recover()
                .retain(|node_id, _| !ids.contains(node_id));
            removed_node_count
        };
        let mut pending = self.pending_turn_inputs.lock_recover();
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
