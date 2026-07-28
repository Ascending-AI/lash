use crate::*;

#[async_trait::async_trait]
impl SessionStoreFactory for PostgresSessionStoreFactory {
    async fn create_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn RuntimePersistence>, String> {
        let store = PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::clone(&self.clock),
            session_id: Some(request.session_id.clone()),
            bound_session: Arc::new(OnceLock::new()),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            incarnation_id: lash_core::IncarnationId::mint_for_store(),
            session_name: request.session_id.clone(),
            created_at: current_timestamp_string(),
            model: request.policy.model.id.clone(),
            cwd: std::env::current_dir()
                .ok()
                .and_then(|path| path.to_str().map(str::to_string)),
            relation: request.relation.clone(),
        };
        let mut tx = self.pool.begin().await.map_err(|err| err.to_string())?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await
            .map_err(|err| err.to_string())?;
        sqlx::query("DELETE FROM lash_deleted_sessions WHERE session_id = $1")
            .bind(&request.session_id)
            .execute(&mut *tx)
            .await
            .map_err(|err| err.to_string())?;
        sqlx::query(
            "INSERT INTO lash_session_meta (session_id, meta_json)
             VALUES ($1, $2)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(&request.session_id)
        .bind(encode_json(&meta))
        .execute(&mut *tx)
        .await
        .map_err(|err| err.to_string())?;
        tx.commit().await.map_err(|err| err.to_string())?;
        Ok(Arc::new(store))
    }

    async fn open_existing_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn RuntimePersistence>>, String> {
        let store = PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::clone(&self.clock),
            session_id: Some(request.session_id.clone()),
            bound_session: Arc::new(OnceLock::new()),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        if store
            .load_session_meta()
            .await
            .map_err(|err| err.to_string())?
            .is_some()
        {
            Ok(Some(Arc::new(store)))
        } else {
            Ok(None)
        }
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let mut tx = self.pool.begin().await.map_err(|err| err.to_string())?;
        delete_session_tx(&mut tx, session_id)
            .await
            .map_err(|err| err.to_string())?;
        tx.commit().await.map_err(|err| err.to_string())
    }

