use super::*;

pub(super) enum ClaimTransactionOutcome<T> {
    Commit(T),
    Rollback(T),
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn checkpoint_work_pending_postgres(
    pool: &PgPool,
    injected_lease_epoch: Option<i64>,
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
    // One statement per checkpoint, chosen exhaustively: the admitted
    // minimum-boundary set is what the checkpoint decides, and an optional
    // predicate over a bound boundary cannot seek an index.
    let family = &crate::turn_ingress::turn_ingress_sql().family_postgres;
    let sql = match checkpoint {
        lash_core::CheckpointKind::AfterWork => family.checkpoint_work_pending_after_work.sql(),
        lash_core::CheckpointKind::BeforeCompletion => {
            family.checkpoint_work_pending_before_completion.sql()
        }
    };
    sqlx::query_scalar(sql)
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(turn_id.as_str())
        .bind(injected_lease_epoch)
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
    let sql = crate::turn_ingress::turn_ingress_sql();
    let head_rows = sqlx::query(sql.queued_batches_postgres.select_head_candidate.sql())
        .bind(session_id.as_str())
        .bind(now as i64)
        .bind(sql_session_lease_generation(generation)?)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let head_candidates = match head_rows.into_iter().next() {
        Some(head_row) => {
            let head_row = queued_batch_row(head_row)?;
            let head_batch = queued_work_batch_from_row(tx, head_row.clone()).await?;
            vec![claim_candidate_from_row(&head_row, &head_batch)]
        }
        None => Vec::new(),
    };
    let deferred_row_pending = head_candidates.is_empty()
        && sqlx::query_scalar::<_, bool>(sql.queued_batches_postgres.exists_deferred.sql())
            .bind(session_id.as_str())
            .bind(now as i64)
            .bind(sql_session_lease_generation(generation)?)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    lash_core::store::claim_plan::classify_empty_claim_scan(
        &head_candidates,
        deferred_row_pending,
        boundary,
        policy,
        now,
    )
}

// Exact selection passes its full validation span: validate every fencing
// token before writing, including candidates outside the selected prefix.
#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_queued_work_rows_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: u64,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    generation: u64,
    selected_rows: &[QueuedBatchRow],
    selected_batches: Vec<QueuedWorkBatch>,
    candidates: &[ClaimCandidate],
) -> Result<ClaimTransactionOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if selected_rows.len() != selected_batches.len() || selected_rows.len() > candidates.len() {
        return Err(StoreError::Backend(format!(
            "queued-work claim observed {} rows, {} batches, {} candidates",
            selected_rows.len(),
            selected_batches.len(),
            candidates.len(),
        )));
    }
    let observations = selected_rows
        .iter()
        .zip(selected_batches)
        .enumerate()
        .map(|(index, (row, batch))| {
            debug_assert_eq!(row.batch_id.as_str(), &*batch.batch_id);
            lash_core::store::claim_plan::QueuedWorkClaimRow {
                candidate: candidates[index].clone(),
                batch,
                claim_token: row.claim_token.clone(),
                claim_session_lease_generation: row.claim_session_lease_generation,
            }
        })
        .collect::<Vec<_>>();
    let plan = match lash_core::store::claim_plan::plan_queued_work_claim(
        lash_core::store::queued_work::ClaimIdDialect::QueuedWork,
        session_id,
        owner,
        generation,
        now,
        observations,
        candidates,
    )? {
        // Empty commits and Defer rolls back: the claim transaction carries
        // the same meaning the hand-written loop did (FIG-1065).
        lash_core::store::claim_plan::ClaimPlanDecision::Empty => {
            return Ok(ClaimTransactionOutcome::Commit(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
    };
    for write in plan.writes() {
        let changed = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .claim
                .sql(),
        )
        .bind(plan.session_id().as_str())
        .bind(write.batch_id.as_str())
        .bind(plan.claim_id())
        .bind(plan.lease_token())
        .bind(sql_session_lease_generation(
            plan.session_lease_generation(),
        )?)
        .bind(sql_counter_value(
            "queued_work_claim_fencing_token",
            write.next_claim_fencing_token,
        )?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly as
        // this site always did — the claim transaction rolls back and no claim
        // is reported.
        if !lash_core::store_backend_support::fenced_write_applied(
            lash_core::store_backend_support::FencedWrite::QueuedWorkClaimAcquisition,
            crate::POSTGRES_BACKEND,
            write.batch_id.as_str(),
            changed,
        ) {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
    }
    Ok(ClaimTransactionOutcome::Commit(Some(plan.into_claim()?)))
}

pub(super) async fn scan_queued_work_candidates_postgres(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: u64,
    session_id: &SessionId,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    max_rows: usize,
) -> Result<
    (
        Vec<QueuedBatchRow>,
        Vec<QueuedWorkBatch>,
        Vec<ClaimCandidate>,
    ),
    StoreError,
> {
    let rows = sqlx::query(postgres_queued_work_claim_candidates_sql(boundary))
        .bind(session_id.as_str())
        .bind(now as i64)
        .bind(sql_session_lease_generation(generation)?)
        .bind(claim_scan_limit(max_rows))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    // The scan's SQL predicate and this filter are the same question, and the
    // shared verdict is the one answer to it: a row already claimed by the
    // claiming generation is not a candidate (ADR 0029).
    let mut selected = Vec::new();
    for row in rows {
        let row = queued_batch_row(row)?;
        if lash_core::store_backend_support::queued_work_batch_claimability(
            row.claim_facts(),
            generation,
        )
        .is_claimable()
        {
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
    Ok((selected, selected_batches, candidates))
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
    let (selected_rows, mut selected_batches, candidates) = scan_queued_work_candidates_postgres(
        tx,
        now,
        session_id,
        generation,
        boundary,
        policy.max_rows,
    )
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
        &selected_rows[..selected_len],
        selected_batches,
        &candidates[..selected_len],
    )
    .await
}

/// Load one cancellation record. Affected-input payloads are receipt
/// snapshots on `lash_turn_cancel_affected_inputs`, so no cross-table
/// snapshot isolation is needed to keep the evidence whole.
pub(super) async fn load_turn_cancel_request_pg(
    pool: &sqlx::PgPool,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let mut connection = acquire_runtime_connection(pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    let record = load_turn_cancel_request_in_tx(&mut tx, session_id, turn_id, false).await?;
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
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .select_request_with_revision_for_update
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .select_request_with_revision
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut *connection)
    .await
    .map_err(store_sqlx_error)?;
    turn_cancel_snapshot_from_row(session_id, turn_id, row)
}

async fn load_turn_cancel_request_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    lock_request: bool,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().cancel_requests_postgres;
    let metadata_sql = if lock_request {
        statements.select_request_for_update.sql()
    } else {
        statements.select_request.sql()
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
    let affected_rows: Vec<(String, String, String)> = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs_postgres
            .select_by_turn
            .sql(),
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
    load_turn_cancel_request_in_tx(tx, session_id, turn_id, true).await
}

/// One `lash_turn_cancel_requests` row: request id, origin, reason,
/// disposition, mode.
pub(super) type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

pub(super) fn turn_cancel_record_from_rows(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: TurnCancelRequestRow,
    affected_rows: Vec<(String, String, String)>,
) -> Result<lash_core::TurnCancelRequestRecord, StoreError> {
    let (request_id, origin, reason, disposition, mode) = row;
    let mut affected_inputs = Vec::with_capacity(affected_rows.len());
    for (input_id, input_json, applied_disposition) in affected_rows {
        affected_inputs.push(lash_core::TurnCancelAffectedInput {
            input_id: input_id.into(),
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
    // Lock the request row so concurrent appends serialize on the ordinal
    // next-val; a missing request leaves no evidence to attach to.
    let request_exists: Option<i32> = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .lock_request
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if request_exists.is_none() {
        return Ok(());
    }
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_affected_inputs_postgres
            .append_at_next_ordinal
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(&*affected.input_id)
    .bind(turn_cancel_disposition_wire(affected.disposition))
    .bind(encode_json(&affected.payload)?)
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
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_postgres
            .upsert_record
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_active_turn_claims
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut turn_ids = std::collections::BTreeSet::new();
    for (state, ingress_json, claim_token, claim_generation) in rows {
        let ingress: lash_core::TurnInputIngress =
            store_decode_json(&ingress_json, "turn-input ingress")?;
        let state =
            lash_core::TurnInputState::from_persisted(&state, ingress).ok_or_else(|| {
                StoreError::Backend(format!(
                    "unknown or scope-illegal turn-input state `{state}`"
                ))
            })?;
        let claim_generation = u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            claim_generation,
        )?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            &state,
            claim_token.is_some(),
            claim_generation,
        ) {
            let turn_id = state.active_turn_id().ok_or_else(|| {
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
    let sql = crate::turn_ingress::turn_ingress_sql();
    let rows = sqlx::query(sql.pending_inputs_postgres.select_active_turn_rows.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let scope = lash_core::OrphanedTurnInputScope::Turn(turn_id);
    let effective =
        settlement.and_then(lash_core::TurnCancelClosureSettlement::effective_cancellation);
    let disposition = effective.map_or(lash_core::TurnCancelDisposition::Defer, |e| e.undelivered);
    let mut repairable = Vec::new();
    for row in rows {
        let input_json: String = row.get("input_json");
        let row = pending_turn_input_row(row)?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            row.state(),
            row.is_claimed(),
            row.claim_session_lease_generation(),
        ) {
            repairable.push((
                row.input_id.clone(),
                store_decode_json(&input_json, "turn input")?,
            ));
        }
    }
    if repairable.is_empty() {
        return Ok(lash_core::TurnCancelRepairResult::Applied(
            Default::default(),
        ));
    }
    let deferred = lash_core::TurnInputState::DeferredNextTurn;
    let deferred_ingress = encode_json(&deferred.ingress())?;
    let mut outcome = lash_core::TurnCancelInputOutcome::default();
    for (input_id, payload) in repairable {
        // Two dispositions, two named statements: deferring rewrites the
        // ingress so the row stops naming a turn that is over (FIG-1573),
        // dropping is the cancel this table already has. An optional
        // `COALESCE($N, ingress_json)` assignment carried both before.
        match disposition {
            lash_core::TurnCancelDisposition::Defer => {
                sqlx::query(sql.pending_inputs.defer_to_next_turn.sql())
                    .bind(session_id.as_str())
                    .bind(&input_id)
                    .bind(deferred.as_str())
                    .bind(deferred_ingress.as_str())
            }
            lash_core::TurnCancelDisposition::Drop => sqlx::query(sql.pending_inputs.cancel.sql())
                .bind(session_id.as_str())
                .bind(&input_id)
                .bind(lash_core::TurnInputStateKind::Cancelled.as_str()),
        }
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let affected = lash_core::TurnCancelAffectedInput {
            input_id: input_id.into(),
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
    // One named statement per filter shape production takes, picked by an
    // exhaustive match: a next-turn scan, and an active-turn scan per
    // checkpoint. The mode used to be spliced into one query-builder statement
    // with a bound `$N AND state = …` disjunct and an interpolated boundary
    // predicate, neither of which a planner can seek.
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let mut query = match &mode {
        lash_core::TurnInputClaimMode::NextTurn => {
            sqlx::query(statements.claim_candidates_next_turn.sql())
        }
        lash_core::TurnInputClaimMode::ActiveTurn { checkpoint, .. } => match checkpoint {
            lash_core::CheckpointKind::AfterWork => {
                sqlx::query(statements.claim_candidates_active_turn_after_work.sql())
            }
            lash_core::CheckpointKind::BeforeCompletion => sqlx::query(
                statements
                    .claim_candidates_active_turn_before_completion
                    .sql(),
            ),
        },
    };
    query = query
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(i64::try_from(max_inputs).unwrap_or(i64::MAX));
    if let lash_core::TurnInputClaimMode::ActiveTurn { turn_id, .. } = &mode {
        query = query.bind(turn_id.to_string());
    }
    let rows = query.fetch_all(&mut **tx).await.map_err(store_sqlx_error)?;
    let selected = rows
        .into_iter()
        .take(max_inputs)
        .map(|row| {
            let row = pending_turn_input_row(row)?;
            Ok((row.clone(), pending_turn_input_from_row(row)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    claim_turn_input_rows_postgres_tx(
        tx,
        now,
        session_id,
        session_execution_lease,
        owner,
        mode,
        selected,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_turn_input_rows_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    mode: lash_core::TurnInputClaimMode,
    selected: Vec<(PendingTurnInputRow, lash_core::PendingTurnInput)>,
) -> Result<ClaimTransactionOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    let generation = session_execution_lease.fencing_token;
    let observations = selected
        .into_iter()
        .map(
            |(row, input)| lash_core::store::claim_plan::TurnInputClaimRow {
                input,
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                claim_token: row.claim_facts().claim_token.map(str::to_string),
                claim_session_lease_generation: row.claim_session_lease_generation(),
            },
        )
        .collect();
    let plan = match lash_core::store::claim_plan::plan_turn_input_claim(
        lash_core::store::queued_work::ClaimIdDialect::TurnInput,
        session_id,
        owner,
        generation,
        now,
        mode,
        observations,
    )? {
        // Empty commits and Defer rolls back: the claim transaction carries
        // the same meaning the hand-written loop did (FIG-1065).
        lash_core::store::claim_plan::ClaimPlanDecision::Empty => {
            return Ok(ClaimTransactionOutcome::Commit(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
    };
    for write in plan.writes() {
        let changed = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .claim
                .sql(),
        )
        .bind(plan.session_id().as_str())
        .bind(write.input_id.as_str())
        .bind(plan.state_after_claim().as_str())
        .bind(plan.claim_id())
        .bind(&owner.owner_id)
        .bind(&owner.incarnation_id)
        .bind(plan.lease_token())
        .bind(sql_session_lease_generation(
            plan.session_lease_generation(),
        )?)
        .bind(sql_counter_value(
            "turn_input_claim_fencing_token",
            write.next_claim_fencing_token,
        )?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the whole claim transaction rolls back and
        // no claim is reported.
        if !lash_core::store_backend_support::fenced_write_applied(
            lash_core::store_backend_support::FencedWrite::TurnInputClaimAcquisition,
            crate::POSTGRES_BACKEND,
            write.input_id.as_str(),
            changed,
        ) {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
    }
    Ok(ClaimTransactionOutcome::Commit(Some(plan.into_claim())))
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
        mode.clone(),
    )
    .await?
    {
        ClaimTransactionOutcome::Commit(value) => {
            if let lash_core::TurnInputClaimMode::ActiveTurn { turn_id, .. } = &mode {
                super::queued_run_assignment::assign_checkpoint_members_tx(
                    &mut tx,
                    session_id,
                    turn_id,
                    value.as_ref(),
                    None,
                )
                .await?;
            }
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
        crate::turn_ingress::turn_ingress_sql()
            .leases
            .select_by_session
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .leases_postgres
            .select_by_session_for_update
            .sql(),
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
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
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
    sqlx::query(crate::turn_ingress::turn_ingress_sql().leases.acquire.sql())
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
    let released = sqlx::query(crate::turn_ingress::turn_ingress_sql().leases.release.sql())
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
