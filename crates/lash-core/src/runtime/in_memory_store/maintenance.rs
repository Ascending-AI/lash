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

    async fn vacuum(&self) -> crate::store::MaintenanceResult<crate::store::VacuumReport> {
        // `deleted_session_ids` is deliberately exempt: it is permanent
        // identity evidence that prevents reuse after all other state is gone.
        // The binding is the only admissible scope source, matching the SQLite
        // backend: metadata a handle merely happened to read is not a binding,
        // and inferring scope from it would let an unbound handle vacuum.
        let session_id = self
            .bound_session_id
            .lock_recover()
            .clone()
            .ok_or_else(|| {
                crate::store::MaintenanceFailure::failed_before_any_work(
                    crate::store::StoreError::SessionNotBound,
                )
            })?;

        let _transaction = self.write_transaction.lock_recover();
        let session_tombstones = {
            let mut tombstoned = self.tombstoned_node_ids.lock_recover();
            let owners = self.global_node_owners.lock_recover();
            let session_tombstones: std::collections::HashSet<String> = tombstoned
                .iter()
                .filter(|node_id| {
                    owners
                        .get(*node_id)
                        .is_some_and(|owner| owner == &session_id)
                })
                .cloned()
                .collect();
            tombstoned.retain(|node_id| !session_tombstones.contains(node_id));
            session_tombstones
        };
        let removed_node_count = if session_tombstones.is_empty() {
            0
        } else {
            let mut graph = self.global_session_graph.lock_recover();
            let before = graph.nodes.len();
            let nodes = graph
                .nodes
                .iter()
                .filter(|node| !session_tombstones.contains(&node.node_id))
                .cloned()
                .collect::<Vec<_>>();
            let removed_node_count = before.saturating_sub(nodes.len());
            *graph = crate::SessionGraph::from_nodes(nodes, None).map_err(|error| {
                // Nothing was physically removed before this point: the node
                // rebuild is the removal.
                crate::store::MaintenanceFailure::failed_before_any_work(error)
            })?;
            self.global_node_owners
                .lock_recover()
                .retain(|node_id, _| !session_tombstones.contains(node_id));
            removed_node_count
        };
        let mut pending = self.pending_turn_inputs.lock_recover();
        let before = pending.len();
        pending.retain(|entry| {
            !(entry.input.session_id == session_id
                && matches!(
                    entry.input.state,
                    crate::TurnInputState::Cancelled | crate::TurnInputState::Completed
                ))
        });
        self.turn_cancel_requests
            .lock_recover()
            .retain(|_, record| record.request.address.session_id != session_id);
        Ok(crate::store::VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count: before.saturating_sub(pending.len()),
        })
    }

    /// Sweep the factory-global checkpoint blob map against the live root
    /// edges every session's last commit recorded, plus the components every
    /// node anchor holds.
    ///
    /// The root set is enumerated in full under the write transaction, so an
    /// empty sweep here is witnessed emptiness and not a stand-in for "this
    /// backend does not collect".
    async fn gc_unreachable(&self) -> crate::store::MaintenanceResult<crate::store::GcReport> {
        let _transaction = self.write_transaction.lock_recover();
        let mut retained_refs = std::collections::HashSet::new();
        let mut root_count = 0usize;
        for session_roots in self.checkpoint_blob_roots.lock_recover().values() {
            root_count += 1;
            retained_refs.extend(session_roots.iter().cloned());
        }
        for (_, checkpoint, _) in self.node_anchors.lock_recover().values() {
            root_count += 1;
            retained_refs.extend(
                checkpoint
                    .components
                    .values()
                    .filter_map(|component| component.blob_ref().cloned()),
            );
        }
        let mut blobs = self.checkpoint_component_blobs.lock_recover();
        let before = blobs.len();
        blobs.retain(|blob_ref, _| retained_refs.contains(blob_ref));
        let retained_blob_count = blobs.len();
        Ok(crate::store::GcReport {
            root_count,
            retained_blob_count,
            deleted_blob_count: before.saturating_sub(retained_blob_count),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    /// The in-memory backend answers in the same maintenance outcome contract
    /// as the durable ones. It used to return `GcReport::default()`
    /// unconditionally, which is the "nothing to do" arm spelled without ever
    /// looking.
    #[tokio::test]
    async fn in_memory_store_satisfies_the_maintenance_outcome_contract() {
        crate::testing::conformance::store_maintenance_outcome_contract(
            "in-memory",
            || {
                Arc::new(super::super::factory::InMemorySessionStoreFactory::new())
                    as Arc<dyn crate::SessionStoreFactory>
            },
            // The in-memory sweep reads only process memory under the write
            // transaction: it has no failure path to inject.
            None,
        )
        .await;
    }
}
