use super::*;
use crate::session_sql::session_sql;

#[async_trait::async_trait]
impl StoreMaintenance for PostgresSessionStore {
    async fn vacuum(&self) -> lash_core_execution::MaintenanceResult<VacuumReport> {
        self.vacuum_tombstones()
            .await
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)
    }

    /// Checkpoint-rooted mark/sweep over `lash_blobs`, mirroring the SQLite
    /// store's semantics ([`GcReport`] fields match). PostgreSQL stores each
    /// checkpoint as one manifest plus separately addressed tool, plugin, and
    /// execution-state components. The Lashlang artifact namespaces live
    /// in a separate, upsert-in-place table (`lash_lashlang_artifacts`).
    /// Those artifact rows are retained service roots, so GC does not touch
    /// this table.
    async fn gc_unreachable(&self) -> lash_core_execution::MaintenanceResult<GcReport> {
        // One transaction: a failure rolls every delete back, so no work
        // survived to report.
        self.gc_unreachable_blobs()
            .await
            .map_err(lash_core_execution::MaintenanceFailure::failed_before_any_work)
    }
}

impl PostgresSessionStore {
    async fn vacuum_tombstones(&self) -> Result<VacuumReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // `lash_deleted_sessions` is deliberately exempt: it is permanent
        // identity evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let removed_node_count = sqlx::query(
            session_sql()
                .graph_postgres
                .delete_tombstoned_for_session
                .sql(),
        )
        .bind(self.session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
        let removed_pending_turn_input_tombstone_count = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .delete_terminal
                .sql(),
        )
        .bind(self.session_id.as_str())
        .execute(&mut *tx)
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
        })
    }
    async fn gc_unreachable_blobs(&self) -> Result<GcReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // Serialize against concurrent checkpoint-blob writers. Every commit
        // INSERTs its new manifest into `lash_blobs` (holding a ROW EXCLUSIVE
        // lock) inside the same transaction that repoints `lash_sessions`, so an
        // EXCLUSIVE table lock makes the root read and the sweep atomic with
        // respect to every committer: a commit racing GC either lands fully
        // before the root read or blocks until GC releases. This is the fenced
        // transactional discipline the store uses on its other write paths.
        tx.execute("LOCK TABLE lash_blobs IN EXCLUSIVE MODE")
            .await
            .map_err(store_sqlx_error)?;
        // Roots: every live session's checkpoint manifest, across ALL sessions.
        // `lash_blobs` is a content-addressed table shared by the whole
        // database, so a blob shared across sessions must stay reachable while
        // ANY session references it — scoping roots to one session would delete
        // another session's live checkpoint.
        let root_refs =
            sqlx::query_scalar::<_, String>(session_sql().head.select_checkpoint_roots.sql())
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        let root_count = root_refs.len();
        let mut retained = std::collections::BTreeSet::<String>::new();
        for checkpoint_hash in root_refs {
            if !retained.insert(checkpoint_hash.clone()) {
                continue;
            }
            // A rooted manifest is live.
            // A present-yet-undecodable manifest is a hard error so GC aborts rather than
            // dropping a live checkpoint's children; an absent one was already collected on a
            // prior run.
            let bytes: Option<Vec<u8>> =
                sqlx::query_scalar(crate::blobs::blob_sql().shared.select_content.sql())
                    .bind(&checkpoint_hash)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            let Some(bytes) = bytes else {
                continue;
            };
            let manifest: SessionCheckpoint =
                lash_core_execution::store::decode_versioned_msgpack_record_for_fleet(
                    &bytes,
                    "SessionCheckpoint",
                    lash_core_execution::surface_format!(
                        lash_core_execution::store::SESSION_CHECKPOINT_SCHEMA_VERSION
                    ),
                    self.fleet_format,
                )?;
            // GC interprets only the root's ref graph, never component bodies.
            // Retain refs even when a newer writer used an unknown component
            // codec so an older binary cannot turn incompatibility into loss.
            for descriptor in manifest.components.values() {
                retained.insert(descriptor.blob_ref.0.clone());
            }
        }
        // Projection edges belong to live head/anchor roots. Sever every dead
        // root's complete outgoing set before hash-ordered blob deletion: a
        // component can sort before its root, and its strict FK must never be
        // weakened to accommodate stale ownership data.
        sqlx::query(session_sql().checkpoint_edges.delete_unrooted.sql())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let all_hashes = sqlx::query_scalar::<_, String>(
            crate::blobs::blob_sql().shared.select_all_hashes.sql(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut deleted_blob_count = 0usize;
        for hash in &all_hashes {
            if retained.contains(hash) {
                continue;
            }
            sqlx::query(crate::blobs::blob_sql().shared.delete_by_hash.sql())
                .bind(hash)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            deleted_blob_count += 1;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(GcReport {
            root_count,
            retained_blob_count: retained.len(),
            deleted_blob_count,
        })
    }
}
