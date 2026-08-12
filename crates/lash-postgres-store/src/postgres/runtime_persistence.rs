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
    if boundary == QueuedWorkClaimBoundary::Idle {
        return format!(
            "queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
            FROM (
                SELECT enqueue_seq AS head_enqueue_seq,
                       batch_id AS head_batch_id,
                       delivery_policy AS head_delivery_policy,
                       claim_id AS head_claim_id
                FROM lash_queued_work_batches
                WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
                ORDER BY enqueue_seq ASC
                LIMIT 1
            ) AS unfiltered_head
         )"
        );
    }
    format!(
        "queued_work_unfiltered_head AS (
            SELECT enqueue_seq AS head_enqueue_seq,
                   batch_id AS head_batch_id,
                   delivery_policy AS head_delivery_policy,
                   claim_id AS head_claim_id
            FROM lash_queued_work_batches
            WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
            ORDER BY enqueue_seq ASC
            LIMIT 1
         ),
         queued_work_head_candidate AS (
            SELECT head_enqueue_seq, head_batch_id, head_delivery_policy, head_claim_id
            FROM (
                SELECT candidate.enqueue_seq AS head_enqueue_seq,
                       candidate.batch_id AS head_batch_id,
                       candidate.delivery_policy AS head_delivery_policy,
                       candidate.claim_id AS head_claim_id
                FROM lash_queued_work_batches AS candidate
                CROSS JOIN queued_work_unfiltered_head AS unfiltered
                WHERE candidate.session_id = $1
                  AND candidate.available_at_ms <= FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000)
                  AND (
                       candidate.claim_token IS NULL
                       OR candidate.claim_session_lease_generation <> $2
                  )
                  AND (
                       (
                            candidate.enqueue_seq = unfiltered.head_enqueue_seq
                            AND unfiltered.head_delivery_policy = 'earliest_safe_boundary'
                       )
                       OR (
                            unfiltered.head_delivery_policy <> 'earliest_safe_boundary'
                            AND unfiltered.head_claim_id IS NOT NULL
                            AND candidate.claim_id IS DISTINCT FROM unfiltered.head_claim_id
                       )
                  )
                ORDER BY candidate.enqueue_seq ASC
                LIMIT 1
            ) AS boundary_head
            WHERE head_delivery_policy = 'earliest_safe_boundary'
         )"
    )
}

