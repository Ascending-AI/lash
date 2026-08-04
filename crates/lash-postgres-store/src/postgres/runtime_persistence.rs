use crate::*;

pub(crate) async fn lock_session_history_mutation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 1::bigint))")
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

pub(crate) async fn ensure_session_not_deleted_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<(), StoreError> {
    lock_session_history_mutation_tx(tx, session_id).await?;
    let deleted = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1 FROM lash_deleted_sessions WHERE session_id = $1
         )",
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: session_id.to_string(),
        })
    } else {
        Ok(())
    }
}

const POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE: &str = "session_id = $1
       AND available_at_ms <= FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000)
       AND (
            claim_token IS NULL
            OR claim_session_lease_generation <> $2
       )";

fn postgres_queued_work_head_candidate_cte(boundary: QueuedWorkClaimBoundary) -> String {
    let delivery_gate = match boundary {
        QueuedWorkClaimBoundary::Idle => "",
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint => {
            "WHERE head_delivery_policy = 'earliest_safe_boundary'"
        }
    };
    format!(
        "queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy
            FROM (
                SELECT enqueue_seq AS head_enqueue_seq,
                       batch_id AS head_batch_id,
                       delivery_policy AS head_delivery_policy
                FROM lash_queued_work_batches
                WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
                ORDER BY enqueue_seq ASC
                LIMIT 1
            ) AS unfiltered_head
            {delivery_gate}
         )"
    )
}

fn postgres_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> String {
    let head_candidate = postgres_queued_work_head_candidate_cte(boundary);
    format!(
        "WITH {head_candidate}
         SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_queued_work_batches
         CROSS JOIN queued_work_head_candidate
         WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
         ORDER BY enqueue_seq ASC
         LIMIT $3
         FOR UPDATE OF lash_queued_work_batches SKIP LOCKED"
    )
}

/// Reclaim the ancestry prefix with no live child, session-head root, or
/// explicit anchor. Every writer that adds an edge or root locks the target
/// node first, so the reachability query runs from a fresh snapshot after
/// concurrent additions have either committed or failed.
pub(crate) async fn retire_unreachable_ancestry_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    first_node_id: &str,
) -> Result<(), StoreError> {
    let mut node_id = first_node_id.to_string();
    loop {
        let parent_node_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT parent_node_id FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
             FOR UPDATE",
        )
        .bind(&node_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        let reachable = sqlx::query_scalar::<_, bool>(
            "SELECT
                EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE parent_node_id = $1 AND tombstoned = FALSE
                )
                OR EXISTS(
                    SELECT 1 FROM lash_sessions WHERE leaf_node_id = $1
                )
                OR EXISTS(
                    SELECT 1 FROM lash_node_anchors WHERE node_id = $1
                )",
        )
        .bind(&node_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if reachable {
            return Ok(());
        }
        sqlx::query("UPDATE lash_graph_nodes SET tombstoned = TRUE WHERE node_id = $1")
            .bind(&node_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        node_id = parent_node_id;
    }
}

pub(crate) async fn nearest_frame_node_id_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    leaf_node_id: &str,
) -> Result<Option<String>, StoreError> {
    sqlx::query_scalar(
        "WITH RECURSIVE ancestry(node_id, parent_node_id, node_json, depth) AS (
            SELECT node_id, parent_node_id, node_json, 0
            FROM lash_graph_nodes
            WHERE node_id = $1 AND tombstoned = FALSE
          UNION ALL
            SELECT parent.node_id, parent.parent_node_id, parent.node_json, ancestry.depth + 1
            FROM lash_graph_nodes AS parent
            JOIN ancestry ON parent.node_id = ancestry.parent_node_id
            WHERE parent.tombstoned = FALSE
        )
        SELECT node_id FROM ancestry
        WHERE node_json::jsonb ->> 'kind' = 'frame_open'
        ORDER BY depth ASC
        LIMIT 1",
    )
    .bind(leaf_node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

async fn enqueue_queued_work_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch: &QueuedWorkBatchDraft,
) -> Result<QueuedWorkBatch, StoreError> {
    enqueue_queued_work_with_outcome_tx(tx, batch)
        .await
        .map(QueuedWorkEnqueueOutcome::into_batch)
}

async fn enqueue_queued_work_with_outcome_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch: &QueuedWorkBatchDraft,
) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
    batch
        .validate_process_wake_source()
        .map_err(StoreError::Backend)?;
    let allocation_floor = if let Some(wake_source) = batch.process_wake_source.as_ref() {
        if let Some(source_key) = batch.source_key.as_deref() {
            lock_process_wake_source_tx(tx, &batch.session_id, source_key).await?;
        }
        sqlx::query_scalar::<_, i64>(
            "SELECT allocation_floor FROM lash_wake_redelivery_fences
             WHERE session_id = $1 AND process_id = $2",
        )
        .bind(&batch.session_id)
        .bind(&wake_source.process_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
    } else {
        None
    };
    if let Some(source_key) = batch.source_key.as_deref() {
        let existing_id: Option<String> = sqlx::query_scalar(
            "SELECT batch_id FROM lash_queued_work_batches
             WHERE session_id = $1 AND source_key = $2",
        )
        .bind(&batch.session_id)
        .bind(source_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(batch_id) = existing_id {
            let existing = load_queued_batch(tx, &batch_id).await?.ok_or_else(|| {
                StoreError::Backend("queued work source row disappeared".to_string())
            })?;
            return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
        }
    }
    if let (Some(wake_source), Some(allocation_floor)) =
        (batch.process_wake_source.as_ref(), allocation_floor)
        && wake_source.sequence <= allocation_floor as u64
    {
        return Err(StoreError::ProcessWakeSequenceRewound {
            session_id: batch.session_id.clone(),
            process_id: wake_source.process_id.clone(),
            sequence: wake_source.sequence,
            allocation_floor: allocation_floor as u64,
        });
    }
    let now = current_epoch_ms();
    let enqueue_seq: i64 = sqlx::query_scalar(
        "SELECT nextval(pg_get_serial_sequence(
            'lash_queued_work_batches',
            'enqueue_seq'
         ))",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(enqueue_seq as u64),
    );
    sqlx::query(
        "INSERT INTO lash_queued_work_batches (
            enqueue_seq, batch_id, session_id, source_key, delivery_policy, slot_policy,
            merge_key_json, available_at_ms, enqueued_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(enqueue_seq)
    .bind(&batch_id)
    .bind(&batch.session_id)
    .bind(&batch.source_key)
    .bind(batch.delivery_policy.as_str())
    .bind(batch.slot_policy.as_str())
    .bind(encode_json(&batch.merge_key))
    .bind(batch.available_at_ms as i64)
    .bind(now as i64)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for (index, payload) in batch.payloads.iter().enumerate() {
        let item_id = format!("{batch_id}:item:{index}");
        sqlx::query(
            "INSERT INTO lash_queued_work_items (batch_id, item_index, item_id, payload_json)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&batch_id)
        .bind(index as i32)
        .bind(item_id)
        .bind(encode_json(payload))
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    let queued = load_queued_batch(tx, &batch_id)
        .await?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    debug_assert_eq!(queued.enqueue_seq, enqueue_seq as u64);
    Ok(QueuedWorkEnqueueOutcome::Inserted(queued))
}

/// Serialize queue insertion and queue consumption for one process-wake source
/// across their otherwise separate live-row and allocation-fence relations.
///
/// The 64-bit hash may collide, which only adds harmless serialization; it
/// cannot permit two equal `(session_id, source_key)` pairs to use different
/// locks.
async fn lock_process_wake_source_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    source_key: &str,
) -> Result<(), StoreError> {
    // `PostgresStorage::from_pool` accepts externally configured pools, so
    // bound this correctness lock locally even when no connection-wide
    // `lock_timeout` was installed. SQLSTATE 55P03 maps to `Contended`.
    sqlx::query(
        "SELECT set_config(
             'lock_timeout',
             CASE
                 WHEN current_setting('lock_timeout') = '0'
                   OR current_setting('lock_timeout')::interval > INTERVAL '10 seconds'
                 THEN '10s'
                 ELSE current_setting('lock_timeout')
             END,
             TRUE
         )",
    )
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length($1)::TEXT || ':' || $1 || length($2)::TEXT || ':' || $2,
                 0
             )
         )",
    )
    .bind(session_id)
    .bind(source_key)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

