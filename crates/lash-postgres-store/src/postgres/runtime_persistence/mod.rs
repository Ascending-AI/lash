use crate::session_sql::session_sql;
use crate::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

pub(crate) async fn allocate_ingress_sequence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<i64, StoreError> {
    lock_session_history_mutation_tx(tx, session_id).await?;
    sqlx::query_scalar(
        crate::session_ingress::session_ingress_sql()
            .allocate_sequence
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

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

/// Refuse a write into session `session_id` once its close began: the
/// `CloseSession` intent is the point of no return of its deletion, so it
/// accepts nothing more (FIG-3600 S7). The caller holds the session's
/// history-mutation lock, which the close takes too.
pub(crate) async fn ensure_session_not_closing_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let closing: Option<Option<i64>> =
        sqlx::query_scalar(session_sql().meta.select_closing_intent.sql())
            .bind(session_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    match closing.flatten() {
        None => Ok(()),
        Some(intent) => Err(StoreError::SessionClosing {
            session_id: session_id.clone(),
            intent: lash_core_execution::store::ControlIntentId::from_sequence(
                crate::support::u64_from_sql("SessionMeta", "closing_intent", intent)?,
            ),
        }),
    }
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
            session_id: SessionId::parse(session_id.to_string())?,
        })
    } else {
        Ok(())
    }
}

/// Reclaim the ancestry prefix with no live child, session-head root, or
/// explicit anchor. Every writer that adds an edge or run locks the target
/// node first, so the reachability query runs from a fresh snapshot after
/// concurrent additions have either committed or failed.
/// Carries one complete child/head/anchor check and the node lock it ran under.
struct RetirableAncestryNode {
    node_id: String,
    parent_node_id: Option<String>,
}

