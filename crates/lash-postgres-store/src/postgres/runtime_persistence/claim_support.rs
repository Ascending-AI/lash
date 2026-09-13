use super::*;

pub(super) enum ClaimTransactionOutcome<T> {
    Commit(T),
    Rollback(T),
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn checkpoint_work_pending_postgres(
    pool: &PgPool,
    session_id: &SessionId,
    generation: u64,
    turn_id: &TurnId,
    checkpoint: lash_core::CheckpointKind,
    max_inputs: usize,
    max_batches: usize,
) -> Result<bool, StoreError> {
    if max_inputs == 0 && max_batches == 0 {
        return Ok(false);
    }
    let mut connection = acquire_runtime_connection(pool).await?;
    let head_candidate =
        postgres_queued_work_head_candidate_cte(QueuedWorkClaimBoundary::ActiveTurnCheckpoint);
    let admitted_min_boundary = lash_core::store_backend_support::admitted_min_boundary_sql(
        "ingress_json::jsonb ->> 'min_boundary'",
        checkpoint,
    );
    let admitted_states = lash_core::store_backend_support::state_sql_literal_list(&[
        lash_core::TurnInputState::PendingActive,
        lash_core::TurnInputState::Accepted,
    ]);
    let sql = format!(
        "WITH {head_candidate}
         SELECT (
            $4 > 0 AND EXISTS (
                SELECT 1
                FROM lash_pending_turn_inputs
                WHERE session_id = $1
                  AND state IN ({admitted_states})
                  AND (claim_token IS NULL OR claim_session_lease_generation <> $2)
                  AND ingress_json::jsonb ->> 'scope' = 'active_turn'
                  AND ingress_json::jsonb ->> 'turn_id' = $3
                  AND {admitted_min_boundary}
                LIMIT 1
            )
         ) OR (
            $5 > 0 AND EXISTS (
                SELECT 1
                FROM lash_queued_work_items AS item
                JOIN queued_work_head_candidate AS head
                  ON head.head_batch_id = item.batch_id
                WHERE item.payload_json::jsonb ->> 'type' <> 'session_command'
                LIMIT 1
            )
         )"
    );
    sqlx::query_scalar(&sql)
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(turn_id.as_str())
        .bind(max_inputs as i64)
        .bind(max_batches as i64)
        .fetch_one(&mut *connection)
        .await
        .map_err(store_sqlx_error)
}

#[allow(clippy::too_many_arguments)]
/// Name the refusal behind an empty candidate scan.
///
/// The candidate query enforces the delivery-boundary rule in SQL, so a scan
/// that comes back empty tells the shared claim state machine nothing. Asking
/// it again with the unfiltered ready head keeps the classification in one
/// place: whatever the head alone is refused for is what this claim is refused
/// for. With no ready head at all, a lane still holding deferred work is not an
/// exhausted lane. Both probes read the same `transaction_timestamp()` cutoff
/// the candidate query used, and both run only on a refusal.
pub(super) async fn postgres_refusal_for_empty_scan(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
) -> Result<TurnWorkEmptyScanDiagnostic, StoreError> {
    let now = postgres_transaction_epoch_ms(tx).await?;
    let head_rows = sqlx::query(&format!(
        "SELECT {QUEUED_WORK_COLUMNS}
         FROM lash_queued_work_batches
         WHERE {POSTGRES_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
         ORDER BY enqueue_seq ASC
         LIMIT 1",
        QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
    ))
    .bind(session_id.as_str())
    .bind(sql_session_lease_generation(generation)?)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if let Some(head_row) = head_rows.into_iter().next() {
        let head_row = queued_batch_row(head_row)?;
        let head_batch = queued_work_batch_from_row(tx, head_row.clone()).await?;
        let head_candidates = vec![claim_candidate_from_row(&head_row, &head_batch)];
        let head_prefix = select_turn_work_claim_prefix(&head_candidates, boundary, policy, now)?;
        return Ok(TurnWorkEmptyScanDiagnostic::from(head_prefix));
    }
    let epoch_ms = transaction_epoch_sql!();
    let deferred_row_pending: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (
             SELECT 1
             FROM lash_queued_work_batches
             WHERE session_id = $1
               AND available_at_ms > {epoch_ms}
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $2
               )
         )",
    ))
    .bind(session_id.as_str())
    .bind(sql_session_lease_generation(generation)?)
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(TurnWorkEmptyScanDiagnostic::Refused {
        reason: if deferred_row_pending {
            QueuedWorkClaimRefusal::NotYetAvailable
        } else {
            QueuedWorkClaimRefusal::Empty
        },
    })
}