#[async_trait::async_trait]
impl SessionCommitStore for PostgresSessionStore {
    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        let Some(session_id) = self.selected_session_id().await? else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let Some(meta) = load_session_head_meta_tx(&mut tx, &session_id, false).await? else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let leaf_node_id = meta.leaf_node_id.clone();
        let graph = load_graph_tx(&mut tx, &session_id, leaf_node_id.clone(), true).await?;
        let checkpoint = match meta.checkpoint_ref.as_ref() {
            Some(blob_ref) => get_checkpoint_tx(&mut tx, blob_ref).await?,
            None => None,
        };
        let token_ledger =
            merge_token_ledger_entries(load_usage_deltas_tx(&mut tx, &session_id).await?);
        let read = PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint,
            token_ledger,
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(read))
    }

    async fn load_node(&self, node_id: &str) -> Result<Option<SessionNodeRecord>, StoreError> {
        let Some(session_id) = self.selected_session_id().await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            "WITH RECURSIVE ancestry(node_id, parent_node_id) AS (
                 SELECT node.node_id, node.parent_node_id
                 FROM lash_graph_nodes node
                 JOIN lash_sessions head ON head.leaf_node_id = node.node_id
                 WHERE head.session_id = $2 AND node.tombstoned = FALSE
                 UNION ALL
                 SELECT parent.node_id, parent.parent_node_id
                 FROM lash_graph_nodes parent
                 JOIN ancestry child ON parent.node_id = child.parent_node_id
                 WHERE parent.tombstoned = FALSE
             )
             SELECT node_id, parent_node_id, node_json FROM lash_graph_nodes
             WHERE node_id = $1 AND tombstoned = FALSE
               AND (
                   session_id = $2
                   OR EXISTS (
                       SELECT 1 FROM ancestry
                       WHERE ancestry.node_id = lash_graph_nodes.node_id
                   )
               )",
        )
        .bind(node_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        row.map(|row| {
            let node_id = row.get(0);
            let parent_node_id = row.get(1);
            let json: String = row.get(2);
            SessionNodeRecord::decode_storage_body(node_id, parent_node_id, &json)
                .map_err(|err| StoreError::Backend(format!("failed to decode graph node: {err}")))
        })
        .transpose()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        commit.validate_budget()?;
        commit.validate_operation_session()?;
        let turn_commit_hash = commit.turn_commit_hash()?;
        self.bind_session_id(&commit.session_id)?;
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| lash_core::store::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect::<Vec<_>>();
        let now = self.clock.timestamp_ms();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // A head row does not exist during the first commit, so row locking
        // alone cannot serialize create-versus-delete. This session-keyed lock
        // is the common authority for every history commit and deletion.
        ensure_session_not_deleted_tx(&mut tx, &commit.session_id).await?;
        // Read without a lock for early validation and receipt replay. Before
        // mutating graph reachability, existing sessions lock and recheck this
        // revision so commit, maintenance, and deletion share one authority.
        let existing = load_session_head_meta_tx(&mut tx, &commit.session_id, false).await?;
        if let Some(bound_session_id) = existing.as_ref().map(|meta| meta.session_id.as_str())
            && bound_session_id != commit.session_id
        {
            return Err(StoreError::SessionBindingMismatch {
                bound_session_id: bound_session_id.to_string(),
                attempted_session_id: commit.session_id,
            });
        }
        let direct_meta = SessionMeta {
            session_id: commit.session_id.clone(),
            session_name: commit.session_id.clone(),
            created_at: self.clock.timestamp_rfc3339(),
            model: commit.config.model.id.clone(),
            cwd: None,
            relation: lash_core::SessionRelation::Root,
        };
        sqlx::query(
            "INSERT INTO lash_session_meta (session_id, meta_json)
             VALUES ($1, $2)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(&commit.session_id)
        .bind(encode_json(&direct_meta))
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        commit.validate_node_derivation()?;
        {
            let completed = &commit.turn_commit;
            let operation_key = completed.operation.storage_key()?;
            let prior = sqlx::query(
                "SELECT turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM lash_runtime_turn_commits
                 WHERE session_id = $1 AND turn_id = $2",
            )
            .bind(&commit.session_id)
            .bind(&operation_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if let Some(row) = prior {
                let hash: String = row.get(0);
                let result_json: String = row.get(1);
                let stored_identity: Option<String> = row.get(2);
                let stored_version: Option<i32> = row.get(3);
                let stored_requested_node_count: Option<i64> = row.get(4);
                let stored_count = stored_requested_node_count
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Backend(
                            "stored append requested-node count is negative".to_string(),
                        )
                    })?;
                let attempted_count = completed
                    .requested_node_count
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| {
                        StoreError::Backend(
                            "attempted append requested-node count does not fit u64".to_string(),
                        )
                    })?;
                match lash_core::store::decide_runtime_commit_receipt(
                    &hash,
                    &turn_commit_hash,
                    stored_version.and_then(|version| u32::try_from(version).ok()),
                    completed.identity_encoding_version,
                    stored_identity.as_deref(),
                    completed.request_identity_hash.as_deref(),
                    stored_count,
                    attempted_count,
                ) {
                    lash_core::store::RuntimeCommitReceiptDecision::Replay => {
                        let mut result: RuntimeCommitResult =
                            store_decode_json(&result_json, "runtime turn commit result")?;
                        result.receipt_replayed = true;
                        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                            release_session_execution_lease_tx(&mut tx, completion).await?;
                        }
                        tx.commit().await.map_err(store_sqlx_error)?;
                        return Ok(result);
                    }
                    lash_core::store::RuntimeCommitReceiptDecision::AppendIdentityConflict => {
                        return Err(StoreError::AppendOperationIdentityConflict {
                            session_id: commit.session_id.clone(),
                            operation_key,
                        });
                    }
                    lash_core::store::RuntimeCommitReceiptDecision::RuntimeCommitConflict => {
                        return Err(StoreError::RuntimeTurnCommitConflict {
                            session_id: commit.session_id.clone(),
                            turn_id: operation_key,
                        });
                    }
                    lash_core::store::RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
                        stored,
                        attempted,
                    } => {
                        return Err(StoreError::AppendReceiptRequestedNodeCountCorrupt {
                            session_id: commit.session_id.clone(),
                            operation_key,
                            stored,
                            attempted,
                        });
                    }
                }
            }
        }
        if commit.turn_commit.request_identity_hash.is_some()
            && let Some(required) = commit.turn_commit.requested_ancestor_node_id.as_deref()
        {
            let active_graph = load_graph_tx(
                &mut tx,
                &commit.session_id,
                existing.as_ref().and_then(|meta| meta.leaf_node_id.clone()),
                true,
            )
            .await?;
            if !active_graph.active_path_contains(required) {
                return Err(StoreError::AppendAncestorNotActive {
                    required_node_id: required.to_string(),
                });
            }
        }
        let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
        let expected_revision = commit.expected_head_revision;
        if expected_revision != actual_revision {
            return Err(StoreError::HeadRevisionConflict {
                expected: commit.expected_head_revision,
                actual: actual_revision,
            });
        }
        if existing.is_some() {
            let locked_revision = sqlx::query_scalar::<_, i64>(
                "SELECT head_revision
                 FROM lash_sessions
                 WHERE session_id = $1
                 FOR UPDATE",
            )
            .bind(&commit.session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if locked_revision != Some(actual_revision as i64) {
                return Err(StoreError::HeadRevisionConflict {
                    expected: commit.expected_head_revision,
                    actual: locked_revision.map_or(0, |revision| revision as u64),
                });
            }
        }
        commit.validate_append_node_ids_unique()?;
        commit.graph.validate_append_topology()?;
        let node_ids = commit
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>();
        let occupied = sqlx::query_scalar::<_, String>(
            "SELECT node_id
             FROM lash_graph_nodes
             WHERE node_id = ANY($1)",
        )
        .bind(&node_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        if let Some(node) = commit
            .graph
            .nodes
            .iter()
            .find(|node| occupied.contains(&node.node_id))
        {
            return Err(StoreError::NodeIdCollision {
                node_id: node.node_id.clone(),
            });
        }
        if let Some(leaf_node_id) = commit.graph.leaf_node_id() {
            let appended = matches!(
                &commit.graph,
                GraphAppend { nodes, .. }
                    if nodes.iter().any(|node| &node.node_id == leaf_node_id)
            );
            let live = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE node_id = $1 AND tombstoned = FALSE
                )",
            )
            .bind(leaf_node_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if !appended && !live {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: Some(leaf_node_id.clone()),
                });
            }
        } else {
            let appends_nodes = matches!(
                &commit.graph,
                GraphAppend { nodes, .. } if !nodes.is_empty()
            );
            let has_live_nodes = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE session_id = $1 AND tombstoned = FALSE
                )",
            )
            .bind(&commit.session_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if appends_nodes || has_live_nodes {
                return Err(StoreError::InvalidGraphLeaf { leaf_node_id: None });
            }
        }
        for completed in &commit.completed_queue_claims {
            if completed.session_id != commit.session_id {
                return Err(StoreError::QueuedWorkClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                });
            }
            ensure_queued_work_completion_tx(&mut tx, completed).await?;
        }
        for completed in &commit.completed_turn_input_claims {
            if completed.session_id != commit.session_id {
                return Err(StoreError::TurnInputClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                });
            }
            ensure_turn_input_completion_tx(&mut tx, completed).await?;
        }
        let (checkpoint_ref, manifest) = put_checkpoint_tx(&mut tx, &commit.checkpoint).await?;
        for entry in &commit.usage_deltas {
            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal).map_err(|_| {
                StoreError::Backend(
                    "usage delta ordinal does not fit PostgreSQL BIGINT".to_string(),
                )
            })?;
            sqlx::query(
                "INSERT INTO lash_usage_deltas (
                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, entry_json
                 ) VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash)
                 DO NOTHING",
            )
            .bind(&commit.session_id)
            .bind(&entry.identity.operation_storage_key)
            .bind(entry_ordinal)
            .bind(i32::try_from(entry.identity.payload_encoding_version).map_err(|_| {
                StoreError::Backend(
                    "usage payload encoding version does not fit PostgreSQL INTEGER".to_string(),
                )
            })?)
            .bind(&entry.identity.payload_hash)
            .bind(encode_json(&entry.entry))
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let old_leaf_node_id = existing.as_ref().and_then(|head| head.leaf_node_id.clone());
        match commit.graph.nodes.first() {
            None if commit.graph.leaf_node_id != old_leaf_node_id => {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: commit.graph.leaf_node_id.clone(),
                });
            }
            Some(first) if first.parent_node_id.as_ref() != old_leaf_node_id.as_ref() => {
                return Err(StoreError::InvalidGraphParent {
                    node_id: first.node_id.clone(),
                    expected: old_leaf_node_id.clone(),
                    actual: first.parent_node_id.clone(),
                });
            }
            _ => {}
        }
        if let Some(old_leaf_node_id) = &old_leaf_node_id {
            let live = sqlx::query_scalar::<_, bool>(
                "SELECT TRUE FROM lash_graph_nodes
                 WHERE node_id = $1 AND tombstoned = FALSE
                 FOR UPDATE",
            )
            .bind(old_leaf_node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if live.is_none() {
                return Err(StoreError::InvalidGraphLeaf {
                    leaf_node_id: Some(old_leaf_node_id.clone()),
                });
            }
        }
        let leaf_node_id = {
            for node in &commit.graph.nodes {
                let node_json = node.encode_storage_body().map_err(|err| {
                    StoreError::Backend(format!("failed to encode graph node body: {err}"))
                })?;
                sqlx::query(
                    "INSERT INTO lash_graph_nodes
                         (session_id, node_id, parent_node_id, node_json)
                         VALUES ($1, $2, $3, $4)",
                )
                .bind(&commit.session_id)
                .bind(&node.node_id)
                .bind(&node.parent_node_id)
                .bind(node_json)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            commit.graph.leaf_node_id.clone()
        };
        let head_changed = old_leaf_node_id != leaf_node_id;
        let derived_frame_node_id = match leaf_node_id.as_deref() {
            Some(leaf_node_id) => Some(
                nearest_frame_node_id_tx(&mut tx, leaf_node_id)
                    .await?
                    .ok_or_else(|| StoreError::MissingFrameOpenAncestor {
                        leaf_node_id: leaf_node_id.to_string(),
                    })?,
            ),
            None => None,
        };
        if commit.current_frame_node_id != derived_frame_node_id {
            return Err(StoreError::Backend(format!(
                "current_frame_node_id {:?} does not match nearest FrameOpen ancestor {:?}",
                commit.current_frame_node_id, derived_frame_node_id
            )));
        }
        let next_revision = actual_revision + 1;
        let meta = SessionHeadMeta::assemble(
            SessionHeadPayload {
                schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                session_id: commit.session_id.clone(),
                config: commit.config.clone(),
                current_frame_node_id: derived_frame_node_id,
            },
            next_revision,
            Some(checkpoint_ref.clone()),
            leaf_node_id,
        );
        // Conditional publication is still required for concurrent first
        // commits, where no head row existed to lock. Existing sessions already
        // hold the row lock above; the revision predicate is defense in depth.
        let head_write = sqlx::query(
            "INSERT INTO lash_sessions
             (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (session_id) DO UPDATE SET
                head_revision = EXCLUDED.head_revision,
                head_json = EXCLUDED.head_json,
                checkpoint_ref = EXCLUDED.checkpoint_ref,
                leaf_node_id = EXCLUDED.leaf_node_id
             WHERE lash_sessions.head_revision = $6",
        )
        .bind(&commit.session_id)
        .bind(next_revision as i64)
        .bind(encode_json(&meta.payload()))
        .bind(checkpoint_ref.as_str())
        .bind(meta.leaf_node_id.as_deref())
        .bind(actual_revision as i64)
        .execute(&mut *tx)
        .await;
        let head_write = match head_write {
            Ok(result) => result,
            Err(err) if is_contention_error(&err) => {
                // PostgreSQL aborted this transaction before the head write
                // published. This is not evidence that the head advanced (the
                // rows_affected == 0 branch below is); the unchanged commit is
                // therefore the only semantically valid retry.
                return Err(StoreError::Contended);
            }
            Err(err) => return Err(store_sqlx_error(err)),
        };
        if head_write.rows_affected() == 0 {
            // A concurrent commit won the race: the head no longer matches the
            // revision we read. Re-read the now-current revision for an accurate
            // report, then drop `tx` (auto-rollback), discarding this attempt's
            // node/usage writes; the caller reloads and retries.
            let actual_now = sqlx::query_scalar::<_, i64>(
                "SELECT head_revision FROM lash_sessions WHERE session_id = $1",
            )
            .bind(&commit.session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .map_or(actual_revision, |revision| revision as u64);
            return Err(StoreError::HeadRevisionConflict {
                expected: commit.expected_head_revision,
                actual: actual_now,
            });
        }
        if head_changed && let Some(old_leaf_node_id) = &old_leaf_node_id {
            retire_unreachable_ancestry_tx(&mut tx, old_leaf_node_id).await?;
        }
        complete_queued_work_claims_tx(&mut tx, &commit.completed_queue_claims).await?;
        complete_turn_input_claims_tx(&mut tx, &commit.completed_turn_input_claims).await?;
        if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
            let rows = sqlx::query(
                "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                        state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                        claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND state = $2
                 ORDER BY enqueue_seq ASC
                 FOR UPDATE",
            )
            .bind(&commit.session_id)
            .bind(lash_core::TurnInputState::PendingActive.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let mut input_ids = Vec::new();
            for row in rows {
                let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
                if input
                    .ingress
                    .active_turn_id()
                    .is_some_and(|active| active == turn_id)
                {
                    input_ids.push(input.input_id);
                }
            }
            for input_id in input_ids {
                sqlx::query(
                    "UPDATE lash_pending_turn_inputs
                     SET state = $3,
                         ingress_json = $4,
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_owner_liveness_json = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = $1 AND input_id = $2",
                )
                .bind(&commit.session_id)
                .bind(input_id)
                .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
                .bind(encode_json(&lash_core::TurnInputIngress::NextTurn))
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            }
        }
        commit_attachment_refs_tx(
            &mut tx,
            &commit.session_id,
            &commit.committed_attachment_ids,
        )
        .await?;
        if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
            sqlx::query(
                "UPDATE lash_attachment_manifest
                     SET committed_at_ms = COALESCE(committed_at_ms, $1)
                     WHERE session_id = $2
                       AND owner_kind = 'turn'
                       AND owner_id = $3
                       AND committed_at_ms IS NULL",
            )
            .bind(now as i64)
            .bind(&commit.session_id)
            .bind(turn_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let mut enqueued_queue_batches = Vec::new();
        for batch in &commit.enqueued_queue_batches {
            if batch.session_id != commit.session_id {
                return Err(StoreError::SessionBindingMismatch {
                    bound_session_id: commit.session_id.clone(),
                    attempted_session_id: batch.session_id.clone(),
                });
            }
            enqueued_queue_batches.push(enqueue_queued_work_tx(&mut tx, batch).await?);
        }
        let result = RuntimeCommitResult {
            head_revision: next_revision,
            checkpoint_ref,
            manifest,
            committed_leaf_node_id: commit.graph.leaf_node_id.clone(),
            realized_node_timestamps,
            committed_usage_delta_identities: commit
                .usage_deltas
                .iter()
                .map(|delta| delta.identity.clone())
                .collect(),
            enqueued_queue_batches,
            turn_input_applications: commit.turn_input_applications(),
            receipt_replayed: false,
        };
        {
            let completed = &commit.turn_commit;
            let operation_key = completed.operation.storage_key()?;
            sqlx::query(
                "INSERT INTO lash_runtime_turn_commits (
                    session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                    request_identity_hash, requested_node_count,
                    requested_ancestor_node_id, identity_encoding_version
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(&commit.session_id)
            .bind(operation_key)
            .bind(&turn_commit_hash)
            .bind(encode_json(&result))
            .bind(now as i64)
            .bind(completed.request_identity_hash.as_deref())
            .bind(completed.requested_node_count.map(|count| count as i64))
            .bind(completed.requested_ancestor_node_id.as_deref())
            .bind(
                completed
                    .identity_encoding_version
                    .and_then(|version| i32::try_from(version).ok()),
            )
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            release_session_execution_lease_tx(&mut tx, completion).await?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        binding.validate()?;
        let session_id = &binding.session_id;
        self.bind_session_id(session_id)?;
        let meta = SessionMeta {
            session_id: session_id.to_string(),
            session_name: session_id.to_string(),
            created_at: self.clock.timestamp_rfc3339(),
            model: binding.model_id.clone(),
            cwd: binding.cwd.clone(),
            relation: binding.relation.clone(),
        };
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let inserted = sqlx::query(
            "INSERT INTO lash_session_meta (session_id, meta_json)
             VALUES ($1, $2)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id)
        .bind(encode_json(&meta))
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted.rows_affected() == 1 {
            lash_core::SessionAdmission::Created
        } else {
            lash_core::SessionAdmission::Rebound
        })
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.bind_session_id(&meta.session_id)?;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &meta.session_id).await?;
        sqlx::query(
            "INSERT INTO lash_session_meta (session_id, meta_json)
             VALUES ($1, $2)
             ON CONFLICT (session_id) DO UPDATE SET meta_json = EXCLUDED.meta_json",
        )
        .bind(&meta.session_id)
        .bind(encode_json(&meta))
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        let json: Option<String> = if let Some(session_id) = &self.session_id {
            sqlx::query_scalar("SELECT meta_json FROM lash_session_meta WHERE session_id = $1")
                .bind(session_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_sqlx_error)?
        } else {
            sqlx::query_scalar(
                "SELECT meta_json FROM lash_session_meta ORDER BY session_id ASC LIMIT 1",
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)?
        };
        json.map(|json| store_decode_json(&json, "session meta"))
            .transpose()
    }
}

async fn complete_queued_work_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[QueuedWorkCompletion],
) -> Result<(), StoreError> {
    for completed in completed_claims {
        for batch_id in &completed.batch_ids {
            let source_key: Option<String> = sqlx::query_scalar(
                "SELECT source_key
                 FROM lash_queued_work_batches
                 WHERE session_id = $1
                   AND batch_id = $2
                   AND claim_id = $3
                   AND claim_token = $4",
            )
            .bind(&completed.session_id)
            .bind(batch_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .flatten();
            let payload_json: Option<String> = sqlx::query_scalar(
                "SELECT item.payload_json
                 FROM lash_queued_work_batches AS batch
                 JOIN lash_queued_work_items AS item ON item.batch_id = batch.batch_id
                 WHERE batch.session_id = $1
                   AND batch.batch_id = $2
                   AND batch.claim_id = $3
                   AND batch.claim_token = $4
                 ORDER BY item.item_index ASC
                 LIMIT 1",
            )
            .bind(&completed.session_id)
            .bind(batch_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            let wake_source = payload_json
                .as_deref()
                .map(|json| {
                    store_decode_json::<lash_core::runtime::QueuedWorkPayload>(
                        json,
                        "queued work payload",
                    )
                })
                .transpose()?
                .and_then(|payload| match payload {
                    lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } => {
                        Some((wake.process_id, wake.sequence))
                    }
                    _ => None,
                });
            if let (Some(source_key), Some(_)) = (source_key.as_deref(), wake_source.as_ref()) {
                lock_process_wake_source_tx(tx, &completed.session_id, source_key).await?;
            }
            if let Some((process_id, sequence)) = wake_source {
                sqlx::query(
                    "INSERT INTO lash_wake_redelivery_fences (
                        session_id, process_id, allocation_floor
                     ) VALUES ($1, $2, $3)
                     ON CONFLICT (session_id, process_id) DO UPDATE
                     SET allocation_floor = GREATEST(
                         lash_wake_redelivery_fences.allocation_floor,
                         EXCLUDED.allocation_floor
                     )",
                )
                .bind(&completed.session_id)
                .bind(process_id)
                .bind(sequence as i64)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            let completion = sqlx::query(
                "DELETE FROM lash_queued_work_batches
                 WHERE session_id = $1 AND batch_id = $2 AND claim_id = $3 AND claim_token = $4",
            )
            .bind(&completed.session_id)
            .bind(batch_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            if completion.rows_affected() != 1 {
                return Err(StoreError::QueuedWorkClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                });
            }
        }
    }
    Ok(())
}

pub(crate) async fn complete_turn_input_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[lash_core::TurnInputCompletion],
) -> Result<(), StoreError> {
    for completed in completed_claims {
        for input_id in &completed.input_ids {
            let completion = sqlx::query(
                "UPDATE lash_pending_turn_inputs
                 SET state = $5,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_owner_liveness_json = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = $1 AND input_id = $2 AND claim_id = $3 AND claim_token = $4",
            )
            .bind(&completed.session_id)
            .bind(input_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .bind(lash_core::TurnInputState::Completed.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            if completion.rows_affected() != 1 {
                return Err(StoreError::TurnInputClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                });
            }
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for PostgresSessionStore {
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        lock_session_execution_lease_tx(&mut tx, session_id).await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let current = load_session_execution_lease_tx(&mut tx, session_id).await?;
        if current
            .as_ref()
            .is_some_and(|lease| lease.lease_token.is_some() && lease.expires_at_ms > now)
        {
            let current = current.expect("checked current lease is present");
            if current
                .owner
                .as_ref()
                .is_some_and(|current_owner| current_owner.same_incarnation(owner))
            {
                let expires_at = now.saturating_add(lease_ttl_ms);
                sqlx::query(
                    "UPDATE lash_session_execution_leases
                     SET lease_expires_at_ms = $2
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .bind(expires_at as i64)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                    SessionExecutionLease {
                        session_id: session_id.to_string(),
                        owner: owner.clone(),
                        lease_token: current.lease_token.expect("live lease token set"),
                        fencing_token: current.fencing_token,
                        claimed_at_epoch_ms: current.claimed_at_ms,
                        expires_at_epoch_ms: expires_at,
                    },
                ));
            }
            let holder = row_to_session_execution_lease(session_id, current)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(SessionExecutionLeaseClaimOutcome::Busy { holder });
        }
        let previous_fencing_token = current.as_ref().map_or(0, |lease| lease.fencing_token);
        let lease = acquire_session_execution_lease_tx(
            &mut tx,
            session_id,
            owner,
            previous_fencing_token,
            now,
            lease_ttl_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(lease))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let current = load_session_execution_lease_tx(&mut tx, &fence.session_id).await?;
        let Some(current) = current else {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        };
        if !current
            .owner
            .as_ref()
            .is_some_and(|owner| owner.same_incarnation(&fence.owner))
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
            || current.fencing_token != fence.fencing_token
            || current.expires_at_ms <= now
        {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        let expires_at = now.saturating_add(lease_ttl_ms);
        sqlx::query(
            "UPDATE lash_session_execution_leases
             SET lease_expires_at_ms = $5
             WHERE session_id = $1
               AND lease_owner_id = $2
               AND lease_owner_incarnation_id = $3
               AND lease_token = $4
               AND lease_fencing_token = $6",
        )
        .bind(&fence.session_id)
        .bind(&fence.owner.owner_id)
        .bind(&fence.owner.incarnation_id)
        .bind(&fence.lease_token)
        .bind(expires_at as i64)
        .bind(fence.fencing_token as i64)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: fence.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_ms,
            expires_at_epoch_ms: expires_at,
        })
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseCompletion,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        release_session_execution_lease_tx(&mut tx, completion).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl QueuedWorkStore for PostgresSessionStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued = enqueue_queued_work_tx(&mut tx, &batch).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued = enqueue_queued_work_with_outcome_tx(&mut tx, &batch).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        // The fence is validated live, so its fencing token is the
        // currently-live session-lease generation; claims pin it and are
        // claimable only across a different generation (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(
            QueuedWorkClaimBoundary::Idle,
        ))
        .bind(session_id)
        .bind(generation as i64)
        .bind(claim_scan_limit(1))
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut selected = Vec::new();
        for row in rows {
            let row = queued_batch_row(row)?;
            if row.claim_token.is_none() || row.claim_session_lease_generation != generation {
                selected.push(row);
            }
        }
        let mut selected_batches = Vec::new();
        for row in &selected {
            selected_batches.push(queued_work_batch_from_row(&mut tx, row.clone()).await?);
        }
        let candidates = selected
            .iter()
            .zip(selected_batches.iter())
            .map(|(row, batch)| {
                Ok(ClaimCandidate {
                    enqueue_seq: row.enqueue_seq,
                    claim_fencing_token: row.claim_fencing_token,
                    work_class: batch.work_class().ok_or_else(|| {
                        StoreError::Backend(format!(
                            "queued-work batch `{}` has mixed or empty payload classes",
                            batch.batch_id
                        ))
                    })?,
                    delivery_policy: row.delivery_policy,
                    slot_policy: row.slot_policy,
                    merge_key: row.merge_key.clone(),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let selected_len = select_leading_session_command(&candidates);
        if selected_len == 0 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        selected.truncate(selected_len);
        selected_batches.truncate(selected_len);
        let lease =
            QueuedWorkClaimLease::derive(&candidates[0], session_id, owner, now, generation);
        let liveness_json: Option<&str> = None;
        for row in &selected {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3,
                     claim_owner_id = $4,
                     claim_owner_incarnation_id = $5,
                     claim_owner_liveness_json = $6,
                     claim_token = $7,
                     claim_fencing_token = claim_fencing_token + 1,
                     claim_session_lease_generation = $8
                 WHERE session_id = $1
                   AND batch_id = $2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> $8
                   )",
            )
            .bind(session_id)
            .bind(&row.batch_id)
            .bind(&lease.claim_id)
            .bind(&owner.owner_id)
            .bind(&owner.incarnation_id)
            .bind(liveness_json)
            .bind(&lease.lease_token)
            .bind(lease.session_lease_generation as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if changed == 0 {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(QueuedWorkClaim {
            session_id: session_id.to_string(),
            claim_id: lease.claim_id,
            owner: owner.clone(),
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
            batches: selected_batches,
        }))
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        max_batches: usize,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        if max_batches == 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(boundary))
            .bind(session_id)
            .bind(generation as i64)
            .bind(claim_scan_limit(max_batches))
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut selected = Vec::new();
        for row in rows {
            let row = queued_batch_row(row)?;
            if row.claim_token.is_none() || row.claim_session_lease_generation != generation {
                selected.push(row);
            }
        }
        let mut selected_batches = Vec::new();
        for row in &selected {
            selected_batches.push(queued_work_batch_from_row(&mut tx, row.clone()).await?);
        }
        let candidates = selected
            .iter()
            .zip(selected_batches.iter())
            .map(|(row, batch)| {
                Ok(ClaimCandidate {
                    enqueue_seq: row.enqueue_seq,
                    claim_fencing_token: row.claim_fencing_token,
                    work_class: batch.work_class().ok_or_else(|| {
                        StoreError::Backend(format!(
                            "queued-work batch `{}` has mixed or empty payload classes",
                            batch.batch_id
                        ))
                    })?,
                    delivery_policy: row.delivery_policy,
                    slot_policy: row.slot_policy,
                    merge_key: row.merge_key.clone(),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let selected_len = select_turn_work_claim_prefix(&candidates, boundary, max_batches);
        if selected_len == 0 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        selected.truncate(selected_len);
        selected_batches.truncate(selected_len);
        let lease =
            QueuedWorkClaimLease::derive(&candidates[0], session_id, owner, now, generation);
        let liveness_json: Option<&str> = None;
        for row in &selected {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3,
                     claim_owner_id = $4,
                     claim_owner_incarnation_id = $5,
                     claim_owner_liveness_json = $6,
                     claim_token = $7,
                     claim_fencing_token = claim_fencing_token + 1,
                     claim_session_lease_generation = $8
                 WHERE session_id = $1
                   AND batch_id = $2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> $8
                   )",
            )
            .bind(session_id)
            .bind(&row.batch_id)
            .bind(&lease.claim_id)
            .bind(&owner.owner_id)
            .bind(&owner.incarnation_id)
            .bind(liveness_json)
            .bind(&lease.lease_token)
            .bind(lease.session_lease_generation as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if changed == 0 {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(QueuedWorkClaim {
            session_id: session_id.to_string(),
            claim_id: lease.claim_id,
            owner: owner.clone(),
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
            batches: selected_batches,
        }))
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        max_batches: usize,
    ) -> Result<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        #[cfg(test)]
        self.checkpoint_probe_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !checkpoint_work_pending_postgres(
            &self.pool,
            session_id,
            session_execution_lease.fencing_token,
            turn_id,
            checkpoint,
            max_inputs,
            max_batches,
        )
        .await?
        {
            return Ok((None, None));
        }

        #[cfg(test)]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let input = claim_pending_turn_inputs_postgres_tx(
            &mut tx,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.to_string(),
                checkpoint,
            },
        )
        .await?;
        let input = match input {
            ClaimTransactionOutcome::Commit(input) => input,
            ClaimTransactionOutcome::Rollback(input) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok((input, None));
            }
        };
        let queued = claim_ready_queued_work_postgres_tx(
            &mut tx,
            session_id,
            session_execution_lease,
            owner,
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            max_batches,
        )
        .await?;
        match queued {
            ClaimTransactionOutcome::Commit(queued) => {
                tx.commit().await.map_err(store_sqlx_error)?;
                Ok((input, queued))
            }
            ClaimTransactionOutcome::Rollback(queued) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                Ok((None, queued))
            }
        }
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        if batch_ids.is_empty() {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let mut selected = Vec::new();
        let mut selected_batches = Vec::new();
        for batch_id in batch_ids {
            let row = sqlx::query(
                "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                        slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                        claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation
                 FROM lash_queued_work_batches
                 WHERE session_id = $1 AND batch_id = $2 AND available_at_ms <= $3
                   AND (claim_token IS NULL OR claim_session_lease_generation <> $4)
                 FOR UPDATE",
            )
            .bind(session_id)
            .bind(batch_id)
            .bind(now as i64)
            .bind(generation as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let Some(row) = row else {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(None);
            };
            let row = queued_batch_row(row)?;
            let batch = queued_work_batch_from_row(&mut tx, row.clone()).await?;
            if batch.work_class() != Some(lash_core::runtime::QueuedWorkClass::TurnWork) {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
            selected.push(row);
            selected_batches.push(batch);
        }
        let candidates = selected
            .iter()
            .map(|row| ClaimCandidate {
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                work_class: lash_core::runtime::QueuedWorkClass::TurnWork,
                delivery_policy: row.delivery_policy,
                slot_policy: row.slot_policy,
                merge_key: row.merge_key.clone(),
            })
            .collect::<Vec<_>>();
        if select_turn_work_claim_prefix(&candidates, boundary, candidates.len())
            != candidates.len()
        {
            tx.rollback().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        let lease =
            QueuedWorkClaimLease::derive(&candidates[0], session_id, owner, now, generation);
        let liveness_json: Option<&str> = None;
        for row in &selected {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3, claim_owner_id = $4,
                     claim_owner_incarnation_id = $5, claim_owner_liveness_json = $6,
                     claim_token = $7, claim_fencing_token = claim_fencing_token + 1,
                     claim_session_lease_generation = $8
                 WHERE session_id = $1 AND batch_id = $2
                   AND (claim_token IS NULL OR claim_session_lease_generation <> $8)",
            )
            .bind(session_id)
            .bind(&row.batch_id)
            .bind(&lease.claim_id)
            .bind(&owner.owner_id)
            .bind(&owner.incarnation_id)
            .bind(liveness_json)
            .bind(&lease.lease_token)
            .bind(generation as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if changed != 1 {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(QueuedWorkClaim {
            session_id: session_id.to_string(),
            claim_id: lease.claim_id,
            owner: owner.clone(),
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
            batches: selected_batches,
        }))
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE lash_queued_work_batches
             SET claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE session_id = $1 AND claim_id = $2 AND claim_token = $3",
        )
        .bind(&claim.session_id)
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .execute(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "UPDATE lash_queued_work_batches
             SET claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN ",
        );
        query.push_tuples(claims, |mut row, claim| {
            row.push_bind(&claim.session_id)
                .push_bind(&claim.claim_id)
                .push_bind(&claim.lease_token);
        });
        query
            .build()
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let row = sqlx::query(
            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM lash_queued_work_batches
             WHERE session_id = $1
               AND batch_id = $2
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM lash_session_execution_leases sel
                    WHERE sel.session_id = $1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > $3
                      AND sel.lease_fencing_token
                          = lash_queued_work_batches.claim_session_lease_generation
               ))
             FOR UPDATE",
        )
        .bind(session_id)
        .bind(batch_id)
        .bind(now as i64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let batch = queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?;
        sqlx::query("DELETE FROM lash_queued_work_batches WHERE batch_id = $1")
            .bind(batch_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(batch))
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let rows = sqlx::query(
            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM lash_queued_work_batches
             WHERE session_id = $1
             ORDER BY enqueue_seq ASC",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(
            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM lash_queued_work_batches
             WHERE session_id = $1
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM lash_session_execution_leases sel
                    WHERE sel.session_id = $1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > $2
                      AND sel.lease_fencing_token
                          = lash_queued_work_batches.claim_session_lease_generation
               ))
             ORDER BY enqueue_seq ASC",
        )
        .bind(session_id)
        .bind(now as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }
}

#[async_trait::async_trait]
impl TurnInputStore for PostgresSessionStore {
    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &draft.session_id).await?;
        let now = current_epoch_ms();
        let input_id = draft.input_id.clone().unwrap_or_else(|| {
            derive_pending_turn_input_id(&draft.session_id, draft.source_key.as_deref(), now)
        });
        let state = match draft.ingress {
            lash_core::TurnInputIngress::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputIngress::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        let ingress_json = encode_json(&draft.ingress);
        let input_json = encode_json(&draft.input);
        let input = if let Some(source_key) = draft.source_key.as_deref() {
            let row = sqlx::query(
                "INSERT INTO lash_pending_turn_inputs (
                    input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (session_id, source_key) DO UPDATE
                 SET source_key = lash_pending_turn_inputs.source_key
                 RETURNING enqueue_seq, input_id, session_id, source_key, ingress_json,
                           state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                           claim_owner_id, claim_owner_incarnation_id,
                           claim_owner_liveness_json, claim_token, claim_session_lease_generation",
            )
            .bind(&input_id)
            .bind(&draft.session_id)
            .bind(source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
            if !draft.submitted_content_matches(&input).map_err(|err| {
                StoreError::Backend(format!(
                    "failed to compare pending turn input submission: {err}"
                ))
            })? {
                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                    session_id: draft.session_id.clone(),
                    source_key: source_key.to_string(),
                    existing_input_id: input.input_id.clone(),
                });
            }
            input
        } else {
            sqlx::query(
                "INSERT INTO lash_pending_turn_inputs (
                    input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&input_id)
            .bind(&draft.session_id)
            .bind(&draft.source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            load_pending_turn_input(&mut tx, &draft.session_id, &input_id)
                .await?
                .ok_or_else(|| {
                    StoreError::Backend("pending turn input insert disappeared".to_string())
                })?
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(input)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM lash_pending_turn_inputs
             WHERE session_id = $1
               AND state IN ($2, $3)
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM lash_session_execution_leases sel
                    WHERE sel.session_id = $1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > $4
                      AND sel.lease_fencing_token
                          = lash_pending_turn_inputs.claim_session_lease_generation
               ))
             ORDER BY enqueue_seq ASC",
        )
        .bind(session_id)
        .bind(lash_core::TurnInputState::PendingActive.as_str())
        .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
        .bind(now as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let inputs = rows
            .into_iter()
            .map(pending_turn_input_row)
            .map(|row| row.and_then(pending_turn_input_from_row))
            .collect::<Result<Vec<_>, StoreError>>()?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<lash_core::TurnInputApplication>, StoreError> {
        let rows = sqlx::query(
            "SELECT turn_id, result_json
             FROM lash_runtime_turn_commits
             WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        let mut commits = Vec::with_capacity(rows.len());
        for row in rows {
            let turn_id: String = row.get(0);
            let result_json: String = row.get(1);
            let result: RuntimeCommitResult =
                store_decode_json(&result_json, "runtime turn commit result")?;
            commits.push((
                result.head_revision,
                turn_id,
                result.turn_input_applications,
            ));
        }
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelResult>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let targets = targets.to_vec();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome =
                match load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &target, true)
                    .await?
                {
                    Some(row) => cancel_pending_turn_input_row_tx(&mut tx, row, now).await?,
                    None => lash_core::PendingTurnInputCancelOutcome::NotFound,
                };
            results.push(lash_core::PendingTurnInputCancelResult { target, outcome });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let anchor = anchor.clone();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let Some(anchor_row) =
            load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &anchor, true).await?
        else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { anchor });
        };
        let rows = sqlx::query(
            "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                    state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                    claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
             FROM lash_pending_turn_inputs
             WHERE session_id = $1 AND enqueue_seq >= $2
             ORDER BY enqueue_seq ASC
             FOR UPDATE",
        )
        .bind(session_id)
        .bind(anchor_row.enqueue_seq as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(
                cancel_pending_turn_input_row_tx(&mut tx, pending_turn_input_row(row)?, now)
                    .await?,
            );
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes { anchor, outcomes })
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        turn_id: &str,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_postgres(
            &self.pool,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.to_string(),
                checkpoint,
            },
        )
        .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseFence,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_postgres(
            &self.pool,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::NextTurn,
        )
        .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::TurnInputClaim,
    ) -> Result<(), StoreError> {
        let restored_state = match claim.mode {
            lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = CASE
                     WHEN state = $4 THEN $5
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE session_id = $1 AND claim_id = $2 AND claim_token = $3",
        )
        .bind(&claim.session_id)
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .bind(lash_core::TurnInputState::Accepted.as_str())
        .bind(restored_state.as_str())
        .execute(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[lash_core::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
            "UPDATE lash_pending_turn_inputs
             SET state = CASE
                     WHEN state = 'accepted' THEN 'pending_active'
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN ",
        );
        query.push_tuples(claims, |mut row, claim| {
            row.push_bind(&claim.session_id)
                .push_bind(&claim.claim_id)
                .push_bind(&claim.lease_token);
        });
        query
            .build()
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl StoreMaintenance for PostgresSessionStore {
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        sqlx::query(
            "INSERT INTO lash_lashlang_artifacts (namespace, artifact_ref, artifact_bytes)
             VALUES ($1, $2, $3)
             ON CONFLICT (namespace, artifact_ref)
             DO UPDATE SET artifact_bytes = EXCLUDED.artifact_bytes",
        )
        .bind(crate::artifact_store::CURRENT_TRIGGER_MANIFEST_NAMESPACE)
        .bind(lash_core::TriggerOwnerScope::session(session_id).namespace())
        .bind([1_u8].as_slice())
        .execute(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        Ok(true)
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        sqlx::query_as(
            "SELECT namespace, artifact_ref
             FROM lash_lashlang_artifacts
             WHERE namespace = $1 AND artifact_ref = $2
             ORDER BY namespace, artifact_ref",
        )
        .bind(crate::artifact_store::CURRENT_TRIGGER_MANIFEST_NAMESPACE)
        .bind(lash_core::TriggerOwnerScope::session(session_id).namespace())
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)
    }

    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        // `lash_deleted_sessions` is deliberately exempt: it is permanent
        // identity evidence and must survive every retention-pruning pass.
        let removed_node_count = if let Some(session_id) = &self.session_id {
            sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1 AND tombstoned = TRUE")
                .bind(session_id)
                .execute(&self.pool)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected()
        } else {
            sqlx::query("DELETE FROM lash_graph_nodes WHERE tombstoned = TRUE")
                .execute(&self.pool)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected()
        };
        let removed_pending_turn_input_tombstone_count = if let Some(session_id) = &self.session_id
        {
            sqlx::query(
                "DELETE FROM lash_pending_turn_inputs
                 WHERE session_id = $1 AND state IN ($2, $3)",
            )
            .bind(session_id)
            .bind(lash_core::TurnInputState::Cancelled.as_str())
            .bind(lash_core::TurnInputState::Completed.as_str())
            .execute(&self.pool)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected()
        } else {
            sqlx::query("DELETE FROM lash_pending_turn_inputs WHERE state IN ($1, $2)")
                .bind(lash_core::TurnInputState::Cancelled.as_str())
                .bind(lash_core::TurnInputState::Completed.as_str())
                .execute(&self.pool)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected()
        };
        Ok(VacuumReport {
            removed_node_count: removed_node_count as usize,
            removed_pending_turn_input_tombstone_count: removed_pending_turn_input_tombstone_count
                as usize,
        })
    }

    /// Checkpoint-rooted mark/sweep over `lash_blobs`, mirroring the SQLite
    /// store's semantics ([`GcReport`] fields match). PostgreSQL stores each
    /// checkpoint as one manifest plus separately addressed tool, plugin, and
    /// execution-state components. The four Lashlang artifact namespaces live
    /// in a separate, upsert-in-place table (`lash_lashlang_artifacts`).
    /// Session-owned trigger-manifest rows are removed with their session; the
    /// other artifact rows are retained service roots, so GC does not touch
    /// this table.
    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
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
            for child in [
                manifest.tool_state_ref,
                manifest.plugin_snapshot_ref,
                manifest.execution_state_ref,
            ]
            .into_iter()
            .flatten()
            {
                retained.insert(child.0);
            }
        }
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

fn derive_pending_turn_input_id(
    session_id: &str,
    source_key: Option<&str>,
    now_epoch_ms: u64,
) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "ti:{:x}",
        Sha256::digest(format!("{session_id}:{source_key:?}:{now_epoch_ms}:{nanos}").as_bytes())
    )
}