async fn retirable_ancestry_node_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node_id: &str,
) -> Result<Option<RetirableAncestryNode>, StoreError> {
    let parent = sqlx::query_scalar::<_, Option<String>>(
        session_sql().graph_postgres.select_parent_for_update.sql(),
    )
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(parent_node_id) = parent else {
        return Ok(None);
    };
    let reachable =
        sqlx::query_scalar::<_, bool>(session_sql().graph_postgres.exists_reachable.sql())
            .bind(node_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    if reachable {
        return Ok(None);
    }
    Ok(Some(RetirableAncestryNode {
        node_id: node_id.to_owned(),
        parent_node_id,
    }))
}

async fn retire_ancestry_node_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    witness: RetirableAncestryNode,
) -> Result<Option<String>, StoreError> {
    sqlx::query(session_sql().graph_postgres.retire.sql())
        .bind(&witness.node_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(witness.parent_node_id)
}

pub(crate) async fn retire_unreachable_ancestry_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    first_node_id: &str,
) -> Result<(), StoreError> {
    let mut node_id = first_node_id.to_string();
    loop {
        let Some(witness) = retirable_ancestry_node_tx(tx, &node_id).await? else {
            return Ok(());
        };
        let Some(parent) = retire_ancestry_node_tx(tx, witness).await? else {
            return Ok(());
        };
        node_id = parent;
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
    lash_core_execution::store_backend_support::validate_queued_work_draft(batch)?;
    let claim = lash_core_execution::ReferrerClaim::unguarded(
        lash_core_execution::ArtifactReferrer::Session(batch.session_id.clone()),
    )
    .map_err(|error| error.into_store_error("queued attachment referrer"))?;
    crate::artifact_store::lock_referrer_tx(tx, &claim.referrer())
        .await
        .map_err(store_sqlx_error)?;
    crate::attachments::acquire_attachment_refs_tx(tx, &claim, &batch.stored_attachment_ids(), now)
        .await?;
    use lash_core_execution::store_backend_support as support;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let submission_digest = support::queued_work_submission_digest(batch)?;
    if batch.process_wake_source.is_some()
        && let Some(source_key) = batch.source_key.as_deref()
    {
        lock_process_wake_source_tx(tx, &batch.session_id, source_key).await?;
    }
    // The session's write authority, taken before the source-key read and
    // held to the commit, so the absence it answers holds until the insert.
    lock_session_history_mutation_tx(tx, &batch.session_id).await?;
    if let Some(source_key) = batch.source_key.as_deref() {
        let by_source_key: Option<(String, String)> =
            sqlx::query_as(sql.queued_batches.select_id_by_source_key.sql())
                .bind(batch.session_id.as_str())
                .bind(source_key)
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        // A changed process wake's refusal is that wake's terminal: the
        // fence rises here and the caller commits it with the refusal
        // (FIG-4487).
        let admission = match support::decide_queued_work_draft_admission(
            batch,
            &submission_digest,
            by_source_key,
        ) {
            Ok(admission) => admission,
            Err(refusal) => {
                if let Some(wake) = support::conflicting_process_wake(batch, &refusal) {
                    raise_wake_redelivery_fence_tx(tx, &batch.session_id, &wake).await?;
                }
                return Err(refusal);
            }
        };
        if let support::QueuedWorkDraftAdmission::Existing { batch_id } = admission {
            let existing = load_queued_batch(tx, batch_id.as_str())
                .await?
                .ok_or_else(|| {
                    StoreError::Backend("queued work source row disappeared".to_string())
                })?;
            return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
        }
    }
    if let Some(wake_source) = batch.process_wake_source.as_ref() {
        let allocation_floor = sqlx::query_scalar::<_, i64>(
            crate::process_sql::process_sql().fence.select_floor.sql(),
        )
        .bind(batch.session_id.as_str())
        .bind(wake_source.process_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .map(|value| u64_from_sql("WakeAllocationFloor", "allocation_floor", value))
        .transpose()?;
        if let Some(allocation_floor) = allocation_floor
            && wake_source.sequence <= allocation_floor
        {
            return Err(StoreError::ProcessWakeSequenceRewound {
                session_id: batch.session_id.clone(),
                process_id: wake_source.process_id.clone(),
                sequence: wake_source.sequence,
                allocation_floor,
            });
        }
    }
    let enqueue_seq = allocate_ingress_sequence_tx(tx, &batch.session_id).await?;
    let enqueue_seq_u64 = u64_from_sql("QueuedWorkBatch", "enqueue_seq", enqueue_seq)?;
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(enqueue_seq_u64),
    );
    sqlx::query(sql.queued_batches_postgres.insert_new.sql())
        .bind(enqueue_seq)
        .bind(&batch_id)
        .bind(batch.session_id.as_str())
        .bind(&batch.source_key)
        .bind(batch.delivery_policy.as_str())
        .bind(batch.kind().as_str())
        .bind(encode_json(&batch.authority)?)
        .bind(&batch.merge_key)
        .bind(now as i64)
        .bind(submission_digest.as_str())
        .bind(encode_json(&batch.payload)?)
        .bind(lash_core_execution::store_backend_support::encode_trace_cause(&batch.trace_cause)?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    // The admitted batch owes its session a shift (ADR 0109 §3), armed in
    // the transaction that admits it.
    crate::ingress_obligation::arm_queued_batch_tx(tx, &batch.session_id, &batch_id, now).await?;
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

/// Raise session `session_id`'s redelivery fence to `max(floor, sequence)`
/// for a wake whose row is leaving the queue in this transaction.
///
/// The one home of the invariant that every terminal transition of a wake —
/// settlement by its run, host cancel and a content conflict's refusal —
/// raises the floor with the row's removal or the refusal (FIG-1065,
/// FIG-3545, FIG-4487). The wake source's advisory lock serializes
/// the fence against a concurrent enqueue of the same source, which takes
/// the same lock before it reads the floor. Callers write the fence before
/// the delete.
pub(crate) async fn raise_wake_redelivery_fence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    wake: &lash_core_execution::store::TerminalProcessWake,
) -> Result<(), StoreError> {
    // A validated wake batch always names its source key, so the advisory
    // lock is always taken; the `None` arm is a corrupt-row path that still
    // writes the fence it can.
    if let Some(source_key) = wake.source_key.as_deref() {
        lock_process_wake_source_tx(tx, session_id, source_key).await?;
    }
    sqlx::query(
        crate::process_sql::process_sql()
            .fence_postgres
            .upsert_max
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(wake.process_id.as_str())
    .bind(sql_counter_value("wake_allocation_floor", wake.sequence)?)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

async fn read_session_state_version_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    lock: bool,
    fleet: lash_core_execution::FleetFormat,
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
        return Ok(lash_core_execution::store::CURRENT_SESSION_STATE_VERSION);
    };
    let marker = marker
        .map(|version| {
            u32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("marker {version} is outside the unsigned 32-bit domain"),
            })
        })
        .transpose()?;
    lash_core_execution::store::resolve_session_state_version(marker, fleet)
}

mod admission;
pub(crate) mod shift_admission;
pub(crate) use admission::{
    admit_at_checkpoint_postgres, admit_run_postgres, open_session_command_run_postgres,
};
mod history;
pub(crate) mod shift_epoch;
pub(crate) use history::read_tx;
mod ingress_settlement;
mod maintenance;
mod queued_work;
mod session_commit;
pub(crate) mod turn_cancel;
mod turn_input;
pub(crate) mod turn_park;
pub(crate) mod turn_park_feed;

use turn_cancel::*;
