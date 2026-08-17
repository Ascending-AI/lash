use super::*;

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
            "SELECT item_id, payload_json
             FROM queued_work_items
             WHERE batch_id = ?1
             ORDER BY item_index ASC",
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
    Ok(QueuedWorkBatch {
        batch_id: row.batch_id,
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
    })
}

pub(crate) fn queued_work_batches_from_conn(
    conn: &Connection,
    rows: &[QueuedBatchRow],
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = "SELECT batch_id, item_id, payload_json
         FROM queued_work_items
         WHERE batch_id IN ("
        .to_string();
    for index in 0..rows.len() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
    sql.push_str(") ORDER BY batch_id ASC, item_index ASC");
    let mut stmt = conn.prepare(&sql).map_err(sqlite_error)?;
    let item_rows = stmt
        .query_map(
            rusqlite::params_from_iter(rows.iter().map(|row| row.batch_id.as_str())),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
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
            Ok(QueuedWorkBatch {
                batch_id: row.batch_id,
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
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedBatchRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) batch_id: String,
    pub(crate) session_id: String,
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

pub(crate) fn claim_candidate_from_row(
    row: &QueuedBatchRow,
    batch: &QueuedWorkBatch,
) -> Result<ClaimCandidate, StoreError> {
    batch.work_class().ok_or_else(|| {
        StoreError::Backend(format!(
            "queued-work batch `{}` has mixed or empty payload classes",
            batch.batch_id
        ))
    })?;
    Ok(ClaimCandidate::from_batch(
        batch,
        row.claim_fencing_token,
        row.claim_id.clone(),
    ))
}

pub(crate) fn queued_batch_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<QueuedBatchRow> {
    Ok(QueuedBatchRow {
        enqueue_seq: u64_from_sql("QueuedWorkBatch", "enqueue_seq", row.get(0)?)?,
        batch_id: row.get(1)?,
        session_id: row.get(2)?,
        source_key: row.get(3)?,
        delivery_policy: row.get(4)?,
        work_kind: row.get(5)?,
        authority_json: row.get(6)?,
        merge_key: row.get(7)?,
        available_at_ms: u64_from_sql("QueuedWorkBatch", "available_at_ms", row.get(8)?)?,
        enqueued_at_ms: u64_from_sql("QueuedWorkBatch", "enqueued_at_ms", row.get(9)?)?,
        claim_fencing_token: u64_from_sql("QueuedWorkBatch", "claim_fencing_token", row.get(10)?)?,
        claim_id: row.get(16)?,
        claim_token: row.get(14)?,
        claim_session_lease_generation: u64_from_sql(
            "QueuedWorkBatch",
            "claim_session_lease_generation",
            row.get(15)?,
        )?,
    })
}

pub(crate) fn load_queued_batch_by_id_conn(
    conn: &Connection,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = conn
        .query_row(
            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    work_kind, authority_json, merge_key, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation, claim_id
             FROM queued_work_batches
             WHERE batch_id = ?1",
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
            "SELECT allocation_floor FROM wake_redelivery_fences
                 WHERE session_id = ?1 AND process_id = ?2",
            params![batch.session_id, wake_source.process_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sqlite_error)?
    } else {
        None
    };
    if let Some(source_key) = batch.source_key.as_deref() {
        let existing_id: Option<String> = conn
            .query_row(
                "SELECT batch_id FROM queued_work_batches
                 WHERE session_id = ?1 AND source_key = ?2",
                params![batch.session_id, source_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        if let Some(batch_id) = existing_id {
            let existing = load_queued_batch_by_id_conn(conn, &batch_id)?.ok_or_else(|| {
                StoreError::Backend("queued work source row disappeared".to_string())
            })?;
            return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
        }
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
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(nonce),
    );
    conn.execute(
        "INSERT INTO queued_work_batches (
            batch_id, session_id, source_key, delivery_policy, work_kind,
            authority_json, merge_key, available_at_ms, enqueued_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            batch_id,
            batch.session_id,
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
    for (index, payload) in batch.payloads.iter().enumerate() {
        let item_id = format!("{batch_id}:item:{index}");
        conn.execute(
            "INSERT INTO queued_work_items (batch_id, item_index, item_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
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
        let authority = conn
            .query_row(
                "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM queued_work_batches
             WHERE session_id = ?1
               AND batch_id = ?2",
                params![completed.session_id, batch_id],
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
        let authority = authority
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
                row_id: Some(batch_id.clone().into_boxed_str()),
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