enum ClaimTransactionOutcome<T> {
    Commit(T),
    Rollback(T),
}

#[allow(clippy::too_many_arguments)]
async fn checkpoint_work_pending_postgres(
    pool: &PgPool,
    session_id: &str,
    generation: u64,
    turn_id: &str,
    checkpoint: lash_core::CheckpointKind,
    max_inputs: usize,
    max_batches: usize,
) -> Result<bool, StoreError> {
    if max_inputs == 0 && max_batches == 0 {
        return Ok(false);
    }
    let head_candidate =
        postgres_queued_work_head_candidate_cte(QueuedWorkClaimBoundary::ActiveTurnCheckpoint);
    let sql = format!(
        "WITH {head_candidate}
         SELECT (
            $5 > 0 AND EXISTS (
                SELECT 1
                FROM lash_pending_turn_inputs
                WHERE session_id = $1
                  AND state = 'pending_active'
                  AND (claim_token IS NULL OR claim_session_lease_generation <> $2)
                  AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                  AND ingress_json::jsonb ->> 'turn_id' = $3
                  AND (
                      $4 = 'before_completion'
                      OR COALESCE(
                          ingress_json::jsonb ->> 'min_boundary',
                          'after_work'
                      ) = 'after_work'
                  )
                LIMIT 1
            )
         ) OR (
            $6 > 0 AND EXISTS (
                SELECT 1
                FROM lash_queued_work_items AS item
                JOIN queued_work_head_candidate AS head
                  ON head.head_batch_id = item.batch_id
                WHERE item.payload_json::jsonb ->> 'type' <> 'session_command'
                LIMIT 1
            )
         )"
    );
    sqlx::query_scalar(&sql)
        .bind(session_id)
        .bind(generation as i64)
        .bind(turn_id)
        .bind(match checkpoint {
            lash_core::CheckpointKind::AfterWork => "after_work",
            lash_core::CheckpointKind::BeforeCompletion => "before_completion",
        })
        .bind(max_inputs as i64)
        .bind(max_batches as i64)
        .fetch_one(pool)
        .await
        .map_err(store_sqlx_error)
}

