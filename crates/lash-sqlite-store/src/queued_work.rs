use super::*;
use lash_sansio::SessionId;

pub(crate) fn decode_delivery_policy(value: String) -> Result<DeliveryPolicy, StoreError> {
    DeliveryPolicy::from_wire_str(&value).ok_or_else(|| {
        stored_data_corrupt(
            "QueuedWorkBatch",
            format_args!("unknown queued-work delivery policy `{value}`"),
        )
    })
}

pub(crate) fn decode_work_kind(value: String) -> Result<QueuedWorkKind, StoreError> {
    QueuedWorkKind::from_wire_str(&value).ok_or_else(|| {
        stored_data_corrupt(
            "QueuedWorkBatch",
            format_args!("unknown queued-work kind `{value}`"),
        )
    })
}

pub(crate) fn decode_authority(value: String) -> Result<QueuedWorkAuthority, StoreError> {
    serde_json::from_str(&value).map_err(|err| stored_data_corrupt("QueuedWorkAuthority", err))
}

pub(crate) fn decode_queued_payload(value: String) -> Result<QueuedWorkPayload, StoreError> {
    serde_json::from_str(&value).map_err(|err| stored_data_corrupt("QueuedWorkPayload", err))
}

pub(crate) fn queued_work_batch_from_row(
    row: QueuedBatchRow,
) -> Result<QueuedWorkBatch, StoreError> {
    row.into_batch()
}

pub(crate) fn queued_work_batches_from_rows(
    rows: &[QueuedBatchRow],
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    rows.iter()
        .cloned()
        .map(QueuedBatchRow::into_batch)
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
    pub(crate) payload_json: String,
    pub(crate) authority_json: String,
    pub(crate) merge_key: Option<String>,
    pub(crate) enqueued_at_ms: u64,
    pub(crate) submission_digest: String,
    /// The run whose admission holds the batch; `None` while it is open.
    pub(crate) admitted_run: Option<String>,
    pub(crate) terminal_cause: Option<String>,
    pub(crate) terminal_at_ms: Option<u64>,
    pub(crate) trace_cause_json: Option<String>,
}

