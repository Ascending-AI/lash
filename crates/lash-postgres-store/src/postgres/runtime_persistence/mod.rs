use crate::session_sql::session_sql;
use crate::*;
use lash_core::store::queued_work::{TurnWorkClaimPrefix, TurnWorkEmptyScanDiagnostic};
use lash_sansio::SessionId;
use lash_sansio::TurnId;

pub(crate) async fn lock_session_history_mutation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_session_history
            .sql(),
    )
    .bind(session_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

pub(crate) async fn lock_session_history_mutations_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_ids: &[SessionId],
) -> Result<(), StoreError> {
    if session_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_session_history_batch
            .sql(),
    )
    .bind(
        session_ids
            .iter()
            .map(SessionId::as_str)
            .collect::<Vec<_>>(),
    )
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

pub(crate) async fn ensure_session_not_deleted_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    lock_session_history_mutation_tx(tx, session_id).await?;
    let deleted = sqlx::query_scalar::<_, bool>(session_sql().deleted_postgres.exists.sql())
        .bind(session_id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: SessionId::from(session_id.to_string()),
        })
    } else {
        Ok(())
    }
}

/// The claim-candidate scan for `boundary`, rendered once at startup.
///
/// The boundary is a closed two-variant choice, so it selects a named statement
/// rather than splicing a predicate: an optional boundary filter cannot use
/// `idx_queued_work_batches_ready`, and this query is the claim path's hottest.
fn postgres_queued_work_claim_candidates_sql(boundary: QueuedWorkClaimBoundary) -> &'static str {
    let sql = crate::turn_ingress::turn_ingress_sql();
    match boundary {
        QueuedWorkClaimBoundary::Idle => sql.queued_batches_postgres.claim_candidates_idle.sql(),
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint => {
            sql.queued_batches_postgres.claim_candidates_boundary.sql()
        }
    }
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
            session_sql().graph_postgres.select_parent_for_update.sql(),
        )
        .bind(&node_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let Some(parent_node_id) = parent_node_id else {
            return Ok(());
        };
        let reachable =
            sqlx::query_scalar::<_, bool>(session_sql().graph_postgres.exists_reachable.sql())
                .bind(&node_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        if reachable {
            return Ok(());
        }
        sqlx::query(session_sql().graph_postgres.retire.sql())
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
    sqlx::query_scalar(session_sql().graph_postgres.select_frame_node_id.sql())
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
        sqlx::query_scalar::<_, i64>(crate::process_sql::process_sql().fence.select_floor.sql())
            .bind(batch.session_id.as_str())
            .bind(wake_source.process_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
    } else {
        None
    };
    let enqueue_seq: i64 = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches_postgres
            .select_next_enqueue_seq
            .sql(),
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
    let sql = crate::turn_ingress::turn_ingress_sql();
    let inserted_id: Option<String> =
        sqlx::query_scalar(sql.queued_batches_postgres.insert_new.sql())
            .bind(enqueue_seq)
            .bind(&batch_id)
            .bind(batch.session_id.as_str())
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
        let existing_id: Option<String> =
            sqlx::query_scalar(sql.queued_batches.select_id_by_source_key.sql())
                .bind(batch.session_id.as_str())
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
        sqlx::query(sql.queued_items.insert_new.sql())
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
    session_id: &SessionId,
    source_key: &str,
) -> Result<(), StoreError> {
    // `PostgresStorage::from_pool` accepts externally configured pools, so
    // bound this correctness lock locally even when no connection-wide
    // `lock_timeout` was installed. SQLSTATE 55P03 maps to `Contended`.
    sqlx::query(
        crate::connection_sql::connection_sql()
            .clamp_lock_timeout
            .sql(),
    )
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text_pair
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(source_key)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

async fn read_session_state_version_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    lock: bool,
) -> Result<u32, StoreError> {
    // One statement per filter shape, not a suffix appended per call: the
    // locked read is a different statement from the unlocked one.
    let statement = if lock {
        session_sql()
            .meta_postgres
            .select_state_version_for_update
            .sql()
    } else {
        session_sql().meta.select_state_version.sql()
    };
    let marker: Option<Option<i32>> = sqlx::query_scalar(statement)
        .bind(session_id.as_str())
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