#[allow(clippy::too_many_arguments)]
async fn claim_ready_queued_work_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    max_batches: usize,
) -> Result<ClaimTransactionOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if max_batches == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(boundary))
        .bind(session_id)
        .bind(generation as i64)
        .bind(claim_scan_limit(max_batches))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut selected = Vec::new();
    for row in rows {
        let row = queued_batch_row(row)?;
        if row.claim_token.is_none() || row.claim_session_lease_generation != generation {
            selected.push(row);
        }
    }
    let mut selected_batches = Vec::new();
    for row in &selected {
        selected_batches.push(queued_work_batch_from_row(tx, row.clone()).await?);
    }
    let candidates = selected
        .iter()
        .zip(selected_batches.iter())
        .map(|(row, batch)| {
            Ok(ClaimCandidate {
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                work_class: batch.work_class().ok_or_else(|| {
                    StoreError::Backend(format!(
                        "queued-work batch `{}` has mixed or empty payload classes",
                        batch.batch_id
                    ))
                })?,
                delivery_policy: row.delivery_policy,
                slot_policy: row.slot_policy,
                merge_key: row.merge_key.clone(),
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let selected_len = select_turn_work_claim_prefix(&candidates, boundary, max_batches);
    if selected_len == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    selected.truncate(selected_len);
    selected_batches.truncate(selected_len);
    let lease = QueuedWorkClaimLease::derive(&candidates[0], session_id, owner, now, generation);
    let liveness_json: Option<&str> = None;
    for row in &selected {
        let changed = sqlx::query(
            "UPDATE lash_queued_work_batches
             SET claim_id = $3,
                 claim_owner_id = $4,
                 claim_owner_incarnation_id = $5,
                 claim_owner_liveness_json = $6,
                 claim_token = $7,
                 claim_fencing_token = claim_fencing_token + 1,
                 claim_session_lease_generation = $8
             WHERE session_id = $1
               AND batch_id = $2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $8
               )",
        )
        .bind(session_id)
        .bind(&row.batch_id)
        .bind(&lease.claim_id)
        .bind(&owner.owner_id)
        .bind(&owner.incarnation_id)
        .bind(liveness_json)
        .bind(&lease.lease_token)
        .bind(lease.session_lease_generation as i64)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
    }
    Ok(ClaimTransactionOutcome::Commit(Some(QueuedWorkClaim {
        session_id: session_id.to_string(),
        claim_id: lease.claim_id,
        owner: owner.clone(),
        lease_token: lease.lease_token,
        fencing_token: lease.fencing_token,
        session_lease_generation: lease.session_lease_generation,
        batches: selected_batches,
    })))
}

#[allow(clippy::too_many_arguments)]
async fn claim_pending_turn_inputs_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<ClaimTransactionOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let wanted_state = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
            lash_core::TurnInputState::PendingActive
        }
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = ",
    );
    query
        .push_bind(session_id)
        .push(" AND state = ")
        .push_bind(wanted_state.as_str())
        .push(
            "
           AND (
                claim_token IS NULL
                OR claim_session_lease_generation <> ",
        )
        .push_bind(generation as i64)
        .push("\n           )");
    if let lash_core::TurnInputClaimMode::ActiveTurn {
        turn_id,
        checkpoint,
    } = &mode
    {
        query
            .push(" AND ingress_json::jsonb ->> 'scope' = 'active_turn'")
            .push(" AND ingress_json::jsonb ->> 'turn_id' = ")
            .push_bind(turn_id);
        if *checkpoint == lash_core::CheckpointKind::AfterWork {
            query.push(
                " AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work') = 'after_work'",
            );
        }
    }
    query
        .push(" ORDER BY enqueue_seq ASC LIMIT ")
        .push_bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
        .push(" FOR UPDATE SKIP LOCKED");
    let rows = query
        .build()
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let selected = rows
        .into_iter()
        .take(max_inputs)
        .map(|row| {
            let row = pending_turn_input_row(row)?;
            Ok((row.clone(), pending_turn_input_from_row(row)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let Some((head, _)) = selected.first() else {
        return Ok(ClaimTransactionOutcome::Commit(None));
    };
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation);
    let liveness_json: Option<&str> = None;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for (row, mut input) in selected {
        let changed = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = $3,
                 claim_id = $4,
                 claim_owner_id = $5,
                 claim_owner_incarnation_id = $6,
                 claim_owner_liveness_json = $7,
                 claim_token = $8,
                 claim_fencing_token = claim_fencing_token + 1,
                 claim_session_lease_generation = $9
             WHERE session_id = $1
               AND input_id = $2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $9
               )",
        )
        .bind(session_id)
        .bind(&row.input_id)
        .bind(state_after_claim.as_str())
        .bind(&lease.claim_id)
        .bind(&owner.owner_id)
        .bind(&owner.incarnation_id)
        .bind(liveness_json)
        .bind(&lease.lease_token)
        .bind(lease.session_lease_generation as i64)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        input.state = state_after_claim;
        inputs.push(input);
    }
    Ok(ClaimTransactionOutcome::Commit(Some(
        lash_core::TurnInputClaim {
            session_id: session_id.to_string(),
            claim_id: lease.claim_id,
            owner: owner.clone(),
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
            mode,
            inputs,
            applications: Vec::new(),
        },
    )))
}

async fn claim_pending_turn_inputs_postgres(
    pool: &PgPool,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseFence,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(&mut tx).await?;
    let wanted_state = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
            lash_core::TurnInputState::PendingActive
        }
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT enqueue_seq, input_id, session_id, source_key, ingress_json,
                state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = ",
    );
    query
        .push_bind(session_id)
        .push(" AND state = ")
        .push_bind(wanted_state.as_str())
        .push(
            "
           AND (
                claim_token IS NULL
                OR claim_session_lease_generation <> ",
        )
        .push_bind(generation as i64)
        .push("\n           )");
    if let lash_core::TurnInputClaimMode::ActiveTurn {
        turn_id,
        checkpoint,
    } = &mode
    {
        query
            .push(" AND ingress_json::jsonb ->> 'scope' = 'active_turn'")
            .push(" AND ingress_json::jsonb ->> 'turn_id' = ")
            .push_bind(turn_id);
        if *checkpoint == lash_core::CheckpointKind::AfterWork {
            query.push(
                " AND COALESCE(ingress_json::jsonb ->> 'min_boundary', 'after_work') = 'after_work'",
            );
        }
    }
    query
        .push(" ORDER BY enqueue_seq ASC LIMIT ")
        .push_bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
        .push(" FOR UPDATE SKIP LOCKED");
    let rows = query
        .build()
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let selected = rows
        .into_iter()
        .take(max_inputs)
        .map(|row| {
            let row = pending_turn_input_row(row)?;
            Ok((row.clone(), pending_turn_input_from_row(row)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let Some((head, _)) = selected.first() else {
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(None);
    };
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation);
    let liveness_json: Option<&str> = None;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for (row, mut input) in selected {
        let changed = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = $3,
                 claim_id = $4,
                 claim_owner_id = $5,
                 claim_owner_incarnation_id = $6,
                 claim_owner_liveness_json = $7,
                 claim_token = $8,
                 claim_fencing_token = claim_fencing_token + 1,
                 claim_session_lease_generation = $9
             WHERE session_id = $1
               AND input_id = $2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $9
               )",
        )
        .bind(session_id)
        .bind(&row.input_id)
        .bind(state_after_claim.as_str())
        .bind(&lease.claim_id)
        .bind(&owner.owner_id)
        .bind(&owner.incarnation_id)
        .bind(liveness_json)
        .bind(&lease.lease_token)
        .bind(lease.session_lease_generation as i64)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            tx.rollback().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        input.state = state_after_claim;
        inputs.push(input);
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(lash_core::TurnInputClaim {
        session_id: session_id.to_string(),
        claim_id: lease.claim_id,
        owner: owner.clone(),
        lease_token: lease.lease_token,
        fencing_token: lease.fencing_token,
        session_lease_generation: lease.session_lease_generation,
        mode,
        inputs,
        applications: Vec::new(),
    }))
}

pub(crate) struct SessionExecutionLeaseRow {
    owner: Option<LeaseOwnerIdentity>,
    pub(crate) lease_token: Option<String>,
    pub(crate) fencing_token: u64,
    claimed_at_ms: u64,
    pub(crate) expires_at_ms: u64,
}

pub(crate) async fn load_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_owner_liveness_json
         FROM lash_session_execution_leases
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(row.map(|row| SessionExecutionLeaseRow {
        owner: lease_owner_from_columns(row.get(0), row.get(5), row.get(6)),
        lease_token: row.get(1),
        fencing_token: row.get::<i64, _>(2) as u64,
        claimed_at_ms: row.get::<i64, _>(3) as u64,
        expires_at_ms: row.get::<i64, _>(4) as u64,
    }))
}