// Exact selection passes its full validation span: validate every fencing
// token before writing, including candidates outside the selected prefix.
pub(super) async fn claim_queued_work_rows_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: u64,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    generation: u64,
    selected_batches: Vec<QueuedWorkBatch>,
    candidates: &[ClaimCandidate],
) -> Result<ClaimTransactionOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if selected_batches.is_empty() {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let lease =
        WorkClaimLease::derive_queued_work(&candidates[0], session_id, owner, now, generation)?;
    let sql_fencing_tokens = sql_claim_fencing_tokens(
        "queued_work_claim_fencing_token",
        candidates
            .iter()
            .map(|candidate| candidate.claim_fencing_token),
    )?;
    for (row, sql_fencing_token) in selected_batches
        .iter()
        .zip(sql_fencing_tokens.iter().copied())
    {
        let changed = sqlx::query(
            "UPDATE lash_queued_work_batches
             SET claim_id = $3,
                 claim_token = $4,
                 claim_fencing_token = $6,
                 claim_session_lease_generation = $5
             WHERE session_id = $1
               AND batch_id = $2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $5
               )",
        )
        .bind(session_id.as_str())
        .bind(&row.batch_id)
        .bind(&lease.claim_id)
        .bind(&lease.lease_token)
        .bind(sql_session_lease_generation(
            lease.session_lease_generation,
        )?)
        .bind(sql_fencing_token)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
    }
    Ok(ClaimTransactionOutcome::Commit(Some(QueuedWorkClaim {
        session_id: SessionId::from(session_id.to_string()),
        claim_id: lease.claim_id,
        owner: owner.clone(),
        lease_token: lease.lease_token,
        fencing_token: lease.fencing_token,
        session_lease_generation: lease.session_lease_generation,
        data: lash_core::store_backend_support::queued_work_claim_data(
            selected_batches,
            candidates[0].prior_claim_id.clone(),
            candidates[0].prior_claim_token.clone(),
        )?,
    })))
}

pub(super) async fn scan_queued_work_candidates_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    max_rows: usize,
) -> Result<(Vec<QueuedWorkBatch>, Vec<ClaimCandidate>), StoreError> {
    let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(boundary))
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(claim_scan_limit(max_rows))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut selected = Vec::new();
    for row in rows {
        let row = queued_batch_row(row)?;
        if row.claim_token.is_none() || row.claim_session_lease_generation != generation {
            selected.push(row);
        }
    }
    let mut selected_batches = Vec::new();
    for row in &selected {
        selected_batches.push(queued_work_batch_from_row(tx, row.clone()).await?);
    }
    let candidates = selected
        .iter()
        .zip(selected_batches.iter())
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Vec<_>>();
    Ok((selected_batches, candidates))
}

pub(super) async fn claim_ready_queued_work_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    policy: QueuedWorkClaimPolicy,
) -> Result<ClaimTransactionOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if policy.max_rows == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let (mut selected_batches, candidates) =
        scan_queued_work_candidates_postgres(tx, session_id, generation, boundary, policy.max_rows)
            .await?;
    let selected_len = match select_turn_work_claim_prefix(&candidates, boundary, &policy, now)? {
        TurnWorkClaimPrefix::Selected { len } => len,
        TurnWorkClaimPrefix::Refused { .. } => {
            return Ok(ClaimTransactionOutcome::Commit(None));
        }
    };

    selected_batches.truncate(selected_len);
    claim_queued_work_rows_postgres(
        tx,
        now,
        session_id,
        owner,
        generation,
        selected_batches,
        &candidates[..selected_len],
    )
    .await
}

/// Load one cancellation record from a single PostgreSQL snapshot so payload
/// retention cannot split request metadata from its affected-input evidence.
pub(super) async fn load_turn_cancel_request_pg(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let record = load_turn_cancel_request_in_tx(&mut tx, session_id, turn_id, false, None).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(record)
}

type TurnCancelIntentRow = (String, Option<String>, Option<String>, String, String, i64);

pub(super) async fn load_turn_cancel_intent_snapshot_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core::TurnCancelIntentSnapshot, StoreError> {
    let row: Option<TurnCancelIntentRow> = sqlx::query_as(
        "SELECT request_id, origin, reason, disposition, mode, intent_revision
         FROM lash_turn_cancel_requests
         WHERE session_id = $1 AND turn_id = $2 FOR UPDATE",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

fn turn_cancel_snapshot_from_row(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: Option<TurnCancelIntentRow>,
) -> Result<lash_core::TurnCancelIntentSnapshot, StoreError> {
    let Some((request_id, origin, reason, disposition, mode, revision)) = row else {
        return Ok(lash_core::TurnCancelIntentSnapshot::Absent);
    };
    let revision = u64::try_from(revision).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "TurnCancelRequest",
        message: "intent revision is negative".to_string(),
    })?;
    if revision == 0 {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: "intent revision is zero".to_string(),
        });
    }
    Ok(lash_core::TurnCancelIntentSnapshot::Present {
        request: lash_core::facade_support::TurnCancelRequest {
            address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_disposition_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
        revision,
    })
}

