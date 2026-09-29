use super::*;
use crate::session_sql::session_sql;

#[async_trait::async_trait]
impl StoreMaintenance for SqliteStore {
    async fn vacuum(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<VacuumReport> {
        // `deleted_sessions` is deliberately exempt: it is permanent identity
        // evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let session_id = session_id.clone();
        let (removed_node_count, removed_pending_turn_input_tombstone_count) = self
            .conn
            .write(move |tx| {
                let removed_node_count = crate::conn::cached_execute(tx,
                    session_sql()
                        .graph_sqlite
                        .delete_tombstoned_for_session
                        .sql(),
                    params![session_id.as_str()],
                )?;
                let removed_pending_turn_input_tombstone_count = crate::conn::cached_execute(tx,
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs
                        .delete_withdrawn
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
                lash_core_execution::MaintenanceFailure::failed_before_any_work(sqlite_error(err))
            })?;
        Ok(VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count,
        })
    }

    async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        SqliteStore::gc_unreachable(self).await
    }
}