pub(crate) fn lease_owner_from_columns(
    owner_id: Option<String>,
    incarnation_id: Option<String>,
    _liveness_json: Option<String>,
) -> Option<LeaseOwnerIdentity> {
    owner_id.map(|owner_id| LeaseOwnerIdentity {
        incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
        owner_id,
    })
}

fn row_to_session_execution_lease(
    session_id: &str,
    row: SessionExecutionLeaseRow,
) -> Result<SessionExecutionLease, StoreError> {
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: row
            .owner
            .ok_or_else(|| StoreError::Backend("live session lease missing owner".to_string()))?,
        lease_token: row.lease_token.ok_or_else(|| {
            StoreError::Backend("live session lease missing lease token".to_string())
        })?,
        fencing_token: row.fencing_token,
        claimed_at_epoch_ms: row.claimed_at_ms,
        expires_at_epoch_ms: row.expires_at_ms,
    })
}

/// Serialize concurrent session-execution-lease claims for one session.
///
/// `try_claim`/`reclaim` read the current lease and then conditionally
/// `acquire` it. That check-then-act is not atomic under Postgres READ
/// COMMITTED, so two concurrent first claims can both observe no live lease and
/// both `ON CONFLICT DO UPDATE`, leaving two acquired winners. A
/// transaction-scoped advisory lock keyed by the session id makes the sequence
/// mutually exclusive per session; Postgres releases it automatically when the
/// transaction ends. (SQLite and the in-memory store serialize writers
/// globally, so they do not need this.)
async fn lock_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0::bigint))")
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