pub(super) async fn load_turn_cancel_intent_snapshot_pg(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core::TurnCancelIntentSnapshot, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let row = sqlx::query_as(
        "SELECT request_id, origin, reason, disposition, mode, intent_revision
         FROM lash_turn_cancel_requests
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

#[derive(Default)]
pub struct TurnCancelReadPause {
    #[cfg(any(test, feature = "testing"))]
    metadata_read: tokio::sync::Notify,
    #[cfg(any(test, feature = "testing"))]
    resume: tokio::sync::Notify,
}

impl TurnCancelReadPause {
    async fn after_metadata_read(&self) {
        #[cfg(any(test, feature = "testing"))]
        {
            self.metadata_read.notify_one();
            self.resume.notified().await;
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn wait_until_metadata_read(&self) {
        self.metadata_read.notified().await;
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn resume(&self) {
        self.resume.notify_one();
    }
}

#[cfg(any(test, feature = "testing"))]
pub(super) async fn load_turn_cancel_request_pg_with_pause(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
    pause: &TurnCancelReadPause,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let record =
        load_turn_cancel_request_in_tx(&mut tx, session_id, turn_id, false, Some(pause)).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(record)
}

async fn load_turn_cancel_request_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    lock_request: bool,
    pause: Option<&TurnCancelReadPause>,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let metadata_sql = if lock_request {
        "SELECT request_id, origin, reason, disposition, mode,
                affected_input_ids, affected_dispositions
         FROM lash_turn_cancel_requests
         WHERE session_id = $1 AND turn_id = $2 FOR UPDATE"
    } else {
        "SELECT request_id, origin, reason, disposition, mode,
                affected_input_ids, affected_dispositions
         FROM lash_turn_cancel_requests
         WHERE session_id = $1 AND turn_id = $2"
    };
    let row: Option<TurnCancelRequestRow> = sqlx::query_as(metadata_sql)
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    validate_turn_cancel_request_arrays(&row)?;
    if let Some(pause) = pause {
        pause.after_metadata_read().await;
    }
    let affected_rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT affected.input_id, pending.input_json, affected.disposition
         FROM lash_turn_cancel_requests request
         CROSS JOIN LATERAL unnest(
             request.affected_input_ids,
             request.affected_dispositions
         ) WITH ORDINALITY AS affected(input_id, disposition, ordinal)
         JOIN lash_pending_turn_inputs pending
           ON pending.session_id = request.session_id
          AND pending.input_id = affected.input_id
         WHERE request.session_id = $1 AND request.turn_id = $2
         ORDER BY affected.ordinal ASC",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_record_from_rows(session_id, turn_id, row, affected_rows).map(Some)
}

pub(super) async fn load_turn_cancel_request_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    load_turn_cancel_request_in_tx(tx, session_id, turn_id, true, None).await
}

/// One `lash_turn_cancel_requests` row: request id, origin, reason,
/// disposition, mode, affected input ids, affected dispositions.
pub(super) type TurnCancelRequestRow = (
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Vec<String>,
    Vec<String>,
);

fn validate_turn_cancel_request_arrays(row: &TurnCancelRequestRow) -> Result<(), StoreError> {
    if row.5.len() != row.6.len() {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: format!(
                "affected input/disposition cardinality differs: {} input ids, {} dispositions",
                row.5.len(),
                row.6.len()
            ),
        });
    }
    Ok(())
}

pub(super) fn turn_cancel_record_from_rows(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: TurnCancelRequestRow,
    affected_rows: Vec<(String, String, String)>,
) -> Result<lash_core::TurnCancelRequestRecord, StoreError> {
    validate_turn_cancel_request_arrays(&row)?;
    let (request_id, origin, reason, disposition, mode, affected_input_ids, affected_dispositions) =
        row;
    if affected_rows.len() != affected_input_ids.len() {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: format!(
                "affected payload rows are incomplete: expected ids {affected_input_ids:?}, found {} rows",
                affected_rows.len()
            ),
        });
    }
    let mut affected_inputs = Vec::with_capacity(affected_rows.len());
    for (index, (input_id, input_json, applied_disposition)) in
        affected_rows.into_iter().enumerate()
    {
        if input_id != affected_input_ids[index]
            || applied_disposition != affected_dispositions[index]
        {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "TurnCancelRequest",
                message: format!(
                    "affected input evidence is misaligned at position {index}: expected id `{}` with disposition `{}`, found id `{input_id}` with disposition `{applied_disposition}`",
                    affected_input_ids[index], affected_dispositions[index]
                ),
            });
        }
        affected_inputs.push(lash_core::TurnCancelAffectedInput {
            input_id,
            payload: store_decode_json(&input_json, "turn input")?,
            disposition: turn_cancel_disposition_from_wire(&applied_disposition)?,
        });
    }
    Ok(lash_core::TurnCancelRequestRecord {
        request: lash_core::facade_support::TurnCancelRequest {
            address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_disposition_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
        outcome: (!affected_inputs.is_empty())
            .then_some(lash_core::TurnCancelInputOutcome { affected_inputs }),
    })
}