fn postgres_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> String {
    let head_candidate = postgres_queued_work_head_candidate_cte(boundary);
    format!(
        "WITH {head_candidate}
         SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
         FROM lash_queued_work_batches
         CROSS JOIN queued_work_head_candidate
         WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
           AND enqueue_seq >= head_enqueue_seq
           AND (head_claim_id IS NULL OR lash_queued_work_batches.claim_id = head_claim_id)
         ORDER BY enqueue_seq ASC
         LIMIT COALESCE((
             SELECT CASE WHEN head_claim_id IS NULL THEN $3 ELSE 9223372036854775807 END
             FROM queued_work_head_candidate
         ), 0)
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
        "SELECT frame_node_id FROM lash_graph_nodes
         WHERE node_id = $1 AND tombstoned = FALSE",
    )
    .bind(leaf_node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

async fn enqueue_queued_work_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch: &QueuedWorkBatchDraft,
    now: u64,
) -> Result<QueuedWorkBatch, StoreError> {
    enqueue_queued_work_with_outcome_tx(tx, batch, now)
        .await
        .map(QueuedWorkEnqueueOutcome::into_batch)
}

async fn enqueue_queued_work_with_outcome_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch: &QueuedWorkBatchDraft,
    now: u64,
) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
    let sql_available_at_ms =
        sql_counter_value("queued_work_available_at_ms", batch.available_at_ms)?;
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
    let allocation_floor = allocation_floor
        .map(|value| u64_from_sql("WakeAllocationFloor", "allocation_floor", value))
        .transpose()?;
    if let (Some(wake_source), Some(allocation_floor)) =
        (batch.process_wake_source.as_ref(), allocation_floor)
        && wake_source.sequence <= allocation_floor
    {
        return Err(StoreError::ProcessWakeSequenceRewound {
            session_id: batch.session_id.clone(),
            process_id: wake_source.process_id.clone(),
            sequence: wake_source.sequence,
            allocation_floor,
        });
    }
    let enqueue_seq: i64 = sqlx::query_scalar(
        "SELECT nextval(pg_get_serial_sequence(
            'lash_queued_work_batches',
            'enqueue_seq'
         ))",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let enqueue_seq_u64 = u64_from_sql("QueuedWorkBatch", "enqueue_seq", enqueue_seq)?;
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(enqueue_seq_u64),
    );
    sqlx::query(
        "INSERT INTO lash_queued_work_batches (
            enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
            authority_json, merge_key, available_at_ms, enqueued_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(enqueue_seq)
    .bind(&batch_id)
    .bind(&batch.session_id)
    .bind(&batch.source_key)
    .bind(batch.delivery_policy.as_str())
    .bind(batch.kind.as_str())
    .bind(encode_json(&batch.authority)?)
    .bind(&batch.merge_key)
    .bind(sql_available_at_ms)
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
        .bind(encode_json(payload)?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    let queued = load_queued_batch(tx, &batch_id)
        .await?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    debug_assert_eq!(queued.enqueue_seq, enqueue_seq_u64);
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
        let session_id = &self.session_id;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let Some(meta) = load_session_head_meta_tx(&mut tx, session_id, false).await? else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let leaf_node_id = meta.leaf_node_id.clone();
        let graph = load_graph_tx(&mut tx, session_id, leaf_node_id.clone()).await?;
        let checkpoint = match meta.checkpoint_ref.as_ref() {
            Some(blob_ref) => get_checkpoint_tx(&mut tx, blob_ref).await?,
            None => None,
        };
        let token_ledger = lash_core::store::merge_token_ledger_entries_checked(
            load_usage_deltas_tx(&mut tx, session_id).await?,
        )?;
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

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        let meta = load_session_head_meta_tx(&mut tx, &self.session_id, false).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(meta)
    }

    async fn load_node(&self, node_id: &str) -> Result<Option<SessionNodeRecord>, StoreError> {
        let session_id = &self.session_id;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let row = sqlx::query(
            "SELECT node.node_id, node.parent_node_id, node.node_json,
                    node.session_id, node.generation
             FROM lash_graph_nodes AS node
             WHERE node.node_id = $1 AND node.tombstoned = FALSE
               AND (
                   node.session_id = $2
                   OR EXISTS (
                       SELECT 1 FROM lash_fork_lineage AS lineage
                       WHERE lineage.session_id = $2
                         AND lineage.ancestor_session_id = node.session_id
                         AND node.generation <= lineage.fork_generation
                   )
               )",
        )
        .bind(node_id)
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let candidate_id: String = row.get(0);
        let parent_node_id: Option<String> = row.get(1);
        let json: String = row.get(2);
        let owner: String = row.get(3);
        let candidate_generation: i64 = row.get(4);
        if owner != *session_id {
            let rows = sqlx::query(
                "WITH readable_sessions(session_id, generation_ceiling) AS (
                     SELECT $1::TEXT, NULL::BIGINT
                     UNION ALL
                     SELECT lineage.ancestor_session_id, lineage.fork_generation
                     FROM lash_fork_lineage AS lineage
                     WHERE lineage.session_id = $1
                 )
                 SELECT session.leaf_node_id, head.generation, head.tombstoned,
                        node.node_id, node.parent_node_id,
                        node.generation, node.tombstoned
                 FROM lash_sessions AS session
                 LEFT JOIN lash_graph_nodes AS head
                   ON head.node_id = session.leaf_node_id
                 LEFT JOIN readable_sessions AS readable ON TRUE
                 LEFT JOIN lash_graph_nodes AS node
                   ON node.session_id = readable.session_id
                  AND node.generation BETWEEN $2 AND head.generation
                  AND (
                      readable.generation_ceiling IS NULL
                      OR node.generation <= readable.generation_ceiling
                  )
                 WHERE session.session_id = $1",
            )
            .bind(session_id)
            .bind(candidate_generation)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let Some(first) = rows.first() else {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            };
            let head_id: Option<String> = first.get(0);
            let Some(head_id) = head_id else {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            };
            let head_generation: Option<i64> = first.get(1);
            let head_tombstoned: Option<bool> = first.get(2);
            let (Some(head_generation), Some(head_tombstoned)) = (head_generation, head_tombstoned)
            else {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "head leaf is missing".to_string(),
                });
            };
            if head_tombstoned {
                return Err(StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "head leaf is tombstoned".to_string(),
                });
            }
            if candidate_generation < 0 || candidate_generation > head_generation {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(None);
            }
            let mut range = std::collections::HashMap::new();
            for row in rows {
                let node_id: Option<String> = row.get(3);
                let generation: Option<i64> = row.get(5);
                let tombstoned: Option<bool> = row.get(6);
                if let (Some(node_id), Some(generation), Some(tombstoned)) =
                    (node_id, generation, tombstoned)
                {
                    range.insert(
                        node_id,
                        (row.get::<Option<String>, _>(4), generation, tombstoned),
                    );
                }
            }
            let mut current_id = head_id;
            let mut current_generation = head_generation;
            loop {
                let Some((parent_id, generation, tombstoned)) = range.get(&current_id) else {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "readable generation range omits an edge-path node".to_string(),
                    });
                };
                if *tombstoned || *generation != current_generation {
                    return Err(StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "parent edge crosses a tombstone or generation gap".to_string(),
                    });
                }
                if current_generation == candidate_generation {
                    if current_id != candidate_id {
                        tx.commit().await.map_err(store_sqlx_error)?;
                        return Ok(None);
                    }
                    break;
                }
                current_id = parent_id
                    .clone()
                    .ok_or_else(|| StoreError::StoredDataCorrupt {
                        record_kind: "SessionGraph",
                        message: "parent edge ended before the candidate generation".to_string(),
                    })?;
                current_generation -= 1;
            }
        }
        let node = SessionNodeRecord::decode_storage_body(candidate_id, parent_node_id, &json)
            .map_err(|err| StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph node",
                message: err.to_string(),
            })?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(node))
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        let planner = lash_core::store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        self.bind_session_id(&commit.session_id)?;
        let now = self.clock.timestamp_ms();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // A head row does not exist during the first commit, so row locking
        // alone cannot serialize create-versus-delete. This session-keyed lock
        // is the common authority for every history commit and deletion.
        ensure_session_not_deleted_tx(&mut tx, &commit.session_id).await?;
        if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
            ensure_session_execution_lease_tx(&mut tx, &commit.session_id, fence).await?;
        }
        // Read without a lock for early validation and receipt replay. Before
        // mutating graph reachability, existing sessions lock and recheck this
        // revision so commit, maintenance, and deletion share one authority.
        let existing = load_session_head_meta_tx(&mut tx, &commit.session_id, false).await?;
        planner.validate_session_binding(existing.as_ref().map(|meta| meta.session_id.as_str()))?;
        let direct_meta = SessionMeta {
            session_id: commit.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
        };
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &direct_meta,
            crate::session_meta::SessionMetaWrite::Insert,
        )
        .await?;
        planner.validate_node_derivation()?;
        {
            let prior = sqlx::query(
                "SELECT turn_commit_hash, result_json,
                        request_identity_hash, identity_encoding_version,
                        requested_node_count
                 FROM lash_runtime_turn_commits
                 WHERE session_id = $1 AND turn_id = $2",
            )
            .bind(&commit.session_id)
            .bind(planner.operation_key())
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
                let result = store_decode_json(&result_json, "runtime turn commit result")?;
                let prior = lash_core::store::RuntimeCommitReceiptRecord {
                    turn_commit_hash: hash,
                    result,
                    request_identity_hash: stored_identity,
                    identity_encoding_version: stored_version
                        .and_then(|version| u32::try_from(version).ok()),
                    requested_node_count: stored_count,
                };
                if let Some(replay) = planner.decide_receipt(Some(prior))? {
                    if let Some(completion) = replay.release_session_execution_lease() {
                        let _release_was_current =
                            release_session_execution_lease_tx(&mut tx, completion).await?;
                        // FIG-884: ancillary stale release must never veto a
                        // replayed commit or clear a successor claim.
                    }
                    tx.commit().await.map_err(store_sqlx_error)?;
                    return Ok(replay.into_result());
                }
            }
        }
        let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
        if existing.is_none() {
            let placeholder = SessionHeadMeta::assemble(
                SessionHeadPayload {
                    schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                    session_id: commit.session_id.clone(),
                    config: commit.config.clone(),
                    current_frame_node_id: None,
                },
                0,
                None,
                None,
            );
            sqlx::query(
                "INSERT INTO lash_sessions
                 (session_id, head_revision, head_json, checkpoint_ref, leaf_node_id)
                 VALUES ($1, 0, $2, NULL, NULL)
                 ON CONFLICT (session_id) DO NOTHING",
            )
            .bind(&commit.session_id)
            .bind(encode_json(&placeholder.payload())?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        let locked_revision = sqlx::query_scalar::<_, i64>(
            "SELECT head_revision
             FROM lash_sessions
             WHERE session_id = $1
             FOR UPDATE",
        )
        .bind(&commit.session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
        .transpose()?
        .ok_or_else(|| StoreError::StoredDataCorrupt {
            record_kind: "SessionHeadMeta",
            message: "head row disappeared while commit authority was held".to_string(),
        })?;
        let old_leaf_node_id = existing.as_ref().and_then(|head| head.leaf_node_id.clone());
        let parent_node_facts = match old_leaf_node_id.as_deref() {
            Some(leaf_node_id) => sqlx::query_as::<_, (i64, String)>(
                "SELECT generation, frame_node_id FROM lash_graph_nodes
                 WHERE node_id = $1 AND tombstoned = FALSE
                 FOR UPDATE",
            )
            .bind(leaf_node_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|(generation, frame_node_id)| {
                Ok(lash_core::store::ParentNodeFacts {
                    node_id: leaf_node_id.to_string(),
                    generation: u64_from_sql("SessionGraph node", "generation", generation)?,
                    frame_node_id,
                })
            })
            .transpose()?,
            None => None,
        };
        let requested_ancestor_is_active = match (
            commit.turn_commit.requested_ancestor_node_id.as_deref(),
            parent_node_facts.as_ref(),
        ) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(required), Some(parent)) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                     SELECT 1 FROM lash_graph_nodes AS node
                     WHERE node.node_id = $1
                       AND node.tombstoned = FALSE
                       AND node.generation <= $3
                       AND (
                           node.session_id = $2
                           OR EXISTS (
                               SELECT 1 FROM lash_fork_lineage AS lineage
                               WHERE lineage.session_id = $2
                                 AND lineage.ancestor_session_id = node.session_id
                                 AND node.generation <= lineage.fork_generation
                           )
                       )
                 )",
            )
            .bind(required)
            .bind(&commit.session_id)
            .bind(i64::try_from(parent.generation).map_err(|_| {
                StoreError::Backend("parent generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
        };
        let authoritative_revision = locked_revision.max(actual_revision);
        let node_ids = commit
            .graph
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>();
        let occupied_node_ids = sqlx::query_scalar::<_, String>(
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
        let selected_leaf_is_live = match commit.graph.leaf_node_id() {
            Some(leaf_node_id) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(
                    SELECT 1 FROM lash_graph_nodes
                    WHERE node_id = $1 AND tombstoned = FALSE
                )",
            )
            .bind(leaf_node_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
            None => false,
        };
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
        let old_leaf_is_live = old_leaf_node_id.is_none() || parent_node_facts.is_some();
        let plan = planner.plan(lash_core::store::FreshRuntimeCommitFacts {
            actual_head_revision: authoritative_revision,
            old_leaf_node_id,
            requested_ancestor_is_active,
            occupied_node_ids,
            selected_leaf_is_live,
            has_live_nodes,
            old_leaf_is_live,
            parent_node_facts,
        })?;
        let sql_head_revision = sql_monotonic_counter_value(
            "session_head_revision",
            plan.actual_head_revision(),
            plan.next_head_revision(),
        )?;
        for completed in &commit.completed_queue_claims {
            ensure_queued_work_completion_tx(&mut tx, completed).await?;
        }
        for completed in &commit.completed_turn_input_claims {
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
            .bind(encode_json(&entry.entry)?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        for (node, facts) in commit.graph.nodes.iter().zip(plan.planned_node_facts()) {
            let node_json = node.encode_storage_body().map_err(|err| {
                StoreError::Backend(format!("failed to encode graph node body: {err}"))
            })?;
            sqlx::query(
                "INSERT INTO lash_graph_nodes
                     (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
                     VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&commit.session_id)
            .bind(&node.node_id)
            .bind(&node.parent_node_id)
            .bind(i64::try_from(facts.generation).map_err(|_| {
                StoreError::Backend("node generation does not fit PostgreSQL BIGINT".to_string())
            })?)
            .bind(&facts.frame_node_id)
            .bind(node_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                graph_node_insert_error(error, &commit.session_id, facts.generation, &node.node_id)
            })?;
        }
        let meta = plan.head_meta(checkpoint_ref.clone());
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
        .bind(sql_head_revision)
        .bind(encode_json(&meta.payload())?)
        .bind(checkpoint_ref.as_str())
        .bind(meta.leaf_node_id.as_deref())
        .bind(plan.actual_head_revision() as i64)
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
            .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
            .transpose()?
            .unwrap_or(plan.actual_head_revision());
            return Err(plan.head_publication_conflict(actual_now));
        }
        if plan.head_changed()
            && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
        {
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
                .bind(encode_json(&lash_core::TurnInputIngress::NextTurn)?)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            }
        }
        commit_attachment_refs_tx(
            &mut tx,
            &commit.session_id,
            &commit.committed_attachment_ids,
            now,
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
            enqueued_queue_batches.push(enqueue_queued_work_tx(&mut tx, batch, now).await?);
        }
        let result = plan.result(checkpoint_ref, manifest, enqueued_queue_batches);
        {
            let receipt = plan.receipt_write(&result);
            sqlx::query(
                "INSERT INTO lash_runtime_turn_commits (
                    session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                    request_identity_hash, requested_node_count,
                    requested_ancestor_node_id, identity_encoding_version
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(receipt.session_id)
            .bind(receipt.operation_key)
            .bind(receipt.turn_commit_hash)
            .bind(encode_json(receipt.result)?)
            .bind(now as i64)
            .bind(receipt.request_identity_hash)
            .bind(receipt.requested_node_count.map(|count| count as i64))
            .bind(receipt.requested_ancestor_node_id)
            .bind(
                receipt
                    .identity_encoding_version
                    .and_then(|version| i32::try_from(version).ok()),
            )
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            let _release_was_current =
                release_session_execution_lease_tx(&mut tx, completion).await?;
            // FIG-884: head CAS is commit authority; release is ancillary.
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
            relation: binding.relation.clone(),
        };
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let inserted = crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Insert,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(if inserted {
            lash_core::SessionAdmission::Created
        } else {
            lash_core::SessionAdmission::Rebound
        })
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.bind_session_id(&meta.session_id)?;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &meta.session_id).await?;
        crate::session_meta::write_session_meta_tx(
            &mut tx,
            &meta,
            crate::session_meta::SessionMetaWrite::Replace,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        crate::session_meta::load_session_meta(&self.pool, Some(&self.session_id)).await
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
                    row_id: Some(batch_id.clone().into_boxed_str()),
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
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
                    row_id: Some(input_id.clone().into_boxed_str()),
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for PostgresSessionStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &lash_core::LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let lease_token = claim_nonce.as_str();
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
                && current.executor_id.as_deref() == Some(executor_id)
            {
                let expires_at = now.saturating_add(lease_ttl_ms);
                let sql_expires_at =
                    sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
                let claimed_at = current.claimed_at_ms;
                sqlx::query(
                    "UPDATE lash_session_execution_leases
                     SET lease_token = $2,
                         lease_claimed_at_ms = $3,
                         lease_expires_at_ms = $4
                     WHERE session_id = $1",
                )
                .bind(session_id)
                .bind(lease_token)
                .bind(claimed_at as i64)
                .bind(sql_expires_at)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                tx.commit().await.map_err(store_sqlx_error)?;
                // Reentry advances no generation: nobody is displaced.
                return Ok(SessionExecutionLeaseClaimOutcome::Acquired(
                    SessionExecutionLeaseAcquisition::fresh(SessionExecutionLease {
                        session_id: session_id.to_string(),
                        owner: owner.clone(),
                        executor_id: executor_id.to_string(),
                        lease_token: lease_token.to_string(),
                        fencing_token: current.fencing_token,
                        claimed_at_epoch_ms: claimed_at,
                        expires_at_epoch_ms: expires_at,
                    }),
                ));
            }
            let holder = row_to_session_execution_lease(session_id, current)?;
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(SessionExecutionLeaseClaimOutcome::Busy { holder });
        }
        let previous_fencing_token = current.as_ref().map_or(0, |lease| lease.fencing_token);
        // The lapsed holder, read under the same row lock as the claim. The
        // winner is the only party guaranteed alive to report the takeover.
        let displaced = current.as_ref().and_then(|lease| {
            lease
                .owner
                .clone()
                .zip(lease.executor_id.clone())
                .filter(|(previous, previous_executor_id)| {
                    !previous.same_incarnation(owner) || previous_executor_id != executor_id
                })
                .map(|(previous, previous_executor_id)| {
                    (
                        previous,
                        previous_executor_id,
                        lease.fencing_token,
                        lease.expires_at_ms,
                    )
                })
        });
        let lease = acquire_session_execution_lease_tx(
            &mut tx,
            lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity {
                session_id,
                owner,
                executor_id,
                lease_token,
            },
            previous_fencing_token,
            now,
            lease_ttl_ms,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLeaseClaimOutcome::Acquired(
            match displaced {
                Some((previous, previous_executor_id, generation, expired_at_epoch_ms)) => {
                    SessionExecutionLeaseAcquisition::displacing_observed(
                        lease,
                        previous,
                        previous_executor_id,
                        generation,
                        expired_at_epoch_ms,
                    )
                }
                None => SessionExecutionLeaseAcquisition::fresh(lease),
            },
        ))
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        // Keep claim and renewal on one explicit per-session lock order. The
        // row read below was already `FOR UPDATE`, so this is a hardening pin
        // and an auditable lock-ordering rule, not a repair for a reachable
        // stale-read race.
        lock_session_execution_lease_tx(&mut tx, &fence.session_id).await?;
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
            || current.executor_id.as_deref() != Some(fence.executor_id.as_str())
            || current.lease_token.as_deref() != Some(fence.lease_token.as_str())
        {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "owner_or_token_mismatch",
                "postgres_locked_transaction",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.executor_id.as_deref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseRenewalRefused {
                session_id: fence.session_id.clone(),
            });
        }
        if current.expires_at_ms <= now {
            return Err(StoreError::SessionExecutionLeaseExpired {
                session_id: fence.session_id.clone(),
            });
        }
        let expires_at = now.saturating_add(lease_ttl_ms);
        let sql_expires_at =
            sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
        let renewed = sqlx::query(
            "UPDATE lash_session_execution_leases
             SET lease_expires_at_ms = $6
             WHERE session_id = $1
               AND lease_owner_id = $2
               AND lease_owner_incarnation_id = $3
               AND lease_executor_id = $4
               AND lease_token = $5",
        )
        .bind(&fence.session_id)
        .bind(&fence.owner.owner_id)
        .bind(&fence.owner.incarnation_id)
        .bind(&fence.executor_id)
        .bind(&fence.lease_token)
        .bind(sql_expires_at)
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if renewed.rows_affected() != 1 {
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Renewal,
                "conditional_update_did_not_match",
                "postgres_locked_transaction",
                fence,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.owner.as_ref(),
                    current.executor_id.as_deref(),
                    current.lease_token.as_deref(),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseRenewalRefused {
                session_id: fence.session_id.clone(),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(SessionExecutionLease {
            session_id: fence.session_id.clone(),
            owner: fence.owner.clone(),
            executor_id: fence.executor_id.clone(),
            lease_token: fence.lease_token.clone(),
            fencing_token: current.fencing_token,
            claimed_at_epoch_ms: current.claimed_at_ms,
            expires_at_epoch_ms: expires_at,
        })
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        if !release_session_execution_lease_tx(&mut tx, completion).await? {
            let current = load_session_execution_lease_tx(&mut tx, &completion.session_id).await?;
            lash_core::store_backend_support::trace_session_execution_lease_refusal(
                lash_core::store_backend_support::SessionExecutionLeaseRefusalOperation::Release,
                "token_scoped_release_did_not_match",
                "postgres_locked_transaction",
                completion,
                lash_core::store_backend_support::SessionExecutionLeaseRefusalFacts::lifecycle(
                    current.as_ref().and_then(|lease| lease.owner.as_ref()),
                    current
                        .as_ref()
                        .and_then(|lease| lease.executor_id.as_deref()),
                    current
                        .as_ref()
                        .and_then(|lease| lease.lease_token.as_deref()),
                ),
            );
            return Err(StoreError::SessionExecutionLeaseReleaseRefused {
                session_id: completion.session_id.clone(),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        // Non-locking on purpose: observation must never be able to delay the
        // lane it observes. See `read_session_execution_lease_unlocked`.
        let current = read_session_execution_lease_unlocked(&self.pool, session_id).await?;
        // A released row keeps its generation but clears owner and token; only a
        // held row is reported. Expiry stays a raw fact for the caller.
        let Some(current) =
            current.filter(|lease| lease.owner.is_some() && lease.lease_token.is_some())
        else {
            return Ok(None);
        };
        Ok(Some(row_to_session_execution_lease(session_id, current)?))
    }
}

#[async_trait::async_trait]
impl QueuedWorkStore for PostgresSessionStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued = enqueue_queued_work_tx(&mut tx, &batch, self.clock.timestamp_ms()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued =
            enqueue_queued_work_with_outcome_tx(&mut tx, &batch, self.clock.timestamp_ms()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
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
        .bind(sql_session_lease_generation(generation)?)
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
            .map(|(row, batch)| claim_candidate_from_row(row, batch))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let selected_len = select_leading_session_command(&candidates);
        if selected_len == 0 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        selected.truncate(selected_len);
        selected_batches.truncate(selected_len);
        let lease =
            WorkClaimLease::derive_queued_work(&candidates[0], session_id, owner, now, generation)?;
        let sql_fencing_tokens = sql_claim_fencing_tokens(
            "queued_work_claim_fencing_token",
            candidates
                .iter()
                .take(selected_len)
                .map(|candidate| candidate.claim_fencing_token),
        )?;
        let liveness_json: Option<&str> = None;
        for (row, sql_fencing_token) in selected.iter().zip(sql_fencing_tokens.iter().copied()) {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3,
                     claim_owner_id = $4,
                     claim_owner_incarnation_id = $5,
                     claim_owner_liveness_json = $6,
                     claim_token = $7,
                     claim_fencing_token = $9,
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
            .bind(sql_session_lease_generation(
                lease.session_lease_generation,
            )?)
            .bind(sql_fencing_token)
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
            data: lash_core::store_backend_support::queued_work_claim_data(
                selected_batches,
                candidates[0].prior_claim_id.clone(),
            ),
        }))
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        if policy.max_rows == 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(boundary))
            .bind(session_id)
            .bind(sql_session_lease_generation(generation)?)
            .bind(claim_scan_limit(policy.max_rows))
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
            .map(|(row, batch)| claim_candidate_from_row(row, batch))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let selected_len = select_turn_work_claim_prefix(&candidates, boundary, policy, now)?;
        if selected_len == 0 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        selected.truncate(selected_len);
        selected_batches.truncate(selected_len);
        let lease =
            WorkClaimLease::derive_queued_work(&candidates[0], session_id, owner, now, generation)?;
        let sql_fencing_tokens = sql_claim_fencing_tokens(
            "queued_work_claim_fencing_token",
            candidates
                .iter()
                .take(selected_len)
                .map(|candidate| candidate.claim_fencing_token),
        )?;
        let liveness_json: Option<&str> = None;
        for (row, sql_fencing_token) in selected.iter().zip(sql_fencing_tokens.iter().copied()) {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3,
                     claim_owner_id = $4,
                     claim_owner_incarnation_id = $5,
                     claim_owner_liveness_json = $6,
                     claim_token = $7,
                     claim_fencing_token = $9,
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
            .bind(sql_session_lease_generation(
                lease.session_lease_generation,
            )?)
            .bind(sql_fencing_token)
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
            data: lash_core::store_backend_support::queued_work_claim_data(
                selected_batches,
                candidates[0].prior_claim_id.clone(),
            ),
        }))
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: QueuedWorkClaimPolicy,
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
            policy.max_rows,
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
                turn_id: turn_id.clone(),
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
            policy,
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
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: QueuedWorkClaimPolicy,
    ) -> Result<lash_core::SelectedQueuedWorkClaimOutcome, StoreError> {
        if batch_ids.is_empty() {
            return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                None,
                Vec::new(),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let requested_ids = batch_ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let present_ids = sqlx::query_scalar::<_, String>(
            "SELECT batch_id
             FROM lash_queued_work_batches
             WHERE session_id = $1 AND batch_id = ANY($2)",
        )
        .bind(session_id)
        .bind(batch_ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        let already_satisfied_batch_ids = batch_ids
            .iter()
            .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if present_ids.is_empty() {
            tx.rollback().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let requested_rows = sqlx::query(
                "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                        work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                        claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                        claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
                 FROM lash_queued_work_batches
                 WHERE session_id = $1 AND available_at_ms <= $2
                   AND (claim_token IS NULL OR claim_session_lease_generation <> $3)
                   AND batch_id = ANY($4)
                 ORDER BY enqueue_seq ASC",
            )
            .bind(session_id)
            .bind(now as i64)
            .bind(sql_session_lease_generation(generation)?)
            .bind(batch_ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .into_iter()
            .map(queued_batch_row)
            .collect::<Result<Vec<_>, _>>()?;
        if requested_rows.len() != present_ids.len() {
            tx.rollback().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        let involved_claim_ids = requested_rows
            .iter()
            .filter_map(|row| row.claim_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut validation_rows = requested_rows.clone();
        if !involved_claim_ids.is_empty() {
            validation_rows.extend(
                sqlx::query(
                    "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                            work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                            claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                            claim_owner_liveness_json, claim_token,
                            claim_session_lease_generation, claim_id
                     FROM lash_queued_work_batches
                     WHERE session_id = $1 AND available_at_ms <= $2
                       AND (claim_token IS NULL OR claim_session_lease_generation <> $3)
                       AND claim_id = ANY($4)
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id)
                .bind(now as i64)
                .bind(sql_session_lease_generation(generation)?)
                .bind(&involved_claim_ids)
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .into_iter()
                .map(queued_batch_row)
                .collect::<Result<Vec<_>, _>>()?,
            );
            validation_rows.sort_by_key(|row| row.enqueue_seq);
            validation_rows.dedup_by(|left, right| left.batch_id == right.batch_id);
        }
        let validation_batch_claims = validation_rows
            .iter()
            .map(|row| (row.batch_id.clone(), row.claim_id.clone()))
            .collect::<Vec<_>>();
        let interrupted_positions =
            lash_core::store::queued_work::select_interrupted_exact_claim_indices(
                &validation_batch_claims,
                batch_ids,
            )
            .map_err(|required_batch_ids| {
                StoreError::SelectedQueuedWorkRequiresInterruptedComposition { required_batch_ids }
            })?;
        let (mut selected, mut selected_batches) =
            if let Some(interrupted_positions) = interrupted_positions {
                let selected = interrupted_positions
                    .into_iter()
                    .map(|position| validation_rows[position].clone())
                    .collect::<Vec<_>>();
                let mut selected_batches = Vec::with_capacity(selected.len());
                for row in &selected {
                    selected_batches.push(queued_work_batch_from_row(&mut tx, row.clone()).await?);
                }
                (selected, selected_batches)
            } else {
                let mut requested_batches = std::collections::BTreeMap::new();
                for row in &requested_rows {
                    let batch = queued_work_batch_from_row(&mut tx, row.clone()).await?;
                    if batch.work_class() != Some(lash_core::store::QueuedWorkClass::TurnWork) {
                        tx.rollback().await.map_err(store_sqlx_error)?;
                        return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                            None,
                            already_satisfied_batch_ids,
                        ));
                    }
                    requested_batches.insert(row.batch_id.clone(), batch);
                }
                let span_rows = sqlx::query(
                    "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                            work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                            claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                            claim_owner_liveness_json, claim_token,
                            claim_session_lease_generation, claim_id
                     FROM lash_queued_work_batches
                     WHERE session_id = $1 AND available_at_ms <= $2
                       AND (claim_token IS NULL OR claim_session_lease_generation <> $3)
                       AND enqueue_seq BETWEEN $4 AND $5
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id)
                .bind(now as i64)
                .bind(sql_session_lease_generation(generation)?)
                .bind(requested_rows[0].enqueue_seq as i64)
                .bind(
                    requested_rows
                        .last()
                        .expect("requested rows exist")
                        .enqueue_seq as i64,
                )
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?
                .into_iter()
                .map(queued_batch_row)
                .collect::<Result<Vec<_>, _>>()?;
                let Some(first_position) = span_rows
                    .iter()
                    .position(|row| requested_ids.contains(&row.batch_id))
                else {
                    tx.rollback().await.map_err(store_sqlx_error)?;
                    return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                        None,
                        already_satisfied_batch_ids,
                    ));
                };
                let selected = span_rows[first_position..]
                    .iter()
                    .take_while(|row| requested_ids.contains(&row.batch_id))
                    .cloned()
                    .collect::<Vec<_>>();
                let selected_batches = selected
                    .iter()
                    .map(|row| {
                        requested_batches
                            .get(&row.batch_id)
                            .expect("contiguous exact row was validated")
                            .clone()
                    })
                    .collect::<Vec<_>>();
                (selected, selected_batches)
            };
        let candidates = selected
            .iter()
            .zip(selected_batches.iter())
            .map(|(row, batch)| claim_candidate_from_row(row, batch))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let selected_len = select_turn_work_claim_prefix(&candidates, boundary, policy, now)?;
        if selected_len == 0 {
            tx.rollback().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                None,
                already_satisfied_batch_ids,
            ));
        }
        selected.truncate(selected_len);
        selected_batches.truncate(selected_len);
        let lease =
            WorkClaimLease::derive_queued_work(&candidates[0], session_id, owner, now, generation)?;
        let sql_fencing_tokens = sql_claim_fencing_tokens(
            "queued_work_claim_fencing_token",
            candidates
                .iter()
                .map(|candidate| candidate.claim_fencing_token),
        )?;
        let liveness_json: Option<&str> = None;
        for (row, sql_fencing_token) in selected.iter().zip(sql_fencing_tokens.iter().copied()) {
            let changed = sqlx::query(
                "UPDATE lash_queued_work_batches
                 SET claim_id = $3, claim_owner_id = $4,
                     claim_owner_incarnation_id = $5, claim_owner_liveness_json = $6,
                     claim_token = $7, claim_fencing_token = $9,
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
            .bind(sql_session_lease_generation(generation)?)
            .bind(sql_fencing_token)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if changed != 1 {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                    None,
                    already_satisfied_batch_ids,
                ));
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
            Some(QueuedWorkClaim {
                session_id: session_id.to_string(),
                claim_id: lease.claim_id,
                owner: owner.clone(),
                lease_token: lease.lease_token,
                fencing_token: lease.fencing_token,
                session_lease_generation: lease.session_lease_generation,
                data: lash_core::store_backend_support::queued_work_claim_data(
                    selected_batches,
                    candidates[0].prior_claim_id.clone(),
                ),
            }),
            already_satisfied_batch_ids,
        ))
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE lash_queued_work_batches
             SET claim_id = $4,
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
        .bind(lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim))
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
            "UPDATE lash_queued_work_batches AS batch
             SET claim_id = abandoned.restore_claim_id,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_owner_liveness_json = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             FROM (",
        );
        query.push_tuples(claims, |mut row, claim| {
            row.push_bind(&claim.session_id)
                .push_bind(&claim.claim_id)
                .push_bind(&claim.lease_token)
                .push_bind(
                    lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim),
                );
        });
        query.push(
            ") AS abandoned(session_id, claim_id, claim_token, restore_claim_id)
             WHERE batch.session_id = abandoned.session_id
               AND batch.claim_id = abandoned.claim_id
               AND batch.claim_token = abandoned.claim_token",
        );
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
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
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
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
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
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
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
        let now = self.clock.timestamp_ms();
        let enqueue_seq: i64 = sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence(
                'lash_pending_turn_inputs',
                'enqueue_seq'
             ))",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let enqueue_seq_u64 = u64_from_sql("PendingTurnInput", "enqueue_seq", enqueue_seq)?;
        let input_id = draft.input_id.clone().unwrap_or_else(|| {
            lash_core::store_backend_support::derive_pending_turn_input_id(
                &draft.session_id,
                draft.source_key.as_deref(),
                now,
                enqueue_seq_u64,
            )
        });
        let state = match draft.ingress {
            lash_core::TurnInputIngress::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputIngress::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        let ingress_json = encode_json(&draft.ingress)?;
        let input_json = encode_json(&draft.input)?;
        let input = if let Some(source_key) = draft.source_key.as_deref() {
            let row = sqlx::query(
                "INSERT INTO lash_pending_turn_inputs (
                    enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json,
                    enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (session_id, source_key) DO UPDATE
                 SET source_key = lash_pending_turn_inputs.source_key
                 RETURNING enqueue_seq, input_id, session_id, source_key, ingress_json,
                           state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                           claim_owner_id, claim_owner_incarnation_id,
                           claim_owner_liveness_json, claim_token, claim_session_lease_generation",
            )
            .bind(enqueue_seq)
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
                    enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json,
                    enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(enqueue_seq)
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
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
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
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )
        .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
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
        let removed_node_count =
            sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1 AND tombstoned = TRUE")
                .bind(&self.session_id)
                .execute(&self.pool)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
        let removed_pending_turn_input_tombstone_count = sqlx::query(
            "DELETE FROM lash_pending_turn_inputs
             WHERE session_id = $1 AND state IN ($2, $3)",
        )
        .bind(&self.session_id)
        .bind(lash_core::TurnInputState::Cancelled.as_str())
        .bind(lash_core::TurnInputState::Completed.as_str())
        .execute(&self.pool)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
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
            // GC interprets only the root's ref graph, never component bodies.
            // Retain refs even when a newer writer used an unknown component
            // codec so an older binary cannot turn incompatibility into loss.
            for descriptor in manifest.components.values() {
                retained.insert(descriptor.blob_ref.0.clone());
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
                  AND state IN ('pending_active', 'accepted')
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
        .bind(sql_session_lease_generation(generation)?)
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
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    policy: QueuedWorkClaimPolicy,
) -> Result<ClaimTransactionOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if policy.max_rows == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(boundary))
        .bind(session_id)
        .bind(sql_session_lease_generation(generation)?)
        .bind(claim_scan_limit(policy.max_rows))
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
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let selected_len = select_turn_work_claim_prefix(&candidates, boundary, policy, now)?;
    if selected_len == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    selected.truncate(selected_len);
    selected_batches.truncate(selected_len);
    let lease =
        WorkClaimLease::derive_queued_work(&candidates[0], session_id, owner, now, generation)?;
    let sql_fencing_tokens = sql_claim_fencing_tokens(
        "queued_work_claim_fencing_token",
        candidates
            .iter()
            .take(selected_len)
            .map(|candidate| candidate.claim_fencing_token),
    )?;
    let liveness_json: Option<&str> = None;
    for (row, sql_fencing_token) in selected.iter().zip(sql_fencing_tokens.iter().copied()) {
        let changed = sqlx::query(
            "UPDATE lash_queued_work_batches
             SET claim_id = $3,
                 claim_owner_id = $4,
                 claim_owner_incarnation_id = $5,
                 claim_owner_liveness_json = $6,
                 claim_token = $7,
                 claim_fencing_token = $9,
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
        .bind(sql_session_lease_generation(
            lease.session_lease_generation,
        )?)
        .bind(sql_fencing_token)
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
        data: lash_core::store_backend_support::queued_work_claim_data(
            selected_batches,
            candidates[0].prior_claim_id.clone(),
        ),
    })))
}

