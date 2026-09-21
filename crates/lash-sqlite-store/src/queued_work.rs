use super::*;
use lash_sansio::SessionId;

pub(crate) fn decode_delivery_policy(value: String) -> Result<DeliveryPolicy, StoreError> {
    DeliveryPolicy::from_wire_str(&value).ok_or_else(|| {
        StoreError::Backend(format!("unknown queued-work delivery policy `{value}`"))
    })
}

pub(crate) fn decode_work_kind(value: String) -> Result<QueuedWorkKind, StoreError> {
    QueuedWorkKind::from_wire_str(&value)
        .ok_or_else(|| StoreError::Backend(format!("unknown queued-work kind `{value}`")))
}

pub(crate) fn decode_authority(value: String) -> Result<QueuedWorkAuthority, StoreError> {
    serde_json::from_str(&value).map_err(|err| {
        StoreError::Backend(format!("failed to decode queued-work authority: {err}"))
    })
}

pub(crate) fn decode_queued_payload(value: String) -> Result<QueuedWorkPayload, StoreError> {
    serde_json::from_str(&value)
        .map_err(|err| StoreError::Backend(format!("failed to decode queued-work payload: {err}")))
}

pub(crate) fn queued_work_batch_from_conn(
    conn: &Connection,
    row: QueuedBatchRow,
) -> Result<QueuedWorkBatch, StoreError> {
    let mut stmt = conn
        .prepare(
            crate::turn_ingress::turn_ingress_sql()
                .queued_items
                .list_by_batch
                .sql(),
        )
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params![row.batch_id.as_str()], |item_row| {
            Ok((item_row.get::<_, String>(0)?, item_row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?;
    let mut items = Vec::new();
    for item in rows {
        let (item_id, payload_json) = item.map_err(sqlite_error)?;
        items.push(QueuedWorkItem {
            item_id,
            payload: decode_queued_payload(payload_json)?,
        });
    }
    let batch = QueuedWorkBatch {
        batch_id: row.batch_id.into(),
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        delivery_policy: decode_delivery_policy(row.delivery_policy)?,
        kind: decode_work_kind(row.work_kind)?,
        authority: decode_authority(row.authority_json)?,
        merge_key: row.merge_key,
        available_at_ms: row.available_at_ms,
        enqueued_at_ms: row.enqueued_at_ms,
        items,
    };
    batch.validate_payload_family()?;
    Ok(batch)
}

pub(crate) fn queued_work_batches_from_conn(
    conn: &Connection,
    rows: &[QueuedBatchRow],
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let batch_ids = encode_json(
        &rows
            .iter()
            .map(|row| row.batch_id.as_str())
            .collect::<Vec<_>>(),
    )?;
    let mut stmt = conn
        .prepare(
            crate::turn_ingress::turn_ingress_sql()
                .queued_items_sqlite
                .list_by_batches
                .sql(),
        )
        .map_err(sqlite_error)?;
    let item_rows = stmt
        .query_map(params![batch_ids], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut items_by_batch = BTreeMap::<String, Vec<QueuedWorkItem>>::new();
    for item_row in item_rows {
        let (batch_id, item_id, payload_json) = item_row.map_err(sqlite_error)?;
        items_by_batch
            .entry(batch_id)
            .or_default()
            .push(QueuedWorkItem {
                item_id,
                payload: decode_queued_payload(payload_json)?,
            });
    }
    rows.iter()
        .cloned()
        .map(|row| {
            let items = items_by_batch.remove(&row.batch_id).unwrap_or_default();
            let batch = QueuedWorkBatch {
                batch_id: row.batch_id.into(),
                session_id: row.session_id,
                enqueue_seq: row.enqueue_seq,
                source_key: row.source_key,
                delivery_policy: decode_delivery_policy(row.delivery_policy)?,
                kind: decode_work_kind(row.work_kind)?,
                authority: decode_authority(row.authority_json)?,
                merge_key: row.merge_key,
                available_at_ms: row.available_at_ms,
                enqueued_at_ms: row.enqueued_at_ms,
                items,
            };
            batch.validate_payload_family()?;
            Ok(batch)
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) source_key: Option<String>,
    pub(crate) delivery_policy: String,
    pub(crate) work_kind: String,
    pub(crate) authority_json: String,
    pub(crate) merge_key: Option<String>,
    pub(crate) available_at_ms: u64,
    pub(crate) enqueued_at_ms: u64,
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

pub(crate) fn queued_batch_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<QueuedBatchRow> {
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql("QueuedWorkBatch", "enqueue_seq", row.get("enqueue_seq")?)?,
        batch_id: row.get("batch_id")?,
        session_id: SessionId::from(row.get::<_, String>("session_id")?),
        source_key: row.get("source_key")?,
        delivery_policy: row.get("delivery_policy")?,
        work_kind: row.get("work_kind")?,
        authority_json: row.get("authority_json")?,
        merge_key: row.get("merge_key")?,
        available_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "available_at_ms",
            row.get("available_at_ms")?,
        )?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms")?,
        )?,
        claim_fencing_token: u64_from_sql(
            "QueuedWorkBatch",
            "claim_fencing_token",
            row.get("claim_fencing_token")?,
        )?,
        claim_id: row.get("claim_id")?,
        claim_token: row.get("claim_token")?,
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get("claim_session_lease_generation")?,
        )?,
    })
}

pub(crate) fn load_queued_batch_by_id_conn(
    conn: &Connection,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .select_by_id
                .sql(),
            params![batch_id],
            queued_batch_row_from_sql,
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(|row| queued_work_batch_from_conn(conn, row))
        .transpose()
}

pub(crate) fn enqueue_queued_work_conn(
    conn: &Connection,
    batch: &QueuedWorkBatchDraft,
    now: u64,
    nonce: u64,
) -> Result<QueuedWorkBatch, StoreError> {
    enqueue_queued_work_conn_with_outcome(conn, batch, now, nonce)
        .map(QueuedWorkEnqueueOutcome::into_batch)
}

pub(crate) fn enqueue_queued_work_conn_with_outcome(
    conn: &Connection,
    batch: &QueuedWorkBatchDraft,
    now: u64,
    nonce: u64,
) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
    let sql_available_at_ms =
        sql_counter_value("queued_work_available_at_ms", batch.available_at_ms)?;
    let allocation_floor = if let Some(wake_source) = batch.process_wake_source.as_ref() {
        conn.query_row(
            crate::process_registry::sql::process_sql()
                .fence
                .select_floor
                .sql(),
            params![batch.session_id.as_str(), wake_source.process_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sqlite_error)?
    } else {
        None
    };
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(nonce),
    );
    let sql = crate::turn_ingress::turn_ingress_sql();
    let inserted = conn
        .execute(
            sql.queued_batches_sqlite.insert_new.sql(),
            params![
                batch_id.as_str(),
                batch.session_id.as_str(),
                batch.source_key.as_deref(),
                batch.delivery_policy.as_str(),
                batch.kind().as_str(),
                encode_json(&batch.authority)?,
                batch.merge_key.as_deref(),
                sql_available_at_ms,
                now as i64,
            ],
        )
        .map_err(sqlite_error)?;
    if inserted == 0 {
        let source_key = batch.source_key.as_deref().ok_or_else(|| {
            StoreError::Backend("queued work insert without source key was ignored".to_string())
        })?;
        let existing_id: Option<String> = conn
            .query_row(
                sql.queued_batches.select_id_by_source_key.sql(),
                params![batch.session_id.as_str(), source_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        let existing_id = existing_id.ok_or_else(|| {
            StoreError::Backend("queued work conflict row disappeared".to_string())
        })?;
        let existing = load_queued_batch_by_id_conn(conn, &existing_id)?
            .ok_or_else(|| StoreError::Backend("queued work source row disappeared".to_string()))?;
        return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
    }
    let allocation_floor = allocation_floor
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                stored_data_corrupt(
                    "WakeAllocationFloor",
                    format!("allocation_floor must be non-negative, got {value}"),
                )
            })
        })
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
        conn.execute(
            sql.queued_items.insert_new.sql(),
            params![batch_id, index as i64, item_id, encode_json(payload)?],
        )
        .map_err(sqlite_error)?;
    }
    let inserted = load_queued_batch_by_id_conn(conn, &batch_id)?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    Ok(QueuedWorkEnqueueOutcome::Inserted(inserted))
}