pub(super) fn turn_cancel_mode_wire(
    mode: lash_core::facade_support::TurnCancelMode,
) -> &'static str {
    match mode {
        lash_core::facade_support::TurnCancelMode::Immediate => "immediate",
        lash_core::facade_support::TurnCancelMode::AfterStep => "after_step",
    }
}

pub(super) fn turn_cancel_mode_from_wire(
    mode: &str,
) -> Result<lash_core::facade_support::TurnCancelMode, StoreError> {
    match mode {
        "immediate" => Ok(lash_core::facade_support::TurnCancelMode::Immediate),
        "after_step" => Ok(lash_core::facade_support::TurnCancelMode::AfterStep),
        other => Err(StoreError::Backend(format!(
            "unknown turn cancel mode `{other}`"
        ))),
    }
}

pub(super) fn turn_cancel_disposition_from_wire(
    disposition: &str,
) -> Result<lash_core::TurnCancelDisposition, StoreError> {
    match disposition {
        "defer" => Ok(lash_core::TurnCancelDisposition::Defer),
        "drop" => Ok(lash_core::TurnCancelDisposition::Drop),
        other => Err(StoreError::Backend(format!(
            "unknown turn cancel disposition `{other}`"
        ))),
    }
}

pub(super) fn turn_cancel_disposition_wire(
    disposition: lash_core::TurnCancelDisposition,
) -> &'static str {
    match disposition {
        lash_core::TurnCancelDisposition::Defer => "defer",
        lash_core::TurnCancelDisposition::Drop => "drop",
    }
}

pub(super) async fn append_turn_cancel_outcome_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    let arrays: Option<(Vec<String>, Vec<String>)> = sqlx::query_as(
        "SELECT affected_input_ids, affected_dispositions
         FROM lash_turn_cancel_requests
         WHERE session_id = $1 AND turn_id = $2 FOR UPDATE",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some((affected_input_ids, affected_dispositions)) = arrays else {
        return Ok(());
    };
    if affected_input_ids.len() != affected_dispositions.len() {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message: format!(
                "affected input/disposition cardinality differs: {} input ids, {} dispositions",
                affected_input_ids.len(),
                affected_dispositions.len()
            ),
        });
    }
    sqlx::query(
        "UPDATE lash_turn_cancel_requests
         SET affected_input_ids = array_append(affected_input_ids, $3),
             affected_dispositions = array_append(affected_dispositions, $4)
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(&affected.input_id)
    .bind(turn_cancel_disposition_wire(affected.disposition))
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

pub(super) async fn reconcile_turn_cancel_winner_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    observed: &lash_core::TurnCancelIntentSnapshot,
    evidence: &lash_core::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_tx(tx, session_id, turn_id).await?;
    if actual != *observed {
        return Ok(false);
    }
    let request = lash_core::facade_support::TurnCancelRequest {
        address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    };
    let revision = match actual {
        lash_core::TurnCancelIntentSnapshot::Absent => 1,
        lash_core::TurnCancelIntentSnapshot::Present {
            request: ref prior,
            revision,
        } if prior == &request => revision,
        lash_core::TurnCancelIntentSnapshot::Present { revision, .. } => {
            StoreError::checked_monotonic_increment("turn_cancel_intent_revision", revision)?
        }
    };
    let revision = i64::try_from(revision).map_err(|_| {
        StoreError::Backend("turn cancel intent revision exceeds PostgreSQL BIGINT".to_string())
    })?;
    sqlx::query(
        "INSERT INTO lash_turn_cancel_requests (
             session_id, turn_id, request_id, origin, reason, disposition, mode, intent_revision
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (session_id, turn_id) DO UPDATE
         SET request_id = EXCLUDED.request_id,
             origin = EXCLUDED.origin,
             reason = EXCLUDED.reason,
             disposition = EXCLUDED.disposition,
             mode = EXCLUDED.mode,
             intent_revision = EXCLUDED.intent_revision",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(&evidence.request_id)
    .bind(&evidence.origin)
    .bind(&evidence.reason)
    .bind(turn_cancel_disposition_wire(evidence.undelivered))
    .bind(turn_cancel_mode_wire(evidence.mode))
    .bind(revision)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(true)
}

pub(super) async fn orphaned_active_turn_ids_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    live_generation: u64,
    scope: lash_core::OrphanedTurnInputScope<'_>,
) -> Result<Vec<TurnId>, StoreError> {
    let rows: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT state, ingress_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND state = ANY($2) ORDER BY enqueue_seq ASC
         FOR UPDATE",
    )
    .bind(session_id.as_str())
    .bind(
        [
            lash_core::TurnInputState::PendingActive.as_str(),
            lash_core::TurnInputState::Accepted.as_str(),
        ]
        .as_slice(),
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut turn_ids = std::collections::BTreeSet::new();
    for (state, ingress_json, claim_token, claim_generation) in rows {
        let state = lash_core::TurnInputState::from_wire_str(&state)
            .ok_or_else(|| StoreError::Backend(format!("unknown turn-input state `{state}`")))?;
        let ingress: lash_core::TurnInputIngress =
            store_decode_json(&ingress_json, "turn-input ingress")?;
        let claim_generation = u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            claim_generation,
        )?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            state,
            &ingress,
            claim_token.is_some(),
            claim_generation,
        ) {
            let turn_id = ingress.active_turn_id().ok_or_else(|| {
                StoreError::Backend("active-turn input has no active turn id".to_string())
            })?;
            turn_ids.insert(turn_id.clone());
        }
    }
    Ok(turn_ids.into_iter().collect())
}