async fn acquire_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    owner: &LeaseOwnerIdentity,
    previous_fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> Result<SessionExecutionLease, StoreError> {
    let fencing_token = previous_fencing_token.saturating_add(1);
    let lease_token = format!(
        "{}:{}:{}:{now}:{fencing_token}",
        session_id, owner.owner_id, owner.incarnation_id
    );
    let expires_at = now.saturating_add(lease_ttl_ms);
    sqlx::query(
        "INSERT INTO lash_session_execution_leases (
            session_id, lease_owner_id, lease_owner_incarnation_id, lease_owner_liveness_json,
            lease_token, lease_fencing_token, lease_claimed_at_ms, lease_expires_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (session_id) DO UPDATE SET
            lease_owner_id = EXCLUDED.lease_owner_id,
            lease_owner_incarnation_id = EXCLUDED.lease_owner_incarnation_id,
            lease_owner_liveness_json = EXCLUDED.lease_owner_liveness_json,
            lease_token = EXCLUDED.lease_token,
            lease_fencing_token = EXCLUDED.lease_fencing_token,
            lease_claimed_at_ms = EXCLUDED.lease_claimed_at_ms,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms",
    )
    .bind(session_id)
    .bind(&owner.owner_id)
    .bind(&owner.incarnation_id)
    .bind(Option::<&str>::None)
    .bind(&lease_token)
    .bind(fencing_token as i64)
    .bind(now as i64)
    .bind(expires_at as i64)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: owner.clone(),
        lease_token,
        fencing_token,
        claimed_at_epoch_ms: now,
        expires_at_epoch_ms: expires_at,
    })
}