#[allow(clippy::too_many_arguments)]
async fn claim_pending_turn_inputs_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<ClaimTransactionOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let active_turn = matches!(mode, lash_core::TurnInputClaimMode::ActiveTurn { .. });
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
        .push(" AND (state = ")
        .push_bind(wanted_state.as_str())
        .push(" OR (")
        .push_bind(active_turn)
        .push(" AND state = 'accepted'))")
        .push(
            "
           AND (
                claim_token IS NULL
                OR claim_session_lease_generation <> ",
        )
        .push_bind(sql_session_lease_generation(generation)?)
        .push("\n           )");
    if let lash_core::TurnInputClaimMode::ActiveTurn {
        turn_id,
        checkpoint,
    } = &mode
    {
        query
            .push(" AND ingress_json::jsonb ->> 'scope' = 'active_turn'")
            .push(" AND ingress_json::jsonb ->> 'turn_id' = ")
            .push_bind(turn_id.as_str());
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
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation)?;
    let sql_fencing_tokens = sql_claim_fencing_tokens(
        "turn_input_claim_fencing_token",
        selected.iter().map(|(row, _)| row.claim_fencing_token),
    )?;
    let liveness_json: Option<&str> = None;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for ((row, mut input), sql_fencing_token) in selected.into_iter().zip(sql_fencing_tokens) {
        let changed = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = $3,
                 claim_id = $4,
                 claim_owner_id = $5,
                 claim_owner_incarnation_id = $6,
                 claim_owner_liveness_json = $7,
                 claim_token = $8,
                 claim_fencing_token = $10,
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
        .bind(sql_session_lease_generation(
            lease.session_lease_generation,
        )?)
        .bind(sql_fencing_token)
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
            data: lash_core::runtime::TurnInputClaimData {
                mode,
                inputs,
                applications: Vec::new(),
            },
        },
    )))
}

