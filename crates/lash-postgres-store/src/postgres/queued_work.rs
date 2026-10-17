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
    pub(crate) payload: lash_core_execution::runtime::SubmittedBody<QueuedWorkPayload>,
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
    let payload = row
        .get::<Option<String>, _>("payload_json")
        .map(|payload| store_decode_json::<QueuedWorkPayload>(&payload, "queued work payload"))
        .transpose()?
        .into();
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
    let row = crate::observed_sql::query(
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
