use super::*;
use crate::guarded_tx::begin_collecting;
use crate::session_sql::session_sql;

#[async_trait::async_trait]
impl StoreMaintenance for PostgresStore {
    async fn vacuum(
        &self,
        session_id: &SessionId,
    ) -> lash_core_execution::MaintenanceResult<VacuumReport> {
        self.vacuum_tombstones(session_id)
            .await
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)
    }

    /// Reachability mark/sweep over `lash_blobs` from every root the graph
    /// root catalog names, mirroring the SQLite store's ([`GcReport`] fields
    /// match). A missing root or child, a corrupt node or an incomplete
    /// source fails the collection with nothing swept. Lash VM artifact rows
    /// (`lash_lash_vm_artifacts`) are not swept: only a bridged artifact's
    /// graph node is a root.
    async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        // One transaction: a failure rolls every delete back, so no work
        // survived to report.
        self.gc_unreachable_blobs()
            .await
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)
    }
}

impl PostgresStore {
    async fn vacuum_tombstones(&self, session_id: &SessionId) -> Result<VacuumReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        // `lash_deleted_sessions` is deliberately exempt: it is permanent
        // identity evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let removed_node_count = sqlx::query(
            session_sql()
                .graph_postgres
                .delete_tombstoned_for_session
                .sql(),
        )
        .bind(session_id.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        let removed_pending_turn_input_tombstone_count = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .delete_withdrawn
                .sql(),
        )
        .bind(session_id.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // A vacuumed command's replay meets its revision check.
        let removed_queued_work_tombstone_count = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .delete_tombstones
                .sql(),
        )
        .bind(session_id.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // Cancellation rows include unresolved recovery intent. They remain
        // until session deletion, which is the only safe reclamation boundary
        // without terminal correlation.
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(VacuumReport {
            removed_node_count,
            removed_pending_turn_input_tombstone_count: removed_pending_turn_input_tombstone_count
                as usize,
            removed_queued_work_tombstone_count: removed_queued_work_tombstone_count as usize,
        })
    }
    async fn gc_unreachable_blobs(&self) -> Result<GcReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        // A host collection is what ends `until_gc`'s hold: release every
        // revision no head, pin or retention window keeps, and retire every
        // expired read pin, in an ordinary guarded transaction of its own
        // (FIG-5887 §8). A crash between it and the collection leaves
        // released roots whose bytes the next collection sweeps.
        {
            let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
            crate::revisions::release_unretained_tx(&mut tx, true, None).await?;
            let now = i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX);
            crate::durable::retire_expired_read_pins_in(&mut tx, now)
                .await
                .map_err(store_sqlx_error)?;
            tx.commit().await.map_err(store_sqlx_error)?;
        }
        // The collection takes the graph gate exclusive ahead of the writer
        // fence: every writer, hydration and pin fetch holds it shared, so
        // the root enumeration, the mark and the sweep are one atomic step
        // against all of them.
        let mut tx = begin_collecting(&mut *connection, &self.fence).await?;
        let collection = crate::durable::collect_graph_in(
            &mut tx,
            lash_durable::domain::graph_commit::CollectionLimits::UNBOUNDED,
        )
        .await
        .map_err(|refusal| StoreError::Backend(refusal.to_string()))?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let as_usize = |count: u64| usize::try_from(count).unwrap_or(usize::MAX);
        Ok(GcReport {
            root_count: as_usize(collection.roots.values().sum()),
            retained_blob_count: as_usize(collection.marked),
            deleted_blob_count: as_usize(collection.swept_blobs),
        })
    }
}
