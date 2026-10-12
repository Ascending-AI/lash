use super::*;
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

    /// Release what the host's collection ends, then audit for orphans,
    /// mirroring the SQLite store's ([`GcReport`] fields match). Every
    /// transaction that retires a root peels it, so no mark and sweep runs
    /// and no lock orders this pass against writers or readers. Lash VM
    /// artifact rows (`lash_lash_vm_artifacts`) are not blobs: only a bridged
    /// artifact's graph node is a root.
    async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        let mut report = GcReport::default();
        let outcome = self.gc_unreachable_blobs(&mut report).await;
        outcome.map(|()| report.clone()).map_err(|error| {
            if report.deleted_blob_count == 0 {
                lash_core_execution::MaintenanceFailure::failed_before_any_work(error)
            } else {
                lash_core_execution::MaintenanceFailure::failed(error, report)
            }
        })
    }
}

impl PostgresStore {
    async fn vacuum_tombstones(&self, session_id: &SessionId) -> Result<VacuumReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        // The session's write lock, which every enqueue takes before it
        // interns a spec, taken before any row this pass deletes: an
        // admission that reuses a spec row commits its input before the
        // reclaim below reads the inputs (FIG-6021).
        super::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        // `lash_deleted_sessions` is deliberately exempt: it is permanent
        // identity evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let removed_node_count = crate::observed_sql::query(
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
        let removed_pending_turn_input_tombstone_count = crate::observed_sql::query(
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
        let removed_queued_work_tombstone_count = crate::observed_sql::query(
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
        // A run's spec bytes are read only to resolve its shape: once no
        // unsettled input names a spec, nothing reads it again (FIG-6021).
        crate::observed_sql::query(
            crate::turn_ingress::turn_ingress_sql()
                .run_specs
                .delete_unnamed
                .sql(),
        )
        .bind(session_id.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
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
    /// The body of [`StoreMaintenance::gc_unreachable`]: every peel it
    /// commits counts into `report` as it commits.
    async fn gc_unreachable_blobs(&self, report: &mut GcReport) -> Result<(), StoreError> {
        let graph_error = |refusal| StoreError::Backend(format!("{refusal}"));
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        // A host collection is what ends `until_gc`'s hold: release every
        // revision no head, pin or retention window keeps, peeling what
        // they held, in an ordinary guarded transaction of its own.
        {
            let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
            let released = crate::revisions::release_unretained_tx(&mut tx, true, None).await?;
            tx.commit().await.map_err(store_sqlx_error)?;
            report.deleted_blob_count += released;
        }
        // The orphan audit: a page per guarded transaction, each locking
        // only the rows it deletes, so commits beside it progress.
        let mut after = String::new();
        loop {
            let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
            let (last, gone) =
                crate::durable::audit_page(&mut tx, &after, crate::durable::AUDIT_PAGE)
                    .await
                    .map_err(graph_error)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            report.deleted_blob_count += gone;
            match last {
                Some(last) => after = last,
                None => break,
            }
        }
        let (roots, blobs) = crate::durable::graph_counts(&mut connection)
            .await
            .map_err(graph_error)?;
        report.root_count = roots;
        report.retained_blob_count = blobs;
        Ok(())
    }
}