pub(crate) fn ensure_queued_work_completion_conn(
    conn: &Connection,
    completed: &QueuedWorkCompletion,
) -> Result<(), StoreError> {
    for batch_id in &completed.batch_ids {
        // Lock and read: this runs inside the commit's `BEGIN IMMEDIATE`
        // transaction, so the row cannot move before the settlement below.
        let observed = conn
            .query_row(
                crate::turn_ingress::turn_ingress_sql()
                    .queued_batches_sqlite
                    .settlement_facts
                    .sql(),
                params![completed.session_id.as_str(), batch_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        let observed = observed
            .map(|(claim_id, claim_token, generation)| {
                Ok((
                    claim_id,
                    claim_token,
                    u64::try_from(generation).map_err(|_| {
                        stored_data_corrupt(
                            "QueuedWorkBatch",
                            format!(
                                "claim_session_lease_generation must be non-negative, got {generation}"
                            ),
                        )
                    })?,
                ))
            })
            .transpose()?;
        // The shared verdict is the decision: a settlement is authorized only
        // while the row still carries this claim's id and lease token.
        lash_core::store_backend_support::require_settleable_queued_work(
            completed,
            batch_id.as_str(),
            observed
                .as_ref()
                .map(|(claim_id, claim_token, generation)| {
                    lash_core::store_backend_support::QueuedWorkSettlementFacts {
                        claim_id: claim_id.as_deref(),
                        claim_token: claim_token.as_deref(),
                        claim_session_lease_generation: *generation,
                    }
                }),
        )?;
    }
    Ok(())
}
