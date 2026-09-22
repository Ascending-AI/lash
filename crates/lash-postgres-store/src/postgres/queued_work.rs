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
    available_at_ms: u64,
    enqueued_at_ms: u64,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_id: Option<String>,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

impl QueuedBatchRow {
    /// The claim columns the shared claimability verdict consults.
    ///
    /// Exposed as one value rather than two fields so a call site cannot pass
    /// a generation that belongs to a different row's token.
    pub(crate) fn claim_facts(&self) -> lash_core::store_backend_support::WorkRowClaimFacts<'_> {
        lash_core::store_backend_support::WorkRowClaimFacts {
            claim_token: self.claim_token.as_deref(),
            claim_session_lease_generation: self.claim_session_lease_generation,
        }
    }
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
        available_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "available_at_ms",
            row.get("available_at_ms"),
        )?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        claim_fencing_token: u64_from_sql(
            "QueuedWorkBatch",
            "claim_fencing_token",
            row.get("claim_fencing_token"),
        )?,
        claim_id: row.get("claim_id"),
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get("claim_session_lease_generation"),
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
        available_at_ms: row.available_at_ms,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    };
    batch.validate_payload_family()?;
    Ok(batch)
}

/// Observe every covered row under `FOR UPDATE` and return the shared
/// settlement plan for this completion (FIG-1065).
///
/// The plan's ordered writes execute later, in
/// [`complete_queued_work_claims_tx`](crate::runtime_persistence::complete_queued_work_claims_tx):
/// each consumed wake's source-key advisory lock and fence write still land
/// immediately before the queue row leaves, exactly where the hand-written
/// body put them.
pub(crate) async fn plan_queued_work_settlement_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed: &QueuedWorkCompletion,
) -> Result<lash_core::store::claim_plan::QueuedWorkSettlementPlan, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut rows = Vec::with_capacity(completed.batch_ids.len());
    for batch_id in &completed.batch_ids {
        // Lock and read: the row is held `FOR UPDATE` for the rest of the
        // commit, so it cannot move before the settlement below.
        let observed: Option<(Option<String>, Option<String>, i64)> =
            sqlx::query_as(sql.queued_batches_postgres.settlement_facts.sql())
                .bind(completed.session_id.as_str())
                .bind(batch_id.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        let claim = observed
            .map(|(claim_id, claim_token, generation)| {
                Ok(lash_core::store::claim_plan::QueuedWorkSettlementRowClaim {
                    claim_id,
                    claim_token,
                    claim_session_lease_generation: u64_from_sql(
                        "QueuedWorkBatch",
                        "claim_session_lease_generation",
                        generation,
                    )?,
                })
            })
            .transpose()?;
        // The wake identity a settled batch contributes to its redelivery
        // fence: the source key (advisory-lock identity) and the head
        // payload, both claim-keyed reads over the locked row. The advisory
        // lock itself stays at the write site.
        let consumed_wake = match claim.as_ref() {
            Some(_) => {
                let source_key: Option<String> =
                    sqlx::query_scalar(sql.family_postgres.select_claimed_batch_source_key.sql())
                        .bind(completed.session_id.as_str())
                        .bind(batch_id.as_str())
                        .bind(&completed.claim_id)
                        .bind(&completed.lease_token)
                        .fetch_optional(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?
                        .flatten();
                let payload_json: Option<String> =
                    sqlx::query_scalar(sql.queued_batches.select_claimed_batch_head_payload.sql())
                        .bind(completed.session_id.as_str())
                        .bind(batch_id.as_str())
                        .bind(&completed.claim_id)
                        .bind(&completed.lease_token)
                        .fetch_optional(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?;
                payload_json
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
                            Some(lash_core::store::claim_plan::ConsumedProcessWake {
                                source_key,
                                process_id: wake.process_id,
                                sequence: wake.sequence,
                            })
                        }
                        _ => None,
                    })
            }
            None => None,
        };
        rows.push(lash_core::store::claim_plan::QueuedWorkSettlementRow {
            batch_id: batch_id.clone(),
            claim,
            consumed_wake,
        });
    }
    // The shared planner takes the verdict: a settlement is authorized only
    // while every covered row still carries this claim's id and lease token.
    lash_core::store::claim_plan::plan_queued_work_settlement(completed, rows).into_result()
}
