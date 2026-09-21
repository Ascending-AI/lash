use super::*;
use crate::session_sql::session_sql;

#[async_trait::async_trait]
impl StoreMaintenance for Store {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<VacuumReport> {
        // `deleted_sessions` is deliberately exempt: it is permanent identity
        // evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let session_id = self.session_id.get().cloned().ok_or_else(|| {
            lash_core::MaintenanceFailure::failed_before_any_work(StoreError::SessionNotBound)
        })?;
        let (removed_node_count, removed_pending_turn_input_tombstone_count) = self
            .conn
            .write(move |tx| {
                let removed_node_count = tx.execute(
                    session_sql()
                        .graph_sqlite
                        .delete_tombstoned_for_session
                        .sql(),
                    params![session_id.as_str()],
                )?;
                let removed_pending_turn_input_tombstone_count = tx.execute(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs
                        .delete_terminal
                        .sql(),
                    params![session_id.as_str()],
                )?;
                // Cancellation rows include unresolved recovery intent. They
                // remain until session deletion, which is the only safe
                // reclamation boundary without terminal correlation.
                Ok((
                    removed_node_count,
                    removed_pending_turn_input_tombstone_count,
                ))
            })
            .await
            // Both deletes ride in one transaction: a failure rolls them back,
            // so no rows survived to report.
            .map_err(|err| {
                lash_core::MaintenanceFailure::failed_before_any_work(sqlite_error(err))
            })?;
        Ok(VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count,
        })
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<GcReport> {
        Store::gc_unreachable(self).await
    }
}
