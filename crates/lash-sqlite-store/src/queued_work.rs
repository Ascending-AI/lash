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
    pub(crate) enqueued_at_ms: u64,
    /// The root whose admission holds the batch; `None` while it is open.
    pub(crate) admitted_root: Option<String>,
}

/// The turn-lane candidate `batch` offers an admission.
pub(crate) fn turn_lane_candidate(batch: &QueuedWorkBatch) -> TurnLaneCandidate {
    TurnLaneCandidate::from_batch(batch)
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
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms")?,
        )?,
        admitted_root: row.get("admitted_root")?,
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
                now as i64,
                crate::session_ingress::allocate_sequence(conn, &batch.session_id)?,
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
        crate::conn::cached_execute(
            conn,
            sql.queued_items.insert_new.sql(),
            params![batch_id, index as i64, item_id, encode_json(payload)?],
        )
        .map_err(sqlite_error)?;
    }
    // The admitted batch owes its session a drive (ADR 0109 §3), armed in
    // the transaction that admits it.
    crate::ingress_obligation::arm_queued_batch_tx(conn, &batch.session_id, &batch_id, now)?;
    let inserted = load_queued_batch_by_id_conn(conn, &batch_id)?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    Ok(QueuedWorkEnqueueOutcome::Inserted(inserted))
}

/// Complete batch `batch_id`, which root `root` of session `session_id` must
/// hold, inside the commit's `BEGIN IMMEDIATE` write transaction (FIG-3927).
///
/// The shared verdict decides over the row as read; a consumed wake's
/// redelivery fence lands before the row leaves, because a crash between the
/// two would replay a wake the session already consumed (FIG-1065). The
/// root predicate stays on the delete as its backstop.
pub(crate) fn complete_admitted_batch_conn(
    conn: &Connection,
    session_id: &SessionId,
    root: &lash_core_execution::TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let turn_ingress = crate::turn_ingress::turn_ingress_sql();
    let row = lash_core_execution::store::IngressRowId::Batch(batch_id.clone());
    let observed: Option<Option<String>> = conn
        .query_row(
            turn_ingress.queued_batches_sqlite.settlement_facts.sql(),
            params![session_id.as_str(), batch_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &row,
        observed.as_ref().map(Option::as_deref),
    )?;
    // The wake identity a settled batch contributes to its redelivery fence:
    // the root-keyed head payload, decoded the same way PostgreSQL decodes it.
    // The wake batch carries exactly one wake item, so the head payload is
    // the batch's whole wake contribution.
    let terminal_wake = conn
        .query_row(
            turn_ingress
                .queued_batches
                .select_admitted_batch_head_payload
                .sql(),
            params![session_id.as_str(), batch_id.as_str(), root.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .map(decode_queued_payload)
        .transpose()?
        .and_then(|payload| {
            lash_core_execution::store::TerminalProcessWake::of_payload(None, &payload)
        });
    if let Some(wake) = terminal_wake.as_ref() {
        raise_wake_redelivery_fence_conn(conn, session_id, wake)?;
    }
    let settled = conn
        .execute(
            turn_ingress.queued_batches.settle_admitted.sql(),
            params![session_id.as_str(), batch_id.as_str(), root.as_str()],
        )
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::SQLITE_BACKEND,
        batch_id.as_str(),
        u64::try_from(settled).unwrap_or(u64::MAX),
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
pub(crate) fn settle_open_command_conn(
    conn: &Connection,
    session_id: &SessionId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let turn_ingress = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> = conn
        .query_row(
            turn_ingress.queued_batches_sqlite.settlement_facts.sql(),
            params![session_id.as_str(), batch_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_open_command(
        session_id,
        batch_id,
        observed.as_ref().map(Option::as_deref),
    )?;
    let settled = conn
        .execute(
            turn_ingress.queued_batches.settle_command.sql(),
            params![session_id.as_str(), batch_id.as_str()],
        )
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::SQLITE_BACKEND,
        batch_id.as_str(),
        u64::try_from(settled).unwrap_or(u64::MAX),
        || StoreError::SessionCommandWithdrawn {
            session_id: session_id.clone(),
            batch_id: batch_id.clone(),
        },
    )
}

/// Raise session `session_id`'s redelivery fence to `max(floor, sequence)`
/// for a wake whose row is leaving the queue in this transaction.
///
/// The one home of the invariant that every terminal transition of a wake —
/// settlement by its root and host cancel — raises the floor with the row's
/// removal (FIG-1065, FIG-3545). Callers write the fence before the delete.
pub(crate) fn raise_wake_redelivery_fence_conn(
    conn: &Connection,
    session_id: &SessionId,
    wake: &lash_core_execution::store::TerminalProcessWake,
) -> Result<(), StoreError> {
    let allocation_floor = i64::try_from(wake.sequence).map_err(|_| {
        stored_data_corrupt(
            "WakeRedeliveryFence",
            format!(
                "allocation_floor does not fit SQLite INTEGER: {}",
                wake.sequence
            ),
        )
    })?;
    crate::conn::cached_execute(
        conn,
        crate::process_registry::sql::process_sql()
            .fence_sqlite
            .upsert_max
            .sql(),
        params![
            session_id.as_str(),
            wake.process_id.as_str(),
            allocation_floor
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
