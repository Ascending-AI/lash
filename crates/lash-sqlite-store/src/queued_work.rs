use super::*;

pub(crate) fn decode_delivery_policy(value: String) -> Result<DeliveryPolicy, StoreError> {
    DeliveryPolicy::from_wire_str(&value).ok_or_else(|| {
        StoreError::Backend(format!("unknown queued-work delivery policy `{value}`"))
    })
}

pub(crate) fn decode_slot_policy(value: String) -> Result<SlotPolicy, StoreError> {
    SlotPolicy::from_wire_str(&value)
        .ok_or_else(|| StoreError::Backend(format!("unknown queued-work slot policy `{value}`")))
}

pub(crate) fn decode_merge_key(value: String) -> Result<MergeKey, StoreError> {
    serde_json::from_str(&value).map_err(|err| {
        StoreError::Backend(format!("failed to decode queued-work merge key: {err}"))
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
        slot_policy: decode_slot_policy(row.slot_policy)?,
        merge_key: decode_merge_key(row.merge_key_json)?,
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
                slot_policy: decode_slot_policy(row.slot_policy)?,
                merge_key: decode_merge_key(row.merge_key_json)?,
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
    pub(crate) slot_policy: String,
    pub(crate) merge_key_json: String,
    pub(crate) available_at_ms: u64,
    pub(crate) enqueued_at_ms: u64,
    pub(crate) claim_fencing_token: u64,
    pub(crate) claim_token: Option<String>,
    pub(crate) claim_session_lease_generation: u64,
}

pub(crate) fn queued_batch_row_from_sql(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<QueuedBatchRow> {
    Ok(QueuedBatchRow {
        enqueue_seq: row.get::<_, i64>(0)? as u64,
        batch_id: row.get(1)?,
        session_id: row.get(2)?,
        source_key: row.get(3)?,
        delivery_policy: row.get(4)?,
        slot_policy: row.get(5)?,
        merge_key_json: row.get(6)?,
        available_at_ms: row.get::<_, i64>(7)? as u64,
        enqueued_at_ms: row.get::<_, i64>(8)? as u64,
        claim_fencing_token: row.get::<_, i64>(9)? as u64,
        claim_token: row.get(13)?,
        claim_session_lease_generation: row.get::<_, i64>(14)? as u64,
    })
}

pub(crate) fn load_queued_batch_by_id_conn(
    conn: &Connection,
    batch_id: &str,
) -> Result<Option<QueuedWorkBatch>, StoreError> {
    let row = conn
        .query_row(
            "SELECT enqueue_seq, batch_id, session_id, source_key, delivery_policy,
                    slot_policy, merge_key_json, available_at_ms, enqueued_at_ms,
                    claim_fencing_token, claim_owner_id, claim_owner_incarnation_id,
                    claim_owner_liveness_json, claim_token, claim_session_lease_generation
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
    if let Some(source_key) = batch.source_key.as_deref() {
        if lash_core::is_process_wake_source_key(source_key)
            && let Some(consumed_at_ms) = conn
                .query_row(
                    "SELECT consumed_at_ms FROM consumed_wake_source_keys
                     WHERE session_id = ?1 AND source_key = ?2",
                    params![batch.session_id, source_key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(sqlite_error)?
        {
            return Ok(lash_core::runtime::consumed_queued_work_batch(
                batch,
                consumed_at_ms as u64,
            ));
        }
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
            return load_queued_batch_by_id_conn(conn, &batch_id)?.ok_or_else(|| {
                StoreError::Backend("queued work source row disappeared".to_string())
            });
        }
    }
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(nonce),
    );
    conn.execute(
        "INSERT INTO queued_work_batches (
            batch_id, session_id, source_key, delivery_policy, slot_policy,
            merge_key_json, available_at_ms, enqueued_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            batch_id,
            batch.session_id,
            batch.source_key.as_deref(),
            batch.delivery_policy.as_str(),
            batch.slot_policy.as_str(),
            encode_json(&batch.merge_key),
            batch.available_at_ms as i64,
            now as i64,
        ],
    )
    .map_err(sqlite_error)?;
    for (index, payload) in batch.payloads.iter().enumerate() {
        let item_id = format!("{batch_id}:item:{index}");
        conn.execute(
            "INSERT INTO queued_work_items (batch_id, item_index, item_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![batch_id, index as i64, item_id, encode_json(payload)],
        )
        .map_err(sqlite_error)?;
    }
    load_queued_batch_by_id_conn(conn, &batch_id)?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))
}

pub(crate) fn ensure_queued_work_completion_conn(
    conn: &Connection,
    completed: &QueuedWorkCompletion,
) -> Result<(), StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*)
             FROM queued_work_batches
             WHERE session_id = ?1
               AND claim_id = ?2
               AND claim_token = ?3",
        )
        .map_err(sqlite_error)?;
    let count: usize = stmt
        .query_row(
            params![
                completed.session_id,
                completed.claim_id,
                completed.lease_token
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sqlite_error)? as usize;
    ensure_completion_owns_all_batches(completed, count)
}
