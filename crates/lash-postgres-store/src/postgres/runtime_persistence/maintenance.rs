use super::*;

#[async_trait::async_trait]
impl StoreMaintenance for PostgresSessionStore {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<VacuumReport> {
        self.vacuum_tombstones()
            .await
            .map_err(lash_core::MaintenanceFailure::failed_before_any_work)
    }

    /// Checkpoint-rooted mark/sweep over `lash_blobs`, mirroring the SQLite
    /// store's semantics ([`GcReport`] fields match). PostgreSQL stores each
    /// checkpoint as one manifest plus separately addressed tool, plugin, and
    /// execution-state components. The four Lashlang artifact namespaces live
    /// in a separate, upsert-in-place table (`lash_lashlang_artifacts`).
    /// Session-owned trigger-manifest rows are removed with their session; the
    /// other artifact rows are retained service roots, so GC does not touch
    /// this table.
    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<GcReport> {
        // One transaction: a failure rolls every delete back, so no work
        // survived to report.
        self.gc_unreachable_blobs()
            .await
            .map_err(lash_core::MaintenanceFailure::failed_before_any_work)
    }
}

impl PostgresSessionStore {
    async fn vacuum_tombstones(&self) -> Result<VacuumReport, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // `lash_deleted_sessions` is deliberately exempt: it is permanent
        // identity evidence and must survive every retention-pruning pass (FIG-754 / FIG-748).
        let removed_node_count =
            sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1 AND tombstoned = TRUE")
                .bind(self.session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected() as usize;
        let removed_pending_turn_input_tombstone_count = sqlx::query(
            "DELETE FROM lash_pending_turn_inputs
             WHERE session_id = $1 AND state IN ($2, $3)",
        )
        .bind(self.session_id.as_str())
        .bind(lash_core::TurnInputState::Cancelled.as_str())
        .bind(lash_core::TurnInputState::Completed.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        sqlx::query("DELETE FROM lash_turn_cancel_requests WHERE session_id = $1")
            .bind(self.session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
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
        let root_refs = sqlx::query_scalar::<_, String>(
            "SELECT checkpoint_ref FROM lash_sessions WHERE checkpoint_ref IS NOT NULL
             UNION
             SELECT checkpoint_ref FROM lash_node_anchors",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let root_count = root_refs.len();
        let mut retained = std::collections::BTreeSet::<String>::new();
        for checkpoint_hash in root_refs {
            if !retained.insert(checkpoint_hash.clone()) {
                continue;
            }
            // A rooted manifest is live. Decode it and retain every component
            // blob it references. A present-yet-undecodable manifest is a
            // hard error so GC aborts rather than dropping a live checkpoint's
            // children; an absent one was already collected on a prior run.
            let bytes: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT content FROM lash_blobs WHERE hash = $1")
                    .bind(&checkpoint_hash)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
            let Some(bytes) = bytes else {
                continue;
            };
            let manifest: SessionCheckpoint = decode_versioned_msgpack_record(
                &bytes,
                "SessionCheckpoint",
                lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
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
        sqlx::query(
            "DELETE FROM lash_checkpoint_blob_refs AS edge
             WHERE NOT EXISTS (
                       SELECT 1 FROM lash_sessions AS head
                       WHERE head.checkpoint_ref = edge.checkpoint_ref
                   )
               AND NOT EXISTS (
                       SELECT 1 FROM lash_node_anchors AS anchor
                       WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                   )",
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let all_hashes =
            sqlx::query_scalar::<_, String>("SELECT hash FROM lash_blobs ORDER BY hash ASC")
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        let mut deleted_blob_count = 0usize;
        for hash in &all_hashes {
            if retained.contains(hash) {
                continue;
            }
            sqlx::query("DELETE FROM lash_blobs WHERE hash = $1")
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
