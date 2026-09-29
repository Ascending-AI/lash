//! Queued-work row model: the durable representation of queued batches and
//! their items, mirroring the SQLite backend's `queued_work` module.
//! Originated in `session_factory.rs`; every item keeps its previous path
//! through the crate-root glob.

use crate::*;

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    session_id: SessionId,
    source_key: Option<String>,
    pub(crate) delivery_policy: DeliveryPolicy,
    pub(crate) kind: QueuedWorkKind,
    pub(crate) authority: QueuedWorkAuthority,
    pub(crate) merge_key: Option<String>,
    enqueued_at_ms: u64,
}

/// The turn-lane candidate `batch` offers an admission.
pub(crate) fn turn_lane_candidate(batch: &QueuedWorkBatch) -> TurnLaneCandidate {
    TurnLaneCandidate::from_batch(batch)
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>("delivery_policy").as_str())
            .ok_or_else(|| {
                StoreError::Backend("invalid queued work delivery policy".to_string())
            })?;
    let kind = QueuedWorkKind::from_wire_str(row.get::<String, _>("work_kind").as_str())
        .ok_or_else(|| StoreError::Backend("invalid queued work kind".to_string()))?;
    let authority_json: String = row.get("authority_json");
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql("QueuedWorkBatch", "enqueue_seq", row.get("enqueue_seq"))?,
        batch_id: row.get("batch_id"),
        session_id: SessionId::from(row.get::<String, _>("session_id")),
        source_key: row.get("source_key"),
        delivery_policy,
        kind,
        authority: store_decode_json(&authority_json, "queued work authority")?,
        merge_key: row.get("merge_key"),
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .select_by_id
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .queued_items
            .list_by_batch
            .sql(),
    )
    .bind(row.batch_id.as_str())
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
    let batch = QueuedWorkBatch {
        batch_id: row.batch_id.into(),
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        delivery_policy: row.delivery_policy,
        kind: row.kind,
        authority: row.authority,
        merge_key: row.merge_key,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    };
    batch.validate_payload_family()?;
    Ok(batch)
}

/// Settle batch `batch_id` of session `session_id`, which root `root` must
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
    root: &lash_core_execution::TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let row = lash_core_execution::store::IngressRowId::Batch(batch_id.clone());
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &row,
        observed.as_ref().map(Option::as_deref),
    )?;
    // The wake identity a settled batch contributes to its redelivery fence:
    // the source key (advisory-lock identity) and the head payload, both
    // root-keyed reads over the locked row.
    let source_key: Option<String> =
        sqlx::query_scalar(sql.family_postgres.select_admitted_batch_source_key.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .bind(root.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .flatten();
    let payload_json: Option<String> =
        sqlx::query_scalar(sql.queued_batches.select_admitted_batch_head_payload.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .bind(root.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    let terminal_wake = payload_json
        .as_deref()
        .map(|json| {
            store_decode_json::<lash_core_execution::runtime::QueuedWorkPayload>(
                json,
                "queued work payload",
            )
        })
        .transpose()?
        .and_then(|payload| {
            lash_core_execution::store::TerminalProcessWake::of_payload(source_key, &payload)
        });
    if let Some(wake) = terminal_wake.as_ref() {
        crate::runtime_persistence::raise_wake_redelivery_fence_tx(tx, session_id, wake).await?;
    }
    let settled = sqlx::query(sql.queued_batches.settle_admitted.sql())
        .bind(session_id.as_str())
        .bind(batch_id.as_str())
        .bind(root.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    // Backstop: the verdict above was taken under the row lock, so the root
    // predicate cannot legitimately miss.
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        batch_id.as_str(),
        settled,
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            root: root.clone(),
            row: Box::new(row.clone()),
            admitted_root: None,
        },
    )
}

/// Settle open session command `batch_id` of session `session_id`, in the
/// commit that applied it (FIG-3927): the command lane takes no admission,
/// so the predicate is the row's presence and openness. A command withdrawn
/// since the drive read it refuses the whole commit.
pub(crate) async fn settle_open_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(&mut **tx)
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
        .execute(&mut **tx)
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