async fn claim_pending_turn_inputs_postgres(
    pool: &PgPool,
    session_id: &str,
    session_execution_lease: &SessionExecutionLeaseAuthority,
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
    let active_turn = matches!(mode, lash_core::TurnInputClaimMode::ActiveTurn { .. });
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
        .push(" AND (state = ")
        .push_bind(wanted_state.as_str())
        .push(" OR (")
        .push_bind(active_turn)
        .push(" AND state = 'accepted'))")
        .push(
            "
           AND (
                claim_token IS NULL
                OR claim_session_lease_generation <> ",
        )
        .push_bind(sql_session_lease_generation(generation)?)
        .push("\n           )");
    if let lash_core::TurnInputClaimMode::ActiveTurn {
        turn_id,
        checkpoint,
    } = &mode
    {
        query
            .push(" AND ingress_json::jsonb ->> 'scope' = 'active_turn'")
            .push(" AND ingress_json::jsonb ->> 'turn_id' = ")
            .push_bind(turn_id.as_str());
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
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation)?;
    let sql_fencing_tokens = sql_claim_fencing_tokens(
        "turn_input_claim_fencing_token",
        selected.iter().map(|(row, _)| row.claim_fencing_token),
    )?;
    let liveness_json: Option<&str> = None;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for ((row, mut input), sql_fencing_token) in selected.into_iter().zip(sql_fencing_tokens) {
        let changed = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = $3,
                 claim_id = $4,
                 claim_owner_id = $5,
                 claim_owner_incarnation_id = $6,
                 claim_owner_liveness_json = $7,
                 claim_token = $8,
                 claim_fencing_token = $10,
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
        .bind(sql_session_lease_generation(
            lease.session_lease_generation,
        )?)
        .bind(sql_fencing_token)
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
        data: lash_core::runtime::TurnInputClaimData {
            mode,
            inputs,
            applications: Vec::new(),
        },
    }))
}