async fn ensure_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    fence: &SessionExecutionLeaseFence,
) -> Result<(), StoreError> {
    if fence.session_id != session_id {
        return Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        });
    }
    let now = postgres_transaction_epoch_ms(tx).await?;
    let current = load_session_execution_lease_tx(tx, session_id).await?;
    let Some(current) = current else {
        return Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        });
    };
    if current
        .owner
        .as_ref()
        .is_some_and(|owner| owner.same_incarnation(&fence.owner))
        && current.lease_token.as_deref() == Some(fence.lease_token.as_str())
        && current.fencing_token == fence.fencing_token
        && current.expires_at_ms > now
    {
        Ok(())
    } else {
        Err(StoreError::SessionExecutionLeaseExpired {
            session_id: session_id.to_string(),
        })
    }
}

async fn release_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completion: &SessionExecutionLeaseCompletion,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE lash_session_execution_leases
         SET lease_owner_id = NULL,
             lease_owner_incarnation_id = NULL,
             lease_owner_liveness_json = NULL,
             lease_token = NULL,
             lease_claimed_at_ms = 0,
             lease_expires_at_ms = 0
         WHERE session_id = $1
           AND lease_owner_id = $2
           AND lease_owner_incarnation_id = $3
           AND lease_token = $4
           AND lease_fencing_token = $5",
    )
    .bind(&completion.session_id)
    .bind(&completion.owner.owner_id)
    .bind(&completion.owner.incarnation_id)
    .bind(&completion.lease_token)
    .bind(completion.fencing_token as i64)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}
