use crate::*;
use lash_core::store::queued_work::{TurnWorkClaimPrefix, TurnWorkEmptyScanDiagnostic};
use lash_sansio::TurnId;

pub(crate) const LOAD_TURN_FAILURE_SETTLEMENTS_SQL: &str = "SELECT turn_id, result_json
     FROM lash_runtime_turn_commits
     WHERE session_id = $1
       AND result_json LIKE '%\"failure_evidence\"%'
     ORDER BY committed_at_ms, turn_id";

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

pub(crate) async fn lock_session_history_mutations_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_ids: &[String],
) -> Result<(), StoreError> {
    if session_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(ordered.session_id, 1::BIGINT))
         FROM (
             SELECT DISTINCT session_id
             FROM unnest($1::TEXT[]) AS target(session_id)
             ORDER BY session_id
         ) AS ordered",
    )
    .bind(session_ids)
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

#[cfg(any(test, feature = "testing"))]
macro_rules! transaction_epoch_sql {
    () => { "COALESCE(NULLIF(current_setting('lash.test_lease_epoch_ms', true), '')::bigint, FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000))" };
}
#[cfg(not(any(test, feature = "testing")))]
macro_rules! transaction_epoch_sql {
    () => {
        "FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()) * 1000)"
    };
}

const PENDING_TURN_INPUT_COLUMNS: &str = "enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json, enqueued_at_ms, claim_id, claim_fencing_token, claim_owner_id, claim_owner_incarnation_id, claim_token, claim_session_lease_generation";

const POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE: &str = concat!(
    "session_id = $1
       AND available_at_ms <= ",
    transaction_epoch_sql!(),
    "
       AND (
            claim_token IS NULL
            OR claim_session_lease_generation <> $2
       )"
);

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
    let epoch_ms = transaction_epoch_sql!();
    let earliest_safe_boundary = DeliveryPolicy::EarliestSafeBoundary.as_str();
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
                  AND candidate.available_at_ms <= {epoch_ms}
                  AND (
                       candidate.claim_token IS NULL
                       OR candidate.claim_session_lease_generation <> $2
                  )
                  AND (
                       (
                            candidate.enqueue_seq = unfiltered.head_enqueue_seq
                            AND unfiltered.head_delivery_policy = '{earliest_safe_boundary}'
                       )
                       OR (
                            unfiltered.head_delivery_policy <> '{earliest_safe_boundary}'
                            AND unfiltered.head_claim_id IS NOT NULL
                            AND candidate.claim_id IS DISTINCT FROM unfiltered.head_claim_id
                       )
                  )
                ORDER BY candidate.enqueue_seq ASC
                LIMIT 1
            ) AS boundary_head
            WHERE head_delivery_policy = '{earliest_safe_boundary}'
         )"
    )
}

fn postgres_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> String {
    let head_candidate = postgres_queued_work_head_candidate_cte(boundary);
    format!(
        "WITH {head_candidate}
         SELECT {QUEUED_WORK_COLUMNS}
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
         FOR UPDATE OF lash_queued_work_batches SKIP LOCKED",
        QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
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
    let inserted_id: Option<String> = sqlx::query_scalar(
        "INSERT INTO lash_queued_work_batches (
            enqueue_seq, batch_id, session_id, source_key, delivery_policy, work_kind,
            authority_json, merge_key, available_at_ms, enqueued_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (session_id, source_key) DO NOTHING
         RETURNING batch_id",
    )
    .bind(enqueue_seq)
    .bind(&batch_id)
    .bind(&batch.session_id)
    .bind(&batch.source_key)
    .bind(batch.delivery_policy.as_str())
    .bind(batch.kind().as_str())
    .bind(encode_json(&batch.authority)?)
    .bind(&batch.merge_key)
    .bind(sql_available_at_ms)
    .bind(now as i64)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(inserted_id) = inserted_id else {
        let source_key = batch.source_key.as_deref().ok_or_else(|| {
            StoreError::Backend("queued work insert without source key was ignored".to_string())
        })?;
        let existing_id: Option<String> = sqlx::query_scalar(
            "SELECT batch_id FROM lash_queued_work_batches
             WHERE session_id = $1 AND source_key = $2",
        )
        .bind(&batch.session_id)
        .bind(source_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let existing_id = existing_id.ok_or_else(|| {
            StoreError::Backend("queued work conflict row disappeared".to_string())
        })?;
        let existing = load_queued_batch(tx, &existing_id)
            .await?
            .ok_or_else(|| StoreError::Backend("queued work source row disappeared".to_string()))?;
        return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
    };
    debug_assert_eq!(inserted_id, batch_id);
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

async fn read_session_state_version_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    lock: bool,
) -> Result<u32, StoreError> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let marker: Option<Option<i32>> = sqlx::query_scalar(&format!(
        "SELECT session_state_version FROM lash_session_meta WHERE session_id = $1{suffix}"
    ))
    .bind(session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(marker) = marker else {
        return Ok(lash_core::store::CURRENT_SESSION_STATE_VERSION);
    };
    let marker = marker
        .map(|version| {
            u32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("marker {version} is outside the unsigned 32-bit domain"),
            })
        })
        .transpose()?;
    lash_core::store::resolve_session_state_version(marker)
}

mod claim_support;
mod commit_claims;
mod maintenance;
mod queued_work;
#[cfg(test)]
mod refusal_probe_tests;
mod session_commit;
mod session_execution_lease;
mod turn_input;

use claim_support::*;
pub(crate) use claim_support::{
    load_session_execution_lease_tx, read_session_execution_lease_unlocked,
};
use commit_claims::complete_queued_work_claims_tx;
pub(crate) use commit_claims::complete_turn_input_claims_tx;