pub(crate) struct SessionExecutionLeaseRow {
    owner: Option<LeaseOwnerIdentity>,
    executor_id: Option<String>,
    pub(crate) lease_token: Option<String>,
    pub(crate) fencing_token: u64,
    claimed_at_ms: u64,
    pub(crate) expires_at_ms: u64,
}

/// Read the lease row without locking it, for diagnostics.
///
/// The mutation paths deliberately take a `FOR UPDATE` row lock (see
/// [`load_session_execution_lease_tx`]) because check-then-act on this row is not
/// atomic under READ COMMITTED. A diagnostic read must never take that lock: an
/// operator polling a stuck session would otherwise make the holder's renewal or
/// a peer's claim wait behind the observer's transaction, so watching the lease
/// could itself delay the lane it is watching. This runs as a single autocommit
/// statement on the pool with no `FOR UPDATE` and no surrounding transaction.
pub(crate) async fn read_session_execution_lease_unlocked(
    pool: &PgPool,
    session_id: &str,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_owner_liveness_json,
                lease_executor_id
         FROM lash_session_execution_leases
         WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .map_err(store_sqlx_error)?;
    row.map(session_execution_lease_row_from_columns)
        .transpose()
}

pub(crate) async fn load_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_owner_liveness_json,
                lease_executor_id
         FROM lash_session_execution_leases
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(session_execution_lease_row_from_columns)
        .transpose()
}