impl QueuedBatchRow {
    /// Decode the batch and its sole payload from one row.
    fn into_batch(self) -> Result<QueuedWorkBatch, StoreError> {
        let payload = decode_queued_payload(self.payload_json)?;
        if decode_work_kind(self.work_kind)? != payload.kind() {
            return Err(stored_data_corrupt(
                "QueuedWorkBatch",
                "work kind contradicts its payload",
            ));
        }
        let batch = QueuedWorkBatch {
            batch_id: self.batch_id.try_into()?,
            session_id: self.session_id,
            enqueue_seq: self.enqueue_seq,
            source_key: self.source_key,
            delivery_policy: decode_delivery_policy(self.delivery_policy)?,
            authority: decode_authority(self.authority_json)?,
            merge_key: self.merge_key,
            enqueued_at_ms: self.enqueued_at_ms,
            payload,
            submission_digest: self.submission_digest,
            terminal: lash_core_execution::store_backend_support::decode_ingress_terminal(
                "QueuedWorkBatch",
                self.terminal_cause.as_deref(),
                self.terminal_at_ms,
            )?,
            trace_cause: lash_core_execution::store_backend_support::decode_trace_cause(
                "QueuedWorkBatch",
                self.trace_cause_json.as_deref(),
            )?,
        };
        Ok(batch)
    }
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
        session_id: crate::codec::sql_identity(row.get::<_, String>("session_id")?)?,
        source_key: row.get("source_key")?,
        delivery_policy: row.get("delivery_policy")?,
        work_kind: row.get("work_kind")?,
        payload_json: row.get("payload_json")?,
        authority_json: row.get("authority_json")?,
        merge_key: row.get("merge_key")?,
        enqueued_at_ms: u64_from_sql(
            "QueuedWorkBatch",
            "enqueued_at_ms",
            row.get("enqueued_at_ms")?,
        )?,
        submission_digest: row.get("submission_digest")?,
        admitted_run: row.get("admitted_run")?,
        terminal_cause: row.get("terminal_cause")?,
        terminal_at_ms: row
            .get::<_, Option<i64>>("terminal_at_ms")?
            .map(|at| u64_from_sql("QueuedWorkBatch", "terminal_at_ms", at))
            .transpose()?,
        trace_cause_json: row.get("trace_cause_json")?,
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
    row.map(queued_work_batch_from_row).transpose()
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

/// Admit `batch` under the database write lock the caller's transaction
/// holds (ADR 0101 §8): a batch the session already filed under the draft's
/// source key, open or a tombstone, answers an identical submission and
/// refuses a changed one; otherwise the draft is inserted at the next
/// position of the session's ingress sequence with its submission digest.
pub(crate) fn enqueue_queued_work_conn_with_outcome(
    conn: &Connection,
    batch: &QueuedWorkBatchDraft,
    now: u64,
    nonce: u64,
) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
    use lash_core_execution::store_backend_support as support;
    support::validate_queued_work_draft(batch)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let submission_digest = support::queued_work_submission_digest(batch)?;
    if let Some(source_key) = batch.source_key.as_deref() {
        let by_source_key = conn
            .query_row(
                sql.queued_batches.select_id_by_source_key.sql(),
                params![batch.session_id.as_str(), source_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let admission =
            support::decide_queued_work_draft_admission(batch, &submission_digest, by_source_key)?;
        if let support::QueuedWorkDraftAdmission::Existing { batch_id } = admission {
            let existing =
                load_queued_batch_by_id_conn(conn, batch_id.as_str())?.ok_or_else(|| {
                    StoreError::Backend("queued work source row disappeared".to_string())
                })?;
            return Ok(QueuedWorkEnqueueOutcome::Existing(existing));
        }
    }
    let batch_id = derive_batch_id(
        &batch.session_id,
        batch.source_key.as_deref(),
        now,
        Some(nonce),
    );
    conn.execute(
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
            submission_digest.as_str(),
            encode_json(&batch.payload)?,
            lash_core_execution::store_backend_support::encode_trace_cause(&batch.trace_cause)?,
        ],
    )
    .map_err(sqlite_error)?;
    // The batch and the session's wake commit together (ADR 0132 §12).
    crate::durable::wake_session_tx(conn, &batch.session_id, false, now)?;
    let inserted = load_queued_batch_by_id_conn(conn, &batch_id)?
        .ok_or_else(|| StoreError::Backend("queued work insert disappeared".to_string()))?;
    Ok(QueuedWorkEnqueueOutcome::Inserted(inserted))
}

/// Complete batch `batch_id`, which run `run` of session `session_id` must
/// hold, inside the commit's `BEGIN IMMEDIATE` write transaction (FIG-3927).
///
/// The shared verdict decides over the row as read; a consumed wake's
/// redelivery fence lands before the row leaves, because a crash between the
/// two would replay a wake the session already consumed (FIG-1065). The
/// run predicate stays on the delete as its backstop.
pub(crate) fn complete_admitted_batch_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &lash_core_execution::TurnId,
    batch_id: &lash_core_execution::BatchId,
    terminal: lash_core_execution::store::IngressTerminal,
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
    lash_core_execution::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &row,
        observed.as_ref().map(Option::as_deref),
    )?;
    let settled = conn
        .execute(
            turn_ingress.queued_batches.settle_admitted.sql(),
            params![
                session_id.as_str(),
                batch_id.as_str(),
                run.as_str(),
                terminal.cause.as_str(),
                crate::clamp_epoch_ms(terminal.at_ms),
            ],
        )
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::SQLITE_BACKEND,
        batch_id.as_str(),
        u64::try_from(settled).unwrap_or(u64::MAX),
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
pub(crate) fn settle_open_command_conn(
    conn: &Connection,
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
            params![
                session_id.as_str(),
                batch_id.as_str(),
                crate::clamp_epoch_ms(at_ms),
                cause.as_str(),
                operation_key
            ],
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