pub(super) async fn repair_orphaned_active_turn_inputs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    live_generation: u64,
    turn_id: &TurnId,
    observed: &lash_core::TurnCancelIntentSnapshot,
    settlement: Option<&lash_core::TurnCancelClosureSettlement>,
) -> Result<lash_core::TurnCancelRepairResult, StoreError> {
    if load_turn_cancel_intent_snapshot_tx(tx, session_id, turn_id).await? != *observed {
        return Ok(lash_core::TurnCancelRepairResult::IntentChanged);
    }
    if let Some(evidence) =
        settlement.and_then(lash_core::TurnCancelClosureSettlement::base_cancellation)
        && !reconcile_turn_cancel_winner_tx(tx, session_id, turn_id, observed, evidence).await?
    {
        return Ok(lash_core::TurnCancelRepairResult::IntentChanged);
    }
    let rows: Vec<(String, String, String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT input_id, state, ingress_json, input_json, claim_token, claim_session_lease_generation
         FROM lash_pending_turn_inputs
         WHERE session_id = $1 AND state = ANY($2) ORDER BY enqueue_seq ASC
         FOR UPDATE",
    )
    .bind(session_id.as_str())
    .bind(
        [
            lash_core::TurnInputState::PendingActive.as_str(),
            lash_core::TurnInputState::Accepted.as_str(),
        ]
        .as_slice(),
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let scope = lash_core::OrphanedTurnInputScope::Turn(turn_id);
    let effective =
        settlement.and_then(lash_core::TurnCancelClosureSettlement::effective_cancellation);
    let disposition = effective.map_or(lash_core::TurnCancelDisposition::Defer, |e| e.undelivered);
    let mut repairable = Vec::new();
    for (input_id, state, ingress_json, input_json, claim_token, claim_generation) in rows {
        let state = lash_core::TurnInputState::from_wire_str(&state)
            .ok_or_else(|| StoreError::Backend(format!("unknown turn-input state `{state}`")))?;
        let ingress: lash_core::TurnInputIngress =
            store_decode_json(&ingress_json, "turn-input ingress")?;
        let claim_generation = u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            claim_generation,
        )?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            state,
            &ingress,
            claim_token.is_some(),
            claim_generation,
        ) {
            repairable.push((input_id, store_decode_json(&input_json, "turn input")?));
        }
    }
    if repairable.is_empty() {
        return Ok(lash_core::TurnCancelRepairResult::Applied(
            Default::default(),
        ));
    }
    let next_turn_ingress = encode_json(&lash_core::TurnInputIngress::NextTurn)?;
    let mut outcome = lash_core::TurnCancelInputOutcome::default();
    for (input_id, payload) in repairable {
        sqlx::query(
            "UPDATE lash_pending_turn_inputs
         SET state = $3,
             ingress_json = COALESCE($4, ingress_json),
             claim_id = NULL,
             claim_owner_id = NULL,
             claim_owner_incarnation_id = NULL,
             claim_token = NULL,
             claim_session_lease_generation = 0
         WHERE session_id = $1 AND input_id = $2",
        )
        .bind(session_id.as_str())
        .bind(&input_id)
        .bind(match disposition {
            lash_core::TurnCancelDisposition::Defer => {
                lash_core::TurnInputState::DeferredNextTurn.as_str()
            }
            lash_core::TurnCancelDisposition::Drop => lash_core::TurnInputState::Cancelled.as_str(),
        })
        .bind(match disposition {
            lash_core::TurnCancelDisposition::Defer => Some(next_turn_ingress.as_str()),
            lash_core::TurnCancelDisposition::Drop => None,
        })
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let affected = lash_core::TurnCancelAffectedInput {
            input_id,
            payload,
            disposition,
        };
        if effective.is_some() {
            append_turn_cancel_outcome_tx(tx, session_id, turn_id, affected.clone()).await?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(lash_core::TurnCancelRepairResult::Applied(outcome))
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_pending_turn_inputs_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<ClaimTransactionOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    let active_turn = matches!(mode, lash_core::TurnInputClaimMode::ActiveTurn { .. });
    let wanted_state = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
            lash_core::TurnInputState::PendingActive
        }
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT {PENDING_TURN_INPUT_COLUMNS}
         FROM lash_pending_turn_inputs
         WHERE session_id = "
    ));
    let accepted_state =
        lash_core::store_backend_support::state_sql_literal(lash_core::TurnInputState::Accepted);
    query
        .push_bind(session_id.as_str())
        .push(" AND (state = ")
        .push_bind(wanted_state.as_str())
        .push(" OR (")
        .push_bind(active_turn)
        .push(" AND state = ")
        .push(accepted_state)
        .push("))")
        .push(
            "
           AND (
                claim_token IS NULL
                OR claim_session_lease_generation <> ",
        )
        .push_bind(sql_session_lease_generation(generation)?)
        .push("\n           )");
    if let lash_core::TurnInputClaimMode::ActiveTurn {
        turn_id,
        checkpoint,
    } = &mode
    {
        query
            .push(" AND ingress_json::jsonb ->> 'scope' = 'active_turn'")
            .push(" AND ingress_json::jsonb ->> 'turn_id' = ")
            .push_bind(turn_id.as_str());
        query.push(format!(
            " AND {}",
            lash_core::store_backend_support::admitted_min_boundary_sql(
                "ingress_json::jsonb ->> 'min_boundary'",
                *checkpoint,
            )
        ));
    }
    query
        .push(" ORDER BY enqueue_seq ASC LIMIT ")
        .push_bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
        .push(" FOR UPDATE SKIP LOCKED");
    let rows = query
        .build()
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let selected = rows
        .into_iter()
        .take(max_inputs)
        .map(|row| {
            let row = pending_turn_input_row(row)?;
            Ok((row.clone(), pending_turn_input_from_row(row)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let Some((head, _)) = selected.first() else {
        return Ok(ClaimTransactionOutcome::Commit(None));
    };
    let lease = TurnInputClaimLease::derive(head, session_id, owner, now, generation)?;
    let sql_fencing_tokens = sql_claim_fencing_tokens(
        "turn_input_claim_fencing_token",
        selected.iter().map(|(row, _)| row.claim_fencing_token),
    )?;
    let state_after_claim = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => lash_core::TurnInputState::Accepted,
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let mut inputs = Vec::new();
    for ((row, mut input), sql_fencing_token) in selected.into_iter().zip(sql_fencing_tokens) {
        let changed = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = $3,
                 claim_id = $4,
                 claim_owner_id = $5,
                 claim_owner_incarnation_id = $6,
                 claim_token = $7,
                 claim_fencing_token = $9,
                 claim_session_lease_generation = $8
             WHERE session_id = $1
               AND input_id = $2
               AND (
                    claim_token IS NULL
                    OR claim_session_lease_generation <> $8
               )",
        )
        .bind(session_id.as_str())
        .bind(&row.input_id)
        .bind(state_after_claim.as_str())
        .bind(&lease.claim_id)
        .bind(&owner.owner_id)
        .bind(&owner.incarnation_id)
        .bind(&lease.lease_token)
        .bind(sql_session_lease_generation(
            lease.session_lease_generation,
        )?)
        .bind(sql_fencing_token)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if changed == 0 {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        input.state = state_after_claim;
        inputs.push(input);
    }
    Ok(ClaimTransactionOutcome::Commit(Some(
        lash_core::TurnInputClaim {
            session_id: SessionId::from(session_id.to_string()),
            claim_id: lease.claim_id,
            owner: owner.clone(),
            lease_token: lease.lease_token,
            fencing_token: lease.fencing_token,
            session_lease_generation: lease.session_lease_generation,
            data: lash_core::runtime::TurnInputClaimData {
                mode,
                inputs,
                applications: Vec::new(),
            },
        },
    )))
}

pub(super) async fn claim_pending_turn_inputs_postgres(
    pool: &PgPool,
    #[cfg(any(test, feature = "testing"))] lease_clock: Option<&Arc<dyn lash_core::Clock>>,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    #[cfg(any(test, feature = "testing"))]
    super::test_support::set_transaction_lease_clock_for_testing(&mut tx, lease_clock).await?;
    ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
    match claim_pending_turn_inputs_postgres_tx(
        &mut tx,
        session_id,
        session_execution_lease,
        owner,
        max_inputs,
        mode,
    )
    .await?
    {
        ClaimTransactionOutcome::Commit(value) => {
            tx.commit().await.map_err(store_sqlx_error)?;
            Ok(value)
        }
        ClaimTransactionOutcome::Rollback(value) => {
            tx.rollback().await.map_err(store_sqlx_error)?;
            Ok(value)
        }
    }
}

/// Read the lease row without locking it, for diagnostics.
///
/// The mutation paths deliberately take a `FOR UPDATE` row lock (see
/// [`load_session_execution_lease_tx`]) because check-then-act on this row is not
/// atomic under READ COMMITTED. A diagnostic read must never take that lock: an
/// operator polling a stuck session would otherwise make the holder's renewal or
/// a peer's claim wait behind the observer's transaction, so watching the lease
/// could itself delay the lane it is watching. The caller owns a short read
/// transaction so the row and `transaction_timestamp()` share one transaction;
/// this SELECT itself still has no `FOR UPDATE`.
pub(crate) async fn read_session_execution_lease_unlocked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_executor_id, lease_term_ms
         FROM lash_session_execution_leases
         WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(session_execution_lease_row_from_columns)
        .transpose()
}

pub(crate) async fn load_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_executor_id, lease_term_ms
         FROM lash_session_execution_leases
         WHERE session_id = $1
         FOR UPDATE",
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(session_execution_lease_row_from_columns)
        .transpose()
}

pub(super) fn session_execution_lease_row_from_columns(
    row: sqlx::postgres::PgRow,
) -> Result<SessionExecutionLeaseRow, StoreError> {
    Ok(SessionExecutionLeaseRow {
        owner: lease_owner_from_columns(row.get(0), row.get(5))?,
        executor_id: row.get(6),
        lease_token: row.get(1),
        fencing_token: u64_from_sql("SessionExecutionLease", "fencing_token", row.get(2))?,
        claimed_at_ms: u64_from_sql("SessionExecutionLease", "claimed_at_ms", row.get(3))?,
        lease_term_ms: u64_from_sql("SessionExecutionLease", "lease_term_ms", row.get(7))?,
        expires_at_ms: u64_from_sql("SessionExecutionLease", "expires_at_ms", row.get(4))?,
    })
}

/// Serialize concurrent session-execution-lease claims for one session.
///
/// `try_claim`/`reclaim` read the current lease and then conditionally
/// `acquire` it. That check-then-act is not atomic under Postgres READ
/// COMMITTED, so two concurrent first claims can both observe no live lease and
/// both `ON CONFLICT DO UPDATE`, leaving two acquired winners. A
/// transaction-scoped advisory lock keyed by the session id makes the sequence
/// mutually exclusive per session; Postgres releases it automatically when the
/// transaction ends. (SQLite and the in-memory store serialize writers
/// globally, so they do not need this.)
pub(super) async fn lock_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0::bigint))")
        .bind(session_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

pub(super) async fn acquire_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    claim: lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity<'_>,
    previous_fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> Result<SessionExecutionLease, StoreError> {
    let lash_core::store_backend_support::SessionExecutionLeaseClaimIdentity {
        session_id,
        owner,
        executor_id,
        lease_token,
    } = claim;
    let fencing_token = StoreError::checked_monotonic_increment(
        "session_execution_lease_fencing_token",
        previous_fencing_token,
    )?;
    let sql_fencing_token = sql_monotonic_counter_value(
        "session_execution_lease_fencing_token",
        previous_fencing_token,
        fencing_token,
    )?;
    let expires_at = now.saturating_add(lease_ttl_ms);
    let sql_expires_at = sql_counter_value("session_execution_lease_expires_at_ms", expires_at)?;
    let sql_lease_term = sql_counter_value("session_execution_lease_term_ms", lease_ttl_ms)?;
    sqlx::query(
        "INSERT INTO lash_session_execution_leases (
            session_id, lease_owner_id, lease_owner_incarnation_id, lease_executor_id,
            lease_token, lease_fencing_token,
            lease_claimed_at_ms, lease_expires_at_ms, lease_term_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (session_id) DO UPDATE SET
            lease_owner_id = EXCLUDED.lease_owner_id,
            lease_owner_incarnation_id = EXCLUDED.lease_owner_incarnation_id,
            lease_executor_id = EXCLUDED.lease_executor_id,
            lease_token = EXCLUDED.lease_token,
            lease_fencing_token = EXCLUDED.lease_fencing_token,
            lease_claimed_at_ms = EXCLUDED.lease_claimed_at_ms,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms,
            lease_term_ms = EXCLUDED.lease_term_ms",
    )
    .bind(session_id.as_str())
    .bind(&owner.owner_id)
    .bind(&owner.incarnation_id)
    .bind(executor_id)
    .bind(lease_token)
    .bind(sql_fencing_token)
    .bind(now as i64)
    .bind(sql_expires_at)
    .bind(sql_lease_term)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(SessionExecutionLease {
        session_id: SessionId::from(session_id.to_string()),
        owner: owner.clone(),
        executor_id: executor_id.to_string(),
        lease_token: lease_token.to_string(),
        fencing_token,
        claimed_at_epoch_ms: now,
        lease_term_ms: lease_ttl_ms,
        expires_at_epoch_ms: expires_at,
    })
}

pub(super) async fn ensure_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    fence: &SessionExecutionLeaseAuthority,
) -> Result<(), StoreError> {
    let now = postgres_transaction_epoch_ms(tx).await?;
    let current = load_session_execution_lease_tx(tx, session_id).await?;
    lash_core::store_backend_support::require_current_session_execution_lease(
        session_id,
        current.as_ref().map(|current| {
            lash_core::store_backend_support::SessionExecutionLeaseFenceFacts {
                owner: current.owner.as_ref(),
                executor_id: current.executor_id.as_deref(),
                lease_token: current.lease_token.as_deref(),
                fencing_token: current.fencing_token,
                expires_at_epoch_ms: current.expires_at_ms,
            }
        }),
        fence,
        now,
    )
}