fn session_execution_lease_row_from_columns(
    row: sqlx::postgres::PgRow,
) -> Result<SessionExecutionLeaseRow, StoreError> {
    Ok(SessionExecutionLeaseRow {
        owner: lease_owner_from_columns(row.get(0), row.get(5), row.get(6)),
        executor_id: row.get(7),
        lease_token: row.get(1),
        fencing_token: u64_from_sql("SessionExecutionLease", "fencing_token", row.get(2))?,
        claimed_at_ms: u64_from_sql("SessionExecutionLease", "claimed_at_ms", row.get(3))?,
        expires_at_ms: u64_from_sql("SessionExecutionLease", "expires_at_ms", row.get(4))?,
    })
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
        executor_id: row.executor_id.ok_or_else(|| {
            StoreError::Backend("live session lease missing executor id".to_string())
        })?,
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
    claim: lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity<'_>,
    previous_fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> Result<SessionExecutionLease, StoreError> {
    let lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity {
        session_id,
        owner,
        executor_id,
        lease_token,
    } = claim;
    let fencing_token = StoreError::checked_monotonic_increment(
        "session_execution_lease_fencing_token",
        previous_fencing_token,
    )?;
    let sql_fencing_token = sql_monotonic_counter_value(
        "session_execution_lease_fencing_token",
        previous_fencing_token,
        fencing_token,
    )?;
    let expires_at = now.saturating_add(lease_ttl_ms);
    let sql_expires_at = sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
    sqlx::query(
        "INSERT INTO lash_session_execution_leases (
            session_id, lease_owner_id, lease_owner_incarnation_id, lease_executor_id,
            lease_owner_liveness_json, lease_token, lease_fencing_token,
            lease_claimed_at_ms, lease_expires_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (session_id) DO UPDATE SET
            lease_owner_id = EXCLUDED.lease_owner_id,
            lease_owner_incarnation_id = EXCLUDED.lease_owner_incarnation_id,
            lease_executor_id = EXCLUDED.lease_executor_id,
            lease_owner_liveness_json = EXCLUDED.lease_owner_liveness_json,
            lease_token = EXCLUDED.lease_token,
            lease_fencing_token = EXCLUDED.lease_fencing_token,
            lease_claimed_at_ms = EXCLUDED.lease_claimed_at_ms,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms",
    )
    .bind(session_id)
    .bind(&owner.owner_id)
    .bind(&owner.incarnation_id)
    .bind(executor_id)
    .bind(Option::<&str>::None)
    .bind(lease_token)
    .bind(sql_fencing_token)
    .bind(now as i64)
    .bind(sql_expires_at)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(SessionExecutionLease {
        session_id: session_id.to_string(),
        owner: owner.clone(),
        executor_id: executor_id.to_string(),
        lease_token: lease_token.to_string(),
        fencing_token,
        claimed_at_epoch_ms: now,
        expires_at_epoch_ms: expires_at,
    })
}

async fn ensure_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    fence: &SessionExecutionLeaseAuthority,
) -> Result<(), StoreError> {
    let now = postgres_transaction_epoch_ms(tx).await?;
    let current = load_session_execution_lease_tx(tx, session_id).await?;
    lash_core::store_backend_support::require_current_session_execution_lease(
        session_id,
        current.as_ref().map(|current| {
            lash_core::store_backend_support::SessionExecutionLeaseFenceFacts {
                owner: current.owner.as_ref(),
                executor_id: current.executor_id.as_deref(),
                lease_token: current.lease_token.as_deref(),
                fencing_token: current.fencing_token,
                expires_at_epoch_ms: current.expires_at_ms,
            }
        }),
        fence,
        now,
    )
}

async fn release_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completion: &SessionExecutionLeaseAuthority,
) -> Result<bool, StoreError> {
    let released = sqlx::query(
        "UPDATE lash_session_execution_leases
         SET lease_owner_id = NULL,
             lease_owner_incarnation_id = NULL,
             lease_executor_id = NULL,
             lease_owner_liveness_json = NULL,
             lease_token = NULL,
             lease_claimed_at_ms = 0,
             lease_expires_at_ms = 0
         WHERE session_id = $1
           AND lease_owner_id = $2
           AND lease_owner_incarnation_id = $3
           AND lease_executor_id = $4
           AND lease_token = $5",
    )
    .bind(&completion.session_id)
    .bind(&completion.owner.owner_id)
    .bind(&completion.owner.incarnation_id)
    .bind(&completion.executor_id)
    .bind(&completion.lease_token)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(released.rows_affected() == 1)
}
