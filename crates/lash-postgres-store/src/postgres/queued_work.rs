//! Queued-work row model: the durable representation of queued batches and
//! their items, mirroring the SQLite backend's `queued_work` module.
//! Originated in `session_factory.rs`; every item keeps its previous path
//! through the crate-root glob.

use crate::*;

pub(crate) const QUEUED_WORK_COLUMNS: [&str; 14] = [
    "enqueue_seq",
    "batch_id",
    "session_id",
    "source_key",
    "delivery_policy",
    "work_kind",
    "authority_json",
    "merge_key",
    "available_at_ms",
    "enqueued_at_ms",
    "claim_fencing_token",
    "claim_token",
    "claim_session_lease_generation",
    "claim_id",
];

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
    available_at_ms: u64,
    enqueued_at_ms: u64,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_id: Option<String>,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

pub(crate) fn claim_candidate_from_row(
    row: &QueuedBatchRow,
    batch: &QueuedWorkBatch,
) -> ClaimCandidate {
    ClaimCandidate::from_batch(
        batch,
        row.claim_fencing_token,
        row.claim_id.clone(),
        row.claim_token.clone(),
    )
}

pub(crate) fn queued_batch_row(row: PgRow) -> Result<QueuedBatchRow, StoreError> {
    let delivery_policy =
        DeliveryPolicy::from_wire_str(row.get::<String, _>(QUEUED_WORK_COLUMNS[4]).as_str())
            .ok_or_else(|| {
                StoreError::Backend("invalid queued work delivery policy".to_string())
            })?;
    let kind = QueuedWorkKind::from_wire_str(row.get::<String, _>(QUEUED_WORK_COLUMNS[5]).as_str())
        .ok_or_else(|| StoreError::Backend("invalid queued work kind".to_string()))?;
    let authority_json: String = row.get(QUEUED_WORK_COLUMNS[6]);
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql(
            "QueuedWorkBatch",
            "enqueue_seq",
            row.get(QUEUED_WORK_COLUMNS[0]),
        )?,
        batch_id: row.get(QUEUED_WORK_COLUMNS[1]),
        session_id: SessionId::from(row.get::<String, _>(QUEUED_WORK_COLUMNS[2])),
        source_key: row.get(QUEUED_WORK_COLUMNS[3]),
        delivery_policy,
        kind,
        authority: store_decode_json(&authority_json, "queued work authority")?,
        merge_key: row.get(QUEUED_WORK_COLUMNS[7]),
        available_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "available_at_ms",
            row.get(QUEUED_WORK_COLUMNS[8]),
        )?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get(QUEUED_WORK_COLUMNS[9]),
        )?,
        claim_fencing_token: u64_from_sql(
            "QueuedWorkBatch",
            "claim_fencing_token",
            row.get(QUEUED_WORK_COLUMNS[10]),
        )?,
        claim_id: row.get(QUEUED_WORK_COLUMNS[13]),
        claim_token: row.get(QUEUED_WORK_COLUMNS[11]),
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get(QUEUED_WORK_COLUMNS[12]),
        )?,
    })
}

pub(crate) async fn load_queued_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = sqlx::query(&format!(
        "SELECT {QUEUED_WORK_COLUMNS}
         FROM lash_queued_work_batches
         WHERE batch_id = $1",
        QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
    ))
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
        available_at_ms: row.available_at_ms,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    };
    batch.validate_payload_family()?;
    Ok(batch)
}

pub(crate) async fn ensure_queued_work_completion_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &QueuedWorkCompletion,
) -> Result<(), StoreError> {
    for batch_id in &completed.batch_ids {
        let authority: Option<(Option<String>, Option<String>, i64)> = sqlx::query_as(
            "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM lash_queued_work_batches
             WHERE session_id = $1
               AND batch_id = $2
             LIMIT 1
             FOR UPDATE",
        )
        .bind(completed.session_id.as_str())
        .bind(batch_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let authority = authority
            .map(|(claim_id, claim_token, generation)| {
                Ok((
                    claim_id,
                    claim_token,
                    u64_from_sql(
                        "QueuedWorkBatch",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                ))
            })
            .transpose()?;
        let owns_row = authority
            .as_ref()
            .is_some_and(|(claim_id, claim_token, _)| {
                claim_id.as_deref() == Some(completed.claim_id.as_str())
                    && claim_token.as_deref() == Some(completed.lease_token.as_str())
            });
        if !owns_row {
            return Err(StoreError::QueuedWorkClaimSuperseded {
                session_id: completed.session_id.clone(),
                claim_id: completed.claim_id.clone(),
                row_id: Some(batch_id.as_str().to_string().into_boxed_str()),
                superseding_claim_id: authority
                    .as_ref()
                    .and_then(|(claim_id, _, _)| claim_id.clone())
                    .map(String::into_boxed_str),
                superseding_session_lease_generation: authority.as_ref().and_then(
                    |(claim_id, _, generation)| claim_id.as_ref().map(|_| Box::new(*generation)),
                ),
            });
        }
    }
    Ok(())
}