pub(super) async fn release_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completion: &SessionExecutionLeaseAuthority,
) -> Result<bool, StoreError> {
    let released = sqlx::query(
        "UPDATE lash_session_execution_leases
         SET lease_owner_id = NULL,
             lease_owner_incarnation_id = NULL,
             lease_executor_id = NULL,
             lease_token = NULL,
             lease_claimed_at_ms = 0,
             lease_term_ms = 0,
             lease_expires_at_ms = 0
         WHERE session_id = $1
           AND lease_owner_id = $2
           AND lease_owner_incarnation_id = $3
           AND lease_executor_id = $4
           AND lease_token = $5",
    )
    .bind(completion.session_id.as_str())
    .bind(&completion.owner.owner_id)
    .bind(&completion.owner.incarnation_id)
    .bind(&completion.executor_id)
    .bind(&completion.lease_token)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(released.rows_affected() == 1)
}

pub(super) fn requested_append_ancestor(stamp: &lash_core::RuntimeTurnCommitStamp) -> Option<&str> {
    match &stamp.append_request_identity {
        lash_core::AppendRequestIdentity::Append {
            requested_ancestor_node_id,
            ..
        } => requested_ancestor_node_id.as_deref(),
        lash_core::AppendRequestIdentity::PlainCommit
        | lash_core::AppendRequestIdentity::SemanticBoundary { .. } => None,
    }
}

