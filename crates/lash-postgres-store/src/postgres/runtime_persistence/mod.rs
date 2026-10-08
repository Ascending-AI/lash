use crate::session_sql::session_sql;
use crate::*;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

pub(crate) async fn allocate_ingress_sequence_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
) -> Result<i64, StoreError> {
    lock_session_history_mutation_tx(tx, session_id).await?;
    sqlx::query_scalar(
        crate::session_ingress::session_ingress_sql()
            .allocate_sequence
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)
}

pub(crate) async fn lock_session_history_mutation_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_session_history
            .sql(),
    )
    .bind(session_id.as_str())
    .execute(crate::observed_sql::executor(&mut *tx))
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
    .execute(crate::observed_sql::executor(&mut **tx))
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
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
        .fetch_one(crate::observed_sql::executor(&mut **tx))
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
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?;
    let Some(parent_node_id) = parent else {
        return Ok(None);
    };
    let reachable =
        sqlx::query_scalar::<_, bool>(session_sql().graph_postgres.exists_reachable.sql())
            .bind(node_id)
            .fetch_one(crate::observed_sql::executor(&mut **tx))
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
        .execute(crate::observed_sql::executor(&mut **tx))
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
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)
}

async fn enqueue_queued_work_tx(
    tx: &mut sqlx::PgConnection,
    batch: &QueuedWorkBatchDraft,
    now: u64,
) -> Result<QueuedWorkBatch, StoreError> {
    enqueue_queued_work_with_outcome_tx(tx, batch, now)
        .await
        .map(QueuedWorkEnqueueOutcome::into_batch)
}

pub(crate) async fn enqueue_queued_work_with_outcome_tx(
    tx: &mut sqlx::PgConnection,
    batch: &QueuedWorkBatchDraft,
    now: u64,
) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
    lash_core_execution::store_backend_support::validate_queued_work_draft(batch)?;
    crate::PostgresDurableStore::lock_session_admission(tx, &batch.session_id).await?;
    // Owner commits take history before referrers, too. An admission from
    // another domain's transaction must preserve the same order.
    lock_session_history_mutation_tx(tx, &batch.session_id).await?;
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
    // The session's write authority is held from before the source-key
    // read until commit, so the absence it answers holds until the insert.
    if let Some(source_key) = batch.source_key.as_deref() {
        let by_source_key: Option<(String, String)> =
            sqlx::query_as(sql.queued_batches.select_id_by_source_key.sql())
                .bind(batch.session_id.as_str())
                .bind(source_key)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
        let admission =
            support::decide_queued_work_draft_admission(batch, &submission_digest, by_source_key)?;
        if let support::QueuedWorkDraftAdmission::Existing { batch_id } = admission {
            let existing = load_queued_batch(tx, batch_id.as_str())
                .await?
                .ok_or_else(|| {
                    StoreError::Backend("queued work source row disappeared".to_string())
                })?;
            return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
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
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
    // The batch and the session's wake commit together (ADR 0132 §12).
    crate::durable::wake_session_tx(tx, &batch.session_id, false, now).await?;
    let queued = load_queued_batch(tx, &batch_id)
        .await?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    debug_assert_eq!(queued.enqueue_seq, enqueue_seq_u64);
    Ok(QueuedWorkEnqueueOutcome::Inserted(queued))
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
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
pub(crate) use admission::{admit_at_checkpoint_postgres, open_session_command_run_postgres};
mod history;
mod session_fault;
pub(crate) use history::read_tx;
mod ingress_settlement;
mod maintenance;
mod queued_work;
mod session_commit;
pub(crate) use session_commit::apply_runtime_commit_tx;
pub(crate) mod turn_cancel;
mod turn_input;
pub(crate) use turn_input::enqueue_pending_turn_inputs_tx;

use turn_cancel::*;