    async fn pin(&self, node_id: &str) -> Result<lash_core::ForkPoint, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let live_node = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if live_node.is_none() {
            return Err(StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        if let Some((checkpoint_ref, source_session_id)) = sqlx::query_as::<_, (String, String)>(
            "SELECT checkpoint_ref, source_session_id
             FROM lash_node_anchors WHERE node_id = $1",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::ForkPoint {
                node_id: node_id.to_string(),
                checkpoint_ref: checkpoint_ref.into(),
                source_session_id,
                pinned: true,
            });
        }
        let retained = sqlx::query_as::<_, (String, String)>(
            "SELECT session_id, checkpoint_ref FROM lash_sessions
             WHERE leaf_node_id = $1 AND checkpoint_ref IS NOT NULL
             ORDER BY session_id LIMIT 1
             FOR SHARE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            retained.ok_or_else(|| StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            })?;
        let changed = sqlx::query(
            "UPDATE lash_graph_nodes SET incoming_refs = incoming_refs + 1
             WHERE node_id = $1 AND tombstoned = FALSE",
        )
        .bind(node_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::ForkPointNotRetained {
                node_id: node_id.to_string(),
            });
        }
        sqlx::query(
            "INSERT INTO lash_node_anchors (node_id, checkpoint_ref, source_session_id)
             VALUES ($1, $2, $3)",
        )
        .bind(node_id)
        .bind(&checkpoint_ref)
        .bind(&source_session_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::ForkPoint {
            node_id: node_id.to_string(),
            checkpoint_ref: checkpoint_ref.into(),
            source_session_id,
            pinned: true,
        })
    }

    async fn unpin(&self, node_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            "SELECT node_id FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let removed = sqlx::query("DELETE FROM lash_node_anchors WHERE node_id = $1")
            .bind(node_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        if removed == 1 {
            crate::runtime_persistence::decrement_node_ref_tx(&mut tx, node_id).await?;
        }
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn fork_points(&self) -> Result<Vec<lash_core::ForkPoint>, StoreError> {
        let rows = sqlx::query(
            "SELECT node_id, checkpoint_ref, source_session_id, pinned
             FROM (
                 SELECT DISTINCT ON (node_id)
                        node_id, checkpoint_ref, source_session_id, pinned
                 FROM (
                     SELECT node_id, checkpoint_ref, source_session_id,
                            TRUE AS pinned, 0 AS priority
                     FROM lash_node_anchors
                     UNION ALL
                     SELECT leaf_node_id, checkpoint_ref, session_id,
                            FALSE AS pinned, 1 AS priority
                     FROM lash_sessions
                     WHERE leaf_node_id IS NOT NULL AND checkpoint_ref IS NOT NULL
                 ) candidates
                 ORDER BY node_id, priority, source_session_id
             ) retained
             ORDER BY node_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        Ok(rows
            .into_iter()
            .map(|row| lash_core::ForkPoint {
                node_id: row.get(0),
                checkpoint_ref: BlobRef(row.get(1)),
                source_session_id: row.get(2),
                pinned: row.get(3),
            })
            .collect())
    }

    async fn fork_at(
        &self,
        request: &lash_core::ForkSessionRequest,
    ) -> Result<lash_core::ForkSessionResult, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::lock_session_history_mutation_tx(&mut tx, &request.session_id)
            .await?;
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                 SELECT 1 FROM lash_sessions WHERE session_id = $1
                 UNION ALL
                 SELECT 1 FROM lash_session_meta WHERE session_id = $1
             )",
        )
        .bind(&request.session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if exists {
            return Err(StoreError::ForkSessionAlreadyExists {
                session_id: request.session_id.clone(),
            });
        }
        let live_node = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(&request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if live_node.is_none() {
            return Err(StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
        let retained = sqlx::query_as::<_, (String, String)>(
            "SELECT source_session_id, checkpoint_ref FROM (
                 SELECT source_session_id, checkpoint_ref, 0 AS priority
                 FROM lash_node_anchors WHERE node_id = $1
                 UNION ALL
                 SELECT session_id, checkpoint_ref, 1 AS priority FROM lash_sessions
                 WHERE leaf_node_id = $1 AND checkpoint_ref IS NOT NULL
             ) retained
             ORDER BY priority, source_session_id LIMIT 1",
        )
        .bind(&request.node_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let (source_session_id, checkpoint_ref) =
            retained.ok_or_else(|| StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            })?;
        if let lash_core::SessionRelation::Fork {
            source_session_id: expected,
            ..
        } = &request.relation
            && expected != &source_session_id
        {
            return Err(StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
        let current_frame_node_id =
            crate::runtime_persistence::nearest_frame_node_id_tx(&mut tx, &request.node_id)
                .await?
                .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                    leaf_node_id: request.node_id.clone(),
                })?;
        let graph_node_count = sqlx::query_scalar::<_, i64>(
            "WITH RECURSIVE ancestry(node_id, parent_node_id) AS (
                 SELECT node_id, parent_node_id FROM lash_graph_nodes
                 WHERE node_id = $1 AND tombstoned = FALSE
                 UNION ALL
                 SELECT parent.node_id, parent.parent_node_id
                 FROM lash_graph_nodes parent
                 JOIN ancestry ON parent.node_id = ancestry.parent_node_id
                 WHERE parent.tombstoned = FALSE
             )
             SELECT COUNT(*) FROM ancestry",
        )
        .bind(&request.node_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)? as usize;
        let changed = sqlx::query(
            "UPDATE lash_graph_nodes SET incoming_refs = incoming_refs + 1
             WHERE node_id = $1 AND tombstoned = FALSE",
        )
        .bind(&request.node_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::ForkPointNotRetained {
                node_id: request.node_id.clone(),
            });
        }
        let config = lash_core::PersistedSessionConfig {
            provider_id: request.policy.recorded_provider_id().to_string(),
            model: request.policy.model.clone(),
        };
        let head = lash_core::store::SessionHeadMeta {
            schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: request.session_id.clone(),
            head_revision: 0,
            config,
            current_frame_node_id: Some(current_frame_node_id),
            checkpoint_ref: Some(checkpoint_ref.clone().into()),
            leaf_node_id: Some(request.node_id.clone()),
            graph_node_count,
            token_ledger: Vec::new(),
        };
        sqlx::query(
            "INSERT INTO lash_sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
             VALUES ($1, 0, $2, $3, $4)",
        )
        .bind(&request.session_id)
        .bind(encode_json(&head))
        .bind(&checkpoint_ref)
        .bind(&request.node_id)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let meta = SessionMeta {
            session_id: request.session_id.clone(),
            incarnation_id: lash_core::IncarnationId::mint_for_store(),
            session_name: request.session_id.clone(),
            created_at: current_timestamp_string(),
            model: request.policy.model.id.clone(),
            cwd: std::env::current_dir()
                .ok()
                .and_then(|path| path.to_str().map(str::to_string)),
            relation: request.relation.clone(),
        };
        sqlx::query("INSERT INTO lash_session_meta (session_id, meta_json) VALUES ($1, $2)")
            .bind(&request.session_id)
            .bind(encode_json(&meta))
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query("DELETE FROM lash_deleted_sessions WHERE session_id = $1")
            .bind(&request.session_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::ForkSessionResult {
            session_id: request.session_id.clone(),
            node_id: request.node_id.clone(),
            source_session_id,
        })
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<std::collections::BTreeSet<lash_core::AttachmentId>, lash_core::StoreError> {
        // Age is only a post-terminal retention policy. This single DELETE
        // composes age with durable owner-death proof: a later committed turn
        // supersedes a turn owner, a missing process row proves a process owner
        // was pruned, and only unscoped host puts use age alone.
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let process_dead = if self.process_registry_shared {
            "OR (
                manifest.owner_kind = 'process'
                AND NOT EXISTS (
                    SELECT 1
                    FROM lash_processes AS process
                    WHERE process.process_id = manifest.owner_id
                )
            )"
        } else {
            ""
        };
        let delete_sql = format!(
            "DELETE FROM lash_attachment_manifest AS manifest
             WHERE manifest.committed_at_ms IS NULL
               AND manifest.intent_at_ms <= $1
               AND (
                    manifest.owner_kind IS NULL
                    OR (
                        manifest.owner_kind = 'turn'
                        AND EXISTS (
                            SELECT 1
                            FROM lash_runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
                    {process_dead}
               )"
        );
        sqlx::query(&delete_sql)
            .bind(intent_grace_cutoff_epoch_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let rows = sqlx::query("SELECT DISTINCT attachment_id FROM lash_attachment_manifest")
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(rows
            .into_iter()
            .map(|row| lash_core::AttachmentId::new(row.get::<String, _>(0)))
            .collect())
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash_core::StoreError> {
        // Read-only delete-time probe using the same age + owner-death predicate
        // as reconciliation. A ref is live unless it is eligible for that
        // conditional forget.
        let process_dead = if self.process_registry_shared {
            "OR (
                manifest.owner_kind = 'process'
                AND NOT EXISTS (
                    SELECT 1 FROM lash_processes AS process
                    WHERE process.process_id = manifest.owner_id
                )
            )"
        } else {
            ""
        };
        let select_sql = format!(
            "SELECT 1 FROM lash_attachment_manifest AS manifest
             WHERE manifest.attachment_id = $1
               AND NOT (
                    manifest.committed_at_ms IS NULL
                    AND manifest.intent_at_ms <= $2
                    AND (
                        manifest.owner_kind IS NULL
                        OR (
                            manifest.owner_kind = 'turn'
                            AND EXISTS (
                                SELECT 1 FROM lash_runtime_turn_commits AS turn_commit
                                WHERE turn_commit.session_id = manifest.session_id
                                  AND turn_commit.turn_id <> manifest.owner_id
                                  AND turn_commit.committed_at_ms > manifest.intent_at_ms
                            )
                        )
                        {process_dead}
                    )
               )
             LIMIT 1"
        );
        let row = sqlx::query(&select_sql)
            .bind(id.as_str())
            .bind(intent_grace_cutoff_epoch_ms as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(row.is_some())
    }
}

pub(crate) async fn delete_session_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<(), StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    sqlx::query(
        "INSERT INTO lash_deleted_sessions (session_id)
         VALUES ($1)
         ON CONFLICT (session_id) DO NOTHING",
    )
    .bind(session_id)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    // Attachment intents are released before the rest of the process-owned
    // store. Process pruning calls this while its terminal row is still locked
    // and deletes that row last, so any failure leaves a process leak instead
    // of making live-looking session state ownerless.
    let leaf_node_id = sqlx::query_scalar::<_, String>(
        "SELECT leaf_node_id FROM lash_sessions
         WHERE session_id = $1 AND leaf_node_id IS NOT NULL
         FOR UPDATE",
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    sqlx::query("DELETE FROM lash_sessions WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(leaf_node_id) = leaf_node_id {
        crate::runtime_persistence::decrement_node_ref_tx(tx, &leaf_node_id).await?;
    }
    let zero_ref_nodes = sqlx::query(
        "SELECT node_id, parent_node_id FROM lash_graph_nodes
         WHERE session_id = $1 AND tombstoned = FALSE AND incoming_refs = 0
         ORDER BY seq DESC
         FOR UPDATE",
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for row in zero_ref_nodes {
        let node_id: String = row.get(0);
        let parent_node_id: Option<String> = row.get(1);
        let cached = sqlx::query_scalar::<_, i64>(
            "SELECT incoming_refs FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE",
        )
        .bind(&node_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if cached != Some(0) {
            continue;
        }
        let derived = crate::runtime_persistence::derived_node_refcount_tx(tx, &node_id).await?;
        if derived != 0 {
            return Err(StoreError::NodeRefcountDrift {
                node_id,
                cached: 0,
                derived,
            });
        }
        sqlx::query("UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = $1")
            .bind(&node_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if let Some(parent_node_id) = parent_node_id {
            crate::runtime_persistence::decrement_node_ref_tx(tx, &parent_node_id).await?;
        }
    }
    sqlx::query("DELETE FROM lash_graph_nodes WHERE tombstoned = TRUE")
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for sql in [
        "DELETE FROM lash_attachment_manifest WHERE session_id = $1",
        "DELETE FROM lash_queued_work_items WHERE batch_id IN (SELECT batch_id FROM lash_queued_work_batches WHERE session_id = $1)",
        "DELETE FROM lash_queued_work_batches WHERE session_id = $1",
        "DELETE FROM lash_pending_turn_inputs WHERE session_id = $1",
        "DELETE FROM lash_session_execution_leases WHERE session_id = $1",
        "DELETE FROM lash_usage_deltas WHERE session_id = $1",
        "DELETE FROM lash_runtime_turn_commits WHERE session_id = $1",
        "DELETE FROM lash_session_meta WHERE session_id = $1",
    ] {
        sqlx::query(sql)
            .bind(session_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    session_id: String,
    source_key: Option<String>,
    pub(crate) delivery_policy: DeliveryPolicy,
    pub(crate) slot_policy: SlotPolicy,
    pub(crate) merge_key: MergeKey,
    available_at_ms: u64,
    enqueued_at_ms: u64,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>("delivery_policy").as_str())
            .ok_or_else(|| {
                StoreError::Backend("invalid queued work delivery policy".to_string())
            })?;
    let slot_policy = SlotPolicy::from_wire_str(row.get::<String, _>("slot_policy").as_str())
        .ok_or_else(|| StoreError::Backend("invalid queued work slot policy".to_string()))?;
    let merge_json: String = row.get("merge_key_json");
    Ok(QueuedBatchRow {
        enqueue_seq: row.get::<i64, _>("enqueue_seq") as u64,
        batch_id: row.get("batch_id"),
        session_id: row.get("session_id"),
        source_key: row.get("source_key"),
        delivery_policy,
        slot_policy,
        merge_key: store_decode_json(&merge_json, "queued work merge key")?,
        available_at_ms: row.get::<i64, _>("available_at_ms") as u64,
        enqueued_at_ms: row.get::<i64, _>("enqueued_at_ms") as u64,
        claim_fencing_token: row.get::<i64, _>("claim_fencing_token") as u64,
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: row.get::<i64, _>("claim_session_lease_generation") as u64,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(
        "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_queued_work_batches
         WHERE batch_id = $1",
    )
    .bind(batch_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let row = queued_batch_row(row)?;
    queued_work_batch_from_row(tx, row).await.map(Some)
}

pub(crate) async fn queued_work_batch_from_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: QueuedBatchRow,
) -> Result<QueuedWorkBatch, StoreError> {
    let item_rows = sqlx::query(
        "SELECT item_id, payload_json
         FROM lash_queued_work_items
         WHERE batch_id = $1
         ORDER BY item_index ASC",
    )
    .bind(&row.batch_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut items = Vec::new();
    for item in item_rows {
        let payload_json: String = item.get(1);
        items.push(QueuedWorkItem {
            item_id: item.get(0),
            payload: store_decode_json(&payload_json, "queued work payload")?,
        });
    }
    Ok(QueuedWorkBatch {
        batch_id: row.batch_id,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        delivery_policy: row.delivery_policy,
        slot_policy: row.slot_policy,
        merge_key: row.merge_key,
        available_at_ms: row.available_at_ms,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    })
}

pub(crate) async fn ensure_queued_work_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &QueuedWorkCompletion,
) -> Result<(), StoreError> {
    for batch_id in &completed.batch_ids {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1::BIGINT FROM lash_queued_work_batches
             WHERE session_id = $1
               AND batch_id = $2
               AND claim_id = $3
               AND claim_token = $4
             LIMIT 1
             FOR UPDATE",
        )
        .bind(&completed.session_id)
        .bind(batch_id)
        .bind(&completed.claim_id)
        .bind(&completed.lease_token)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if exists.is_none() {
            return Err(StoreError::QueuedWorkClaimSuperseded {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    session_id: String,
    source_key: Option<String>,
    ingress_json: String,
    state: lash_core::TurnInputState,
    input_json: String,
    enqueued_at_ms: u64,
    claim_id: Option<String>,
    claim_fencing_token: u64,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_token: Option<String>,
    claim_session_lease_generation: u64,
}

pub(crate) fn pending_turn_input_row(row: PgRow) -> Result<PendingTurnInputRow, StoreError> {
    let state = lash_core::TurnInputState::from_wire_str(row.get::<String, _>("state").as_str())
        .ok_or_else(|| StoreError::Backend("invalid pending turn-input state".to_string()))?;
    Ok(PendingTurnInputRow {
        enqueue_seq: row.get::<i64, _>("enqueue_seq") as u64,
        input_id: row.get("input_id"),
        session_id: row.get("session_id"),
        source_key: row.get("source_key"),
        ingress_json: row.get("ingress_json"),
        state,
        input_json: row.get("input_json"),
        enqueued_at_ms: row.get::<i64, _>("enqueued_at_ms") as u64,
        claim_id: row.get("claim_id"),
        claim_fencing_token: row.get::<i64, _>("claim_fencing_token") as u64,
        claim_owner: lease_owner_from_columns(
            row.get("claim_owner_id"),
            row.get("claim_owner_incarnation_id"),
            row.get("claim_owner_liveness_json"),
        ),
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: row.get::<i64, _>("claim_session_lease_generation") as u64,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core::PendingTurnInput, StoreError> {
    Ok(lash_core::PendingTurnInput {
        input_id: row.input_id,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        ingress: store_decode_json(&row.ingress_json, "turn-input ingress")?,
        state: row.state,
        enqueued_at_ms: row.enqueued_at_ms,
        input: store_decode_json(&row.input_json, "turn input")?,
    })
}

pub(crate) async fn load_pending_turn_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    input_id: &str,
) -> Result<Option<lash_core::PendingTurnInput>, StoreError> {
    let row = sqlx::query(
        "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND input_id = $2",
    )
    .bind(session_id)
    .bind(input_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row)
        .transpose()?
        .map(pending_turn_input_from_row)
        .transpose()
}

pub(crate) async fn load_pending_turn_input_row_by_target_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    target: &lash_core::PendingTurnInputCancelTarget,
    for_update: bool,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    let for_update = if for_update { " FOR UPDATE" } else { "" };
    let row = match target {
        lash_core::PendingTurnInputCancelTarget::InputId(input_id) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND input_id = $2{for_update}"
        ))
        .bind(session_id)
        .bind(input_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?,
        lash_core::PendingTurnInputCancelTarget::SourceKey(source_key) => sqlx::query(&format!(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND source_key = $2{for_update}"
        ))
        .bind(session_id)
        .bind(source_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?,
    };
    row.map(pending_turn_input_row).transpose()
}

fn pending_turn_input_claim_diagnostics_from_row(
    row: &PendingTurnInputRow,
) -> Option<lash_core::PendingTurnInputClaimDiagnostics> {
    (row.claim_token.is_some() || matches!(row.state, lash_core::TurnInputState::Accepted)).then(
        || lash_core::PendingTurnInputClaimDiagnostics {
            state: row.state,
            claim_id: row.claim_id.clone(),
            claim_owner: row.claim_owner.clone(),
            claim_session_lease_generation: row
                .claim_token
                .as_ref()
                .map(|_| row.claim_session_lease_generation),
            claim_fencing_token: row.claim_fencing_token,
        },
    )
}

pub(crate) async fn cancel_pending_turn_input_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
) -> Result<lash_core::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state {
        lash_core::TurnInputState::Cancelled => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input),
        ),
        lash_core::TurnInputState::Completed => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(input),
        ),
        lash_core::TurnInputState::Accepted => {
            Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                input,
                claim: pending_turn_input_claim_diagnostics_from_row(&row),
            })
        }
        lash_core::TurnInputState::PendingActive | lash_core::TurnInputState::DeferredNextTurn => {
            // A claim is live only while the session-execution-lease generation it
            // pins still holds the session lease (ADR 0029).
            let live_claim = row.claim_token.is_some()
                && load_session_execution_lease_tx(tx, &row.session_id)
                    .await?
                    .is_some_and(|lease| {
                        lease.lease_token.is_some()
                            && lease.expires_at_ms > now_epoch_ms
                            && lease.fencing_token == row.claim_session_lease_generation
                    });
            if live_claim {
                return Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    input,
                    claim: pending_turn_input_claim_diagnostics_from_row(&row),
                });
            }
            sqlx::query(
                "UPDATE lash_pending_turn_inputs
                 SET state = $3,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_owner_liveness_json = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = $1 AND input_id = $2",
            )
            .bind(&row.session_id)
            .bind(&row.input_id)
            .bind(lash_core::TurnInputState::Cancelled.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            input.state = lash_core::TurnInputState::Cancelled;
            Ok(lash_core::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

pub(crate) async fn ensure_turn_input_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &lash_core::TurnInputCompletion,
) -> Result<(), StoreError> {
    for input_id in &completed.input_ids {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1::BIGINT FROM lash_pending_turn_inputs
             WHERE session_id = $1
               AND input_id = $2
               AND claim_id = $3
               AND claim_token = $4
             LIMIT 1
             FOR UPDATE",
        )
        .bind(&completed.session_id)
        .bind(input_id)
        .bind(&completed.claim_id)
        .bind(&completed.lease_token)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if exists.is_none() {
            return Err(StoreError::TurnInputClaimSuperseded {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct TurnInputClaimLease {
    pub(crate) claim_id: String,
    pub(crate) lease_token: String,
    pub(crate) fencing_token: u64,
    pub(crate) session_lease_generation: u64,
}

impl TurnInputClaimLease {
    pub(crate) fn derive(
        head: &PendingTurnInputRow,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        now_epoch_ms: u64,
        session_lease_generation: u64,
    ) -> Self {
        let fencing_token = head.claim_fencing_token.saturating_add(1);
        let claim_id = format!("tic:{}:{fencing_token}", head.enqueue_seq);
        let lease_token = format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{}:{}:{}:{}:{}",
                    session_id, owner.owner_id, owner.incarnation_id, claim_id, now_epoch_ms
                )
                .as_bytes(),
            )
        );
        Self {
            claim_id,
            lease_token,
            fencing_token,
            session_lease_generation,
        }
    }
}