pub(super) type AppendIdentityColumns<'a> = (Option<&'a str>, Option<i64>, Option<i32>);

pub(super) fn append_identity_columns(
    identity: &lash_core::AppendRequestIdentity,
) -> Result<AppendIdentityColumns<'_>, StoreError> {
    // A semantic-boundary identity persists without a node count; the NULL
    // count is what distinguishes its family on decode (FIG-2480).
    let (encoding_version, request_hash, requested_node_count) = match identity {
        lash_core::AppendRequestIdentity::PlainCommit => return Ok((None, None, None)),
        lash_core::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            *encoding_version,
            request_hash.as_str(),
            Some(*requested_node_count as i64),
        ),
        lash_core::AppendRequestIdentity::SemanticBoundary {
            operation: _,
            encoding_version,
            request_hash,
        } => (*encoding_version, request_hash.as_str(), None),
    };
    let encoding_version =
        i32::try_from(encoding_version).map_err(|_| StoreError::RecordEncodingFailed {
            record_kind: "RuntimeCommitReceipt append identity".to_string(),
            message: format!(
                "identity_encoding_version `{}` does not fit PostgreSQL INTEGER",
                encoding_version
            ),
        })?;
    Ok((
        Some(request_hash),
        requested_node_count,
        Some(encoding_version),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_identity_columns_refuse_encoding_versions_that_do_not_fit_postgres_integer() {
        let identity = lash_core::AppendRequestIdentity::Append {
            encoding_version: i32::MAX as u32 + 1,
            request_hash: "request-hash".to_string(),
            requested_node_count: 1,
            requested_ancestor_node_id: None,
        };

        let error = append_identity_columns(&identity)
            .expect_err("out-of-range encoding version must be refused");
        match error {
            StoreError::RecordEncodingFailed {
                record_kind,
                message,
            } => {
                assert_eq!(record_kind, "RuntimeCommitReceipt append identity");
                assert_eq!(
                    message,
                    "identity_encoding_version `2147483648` does not fit PostgreSQL INTEGER"
                );
            }
            other => panic!("expected typed record encoding failure, got {other:?}"),
        }
    }
}
