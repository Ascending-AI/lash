//! Queued-work row model: the durable representation of queued batches and
//! their single payloads, mirroring the SQLite backend's `queued_work` module.
//! Originated in `session_factory.rs`; every item keeps its previous path
//! through the crate-root glob.

use crate::*;
use lash_core_execution::runtime::QueuedWorkPayload;

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    session_id: SessionId,
    source_key: Option<String>,
    pub(crate) delivery_policy: DeliveryPolicy,
    pub(crate) payload: QueuedWorkPayload,
    pub(crate) authority: QueuedWorkAuthority,
    pub(crate) merge_key: Option<String>,
    enqueued_at_ms: u64,
    submission_digest: String,
    terminal: Option<lash_core_execution::store::IngressTerminal>,
    trace_cause: lash_core_execution::TraceCause,
}

/// The turn-lane candidate `batch` offers an admission.
pub(crate) fn turn_lane_candidate(batch: &QueuedWorkBatch) -> TurnLaneCandidate {
    TurnLaneCandidate::from_batch(batch)
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>("delivery_policy").as_str())
            .ok_or_else(|| StoreError::StoredDataCorrupt {
                record_kind: "QueuedWorkBatch",
                message: "unknown queued-work delivery policy".to_string(),
            })?;
    let payload: QueuedWorkPayload = store_decode_json(
        row.get::<String, _>("payload_json").as_str(),
        "queued work payload",
    )?;
    let authority_json: String = row.get("authority_json");
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql("QueuedWorkBatch", "enqueue_seq", row.get("enqueue_seq"))?,
        batch_id: row.get("batch_id"),
        session_id: SessionId::parse(row.get::<String, _>("session_id"))?,
        source_key: row.get("source_key"),
        delivery_policy,
        payload,
        authority: store_decode_json(&authority_json, "queued work authority")?,
        merge_key: row.get("merge_key"),
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        submission_digest: row.get("submission_digest"),
        terminal: lash_core_execution::store_backend_support::decode_ingress_terminal(
            "QueuedWorkBatch",
            row.get::<Option<String>, _>("terminal_cause").as_deref(),
            row.get::<Option<i64>, _>("terminal_at_ms")
                .map(|at| u64_from_sql("QueuedWorkBatch", "terminal_at_ms", at))
                .transpose()?,
        )?,
        trace_cause: lash_core_execution::store_backend_support::decode_trace_cause(
            "QueuedWorkBatch",
            row.get::<Option<String>, _>("trace_cause_json").as_deref(),
        )?,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::PgConnection,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .select_by_id
            .sql(),
    )
    .bind(batch_id)
    .fetch_optional(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let row = queued_batch_row(row)?;
    queued_work_batch_from_row(row).map(Some)
}

pub(crate) fn queued_work_batch_from_row(
    row: QueuedBatchRow,
) -> Result<QueuedWorkBatch, StoreError> {
    let batch = QueuedWorkBatch {
        batch_id: row.batch_id.try_into()?,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        delivery_policy: row.delivery_policy,
        authority: row.authority,
        merge_key: row.merge_key,
        enqueued_at_ms: row.enqueued_at_ms,
        payload: row.payload,
        submission_digest: row.submission_digest,
        terminal: row.terminal,
        trace_cause: row.trace_cause,
    };
    Ok(batch)
}

/// Settle batch `batch_id` of session `session_id`, which run `run` must
/// hold, in the commit that completed it (FIG-3927).
///
/// The verdict is taken over the row under `FOR UPDATE`; then the consumed
/// wake's redelivery fence lands, under the wake source's advisory lock that
/// serializes queue insertion against consumption, before the row leaves. A
/// crash between the two would replay a wake the session already consumed,
/// so the fence must land first.
pub(crate) async fn complete_admitted_batch_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    run: &lash_core_execution::TurnId,
    batch_id: &lash_core_execution::BatchId,
    terminal: lash_core_execution::store::IngressTerminal,
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let row = lash_core_execution::store::IngressRowId::Batch(batch_id.clone());
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &row,
        observed.as_ref().map(Option::as_deref),
    )?;
    let settled = sqlx::query(sql.queued_batches.settle_admitted.sql())
        .bind(session_id.as_str())
        .bind(batch_id.as_str())
        .bind(run.as_str())
        .bind(terminal.cause.as_str())
        .bind(crate::support::clamp_epoch_ms(terminal.at_ms))
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    // Backstop: the verdict above was taken under the row lock, so the run
    // predicate cannot legitimately miss.
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        batch_id.as_str(),
        settled,
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            run: run.clone(),
            row: Box::new(row.clone()),
            admitted_run: None,
        },
    )
}

/// Settle open session command `batch_id` of session `session_id`, in the
/// commit that applied it (FIG-3927): the command lane takes no admission,
/// so the predicate is the row's presence and openness. A command withdrawn
/// since the shift read it refuses the whole commit.
pub(crate) async fn settle_open_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    commit: &lash_core_execution::store::RuntimeCommit,
    batch_id: &lash_core_execution::BatchId,
    at_ms: u64,
) -> Result<(), StoreError> {
    let session_id = &commit.session_id;
    let operation_key = commit.turn_commit.operation.storage_key()?;
    let cause = match commit.command_outcomes.get(batch_id) {
        Some(lash_core_execution::runtime::SessionCommandOutcome::ConfigTransaction {
            outcome: lash_core_execution::ConfigTransactionOutcome::Stale { .. },
        }) => lash_core_execution::store::IngressTerminalCause::StaleConfigRevision,
        _ => lash_core_execution::store::IngressTerminalCause::Applied,
    };
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_open_command(
        session_id,
        batch_id,
        observed.as_ref().map(Option::as_deref),
    )?;
    let settled = sqlx::query(sql.queued_batches.settle_command.sql())
        .bind(session_id.as_str())
        .bind(batch_id.as_str())
        .bind(crate::support::clamp_epoch_ms(at_ms))
        .bind(cause.as_str())
        .bind(operation_key)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        batch_id.as_str(),
        settled,
        || StoreError::SessionCommandWithdrawn {
            session_id: session_id.clone(),
            batch_id: batch_id.clone(),
        },
    )
}
