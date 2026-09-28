use super::*;

/// Admit the root's turn-lane run and record it with its rows, bindings and
/// base, in one transaction ([`RootStore::admit_root`]).
///
/// [`RootStore::admit_root`]: lash_core_execution::store::RootStore::admit_root
pub(crate) async fn admit_root_postgres(
    store: &crate::PostgresSessionStore,
    request: &lash_core_execution::store::AdmitRootRequest,
) -> Result<Option<lash_core_execution::store::RootAdmission>, StoreError> {
    use lash_core_execution::store::{AdmittedHead, RootAdmission};
    let mut connection = acquire_runtime_connection(&store.pool).await?;
    let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    ensure_session_execution_lease_tx(&mut tx, &request.session_id, &request.lease).await?;
    let roots = crate::session_roots::session_roots_sql();
    let existing: Option<Option<String>> = sqlx::query_scalar(roots.roots.select_admission.sql())
        .bind(request.session_id.as_str())
        .bind(request.root.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(Some(json)) = existing {
        let admission = crate::session_roots::decode_root_admission(&json)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(Some(admission));
    }
    if let Some(unfinished) =
        crate::session_roots::unfinished_root_conn(&mut tx, &request.session_id).await?
    {
        return Err(StoreError::UnfinishedRootConflict {
            session_id: request.session_id.clone(),
            root: unfinished.root,
        });
    }
    let composed = match &request.head {
        AdmittedHead::Input(head) => match claim_pending_turn_inputs_postgres_tx(
            &mut tx,
            &request.session_id,
            &request.lease,
            &request.owner,
            request.max_inputs,
            lash_core_execution::TurnInputClaimMode::NextTurn,
            CommandLaneGate::AdmittedRoot,
        )
        .await?
        {
            ClaimTransactionOutcome::Commit(Some(claim))
                if claim.inputs.iter().any(|input| input.input_id == *head) =>
            {
                Some((Some(Box::new(claim)), None))
            }
            _ => None,
        },
        AdmittedHead::Batch(head) => match claim_ready_queued_work_postgres_tx(
            &mut tx,
            &request.session_id,
            &request.lease,
            &request.owner,
            QueuedWorkClaimBoundary::Idle,
            request.policy.clone(),
        )
        .await?
        {
            ClaimTransactionOutcome::Commit(Some(claim))
                if claim.batches.iter().any(|batch| batch.batch_id == *head) =>
            {
                Some((None, Some(Box::new(claim))))
            }
            _ => None,
        },
    };
    // A composition that misses the head takes nothing.
    let Some((inputs, queued)) = composed else {
        tx.rollback().await.map_err(store_sqlx_error)?;
        return Ok(None);
    };
    let mut base = request.base.clone();
    base.generation =
        read_session_state_version_tx(&mut tx, &request.session_id, true, store.fleet_format)
            .await?;
    sqlx::query(session_sql().meta.retain_admission_base.sql())
        .bind(request.session_id.as_str())
        .bind(base.checkpoint.as_ref().map(|blob_ref| blob_ref.as_str()))
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let admission = RootAdmission {
        head: request.head.clone(),
        inputs,
        queued,
        base,
        turn_index: request.turn_index,
        generation: request.generation.clone(),
    };
    crate::session_roots::bind_root_inputs_conn(
        &mut tx,
        &request.session_id,
        &request.root,
        &admission.input_ids(),
    )
    .await?;
    let json = serde_json::to_string(&admission)
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    let changed = sqlx::query(roots.roots.write_admission.sql())
        .bind(request.session_id.as_str())
        .bind(request.root.as_str())
        .bind(json)
        .bind(request.admitted_generation.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if changed != 1 {
        return Err(StoreError::Backend(
            "root admission was already recorded".into(),
        ));
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(admission))
}

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
    checkpoint: lash_core_execution::CheckpointKind,
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
        lash_core_execution::CheckpointKind::AfterWork => {
            family.checkpoint_work_pending_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => {
            family.checkpoint_work_pending_before_completion.sql()
        }
    };
    sqlx::query_scalar(sql)
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(turn_id.as_str())
        .bind(max_inputs as i64)
        .bind(max_batches as i64)
        .fetch_one(&mut *connection)
        .await
        .map_err(store_sqlx_error)
}

/// Name the refusal behind an empty candidate scan.
///
/// The candidate query enforces the delivery-boundary rule in SQL, so a scan
/// that comes back empty tells the shared claim state machine nothing. Asking
/// it again with the unfiltered head keeps the classification in one place:
/// whatever the head alone is refused for is what this claim is refused for.
/// The probe runs only on a refusal.
pub(super) async fn postgres_refusal_for_empty_scan(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
) -> Result<TurnWorkEmptyScanDiagnostic, StoreError> {
    let now = postgres_transaction_epoch_ms(tx).await?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let head_rows = sqlx::query(sql.queued_batches.select_head_candidate.sql())
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(&owner.incarnation_id)
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
    lash_core_execution::store::claim_plan::classify_empty_claim_scan(
        &head_candidates,
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
            lash_core_execution::store::claim_plan::QueuedWorkClaimRow {
                candidate: candidates[index].clone(),
                batch,
                claim_token: row.claim_token.clone(),
                claim_session_lease_generation: row.claim_session_lease_generation,
                claim_owner_incarnation_id: row.claim_owner_incarnation_id.clone(),
            }
        })
        .collect::<Vec<_>>();
    let plan = match lash_core_execution::store::claim_plan::plan_queued_work_claim(
        lash_core_execution::store::queued_work::ClaimIdDialect::QueuedWork,
        session_id,
        owner,
        generation,
        now,
        observations,
        candidates,
    )? {
        // Empty commits and Defer rolls back: the claim transaction carries
        // the same meaning the hand-written loop did (FIG-1065).
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Empty => {
            return Ok(ClaimTransactionOutcome::Commit(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
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
        .bind(i64::try_from(now).unwrap_or(i64::MAX))
        .bind(&plan.owner().incarnation_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly as
        // this site always did — the claim transaction rolls back and no claim
        // is reported.
        if !lash_core_execution::store_backend_support::fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::QueuedWorkClaimAcquisition,
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
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
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
        .bind(sql_session_lease_generation(generation)?)
        .bind(claim_scan_limit(max_rows))
        .bind(&owner.incarnation_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    // The scan's SQL predicate and this filter are the same question, and the
    // shared verdict is the one answer to it: a row already claimed by the
    // claiming generation is not a candidate (ADR 0029).
    let mut selected = Vec::new();
    for row in rows {
        let row = queued_batch_row(row)?;
        if lash_core_execution::store_backend_support::queued_work_batch_claimability(
            row.claim_facts(),
            generation,
            &owner.incarnation_id,
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

/// The `enqueue_seq` of session `session_id`'s earliest next-turn input that
/// `generation` has not claimed: the turn-lane head of the input table.
pub(super) async fn earliest_next_turn_candidate_seq_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
) -> Result<Option<u64>, StoreError> {
    earliest_candidate_seq_tx(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .earliest_next_turn_candidate_seq
            .sql(),
        session_id,
        generation,
        owner,
    )
    .await
}

async fn earliest_candidate_seq_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &'static str,
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
) -> Result<Option<u64>, StoreError> {
    let seq: Option<i64> = sqlx::query_scalar(sql)
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(&owner.incarnation_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    seq.map(|seq| u64_from_sql("turn_lane", "enqueue_seq", seq))
        .transpose()
}

pub(super) async fn claim_ready_queued_work_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    session_execution_lease: &ClaimAuthority,
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
        session_id,
        generation,
        owner,
        boundary,
        policy.max_rows,
    )
    .await?;
    // ADR 0101 §5: queued work accepted after an unclaimed next-turn input
    // waits behind it, at idle and at a checkpoint alike. Read after the scan:
    // an input committed before a scanned row took its sequence first, so
    // this read sees it.
    let admitted = TurnLaneStop::before(
        earliest_next_turn_candidate_seq_tx(tx, session_id, generation, owner).await?,
    )
    .queued_prefix(&candidates);
    let candidates = &candidates[..admitted];
    let selected_len = match select_turn_work_claim_prefix(candidates, boundary, &policy, now)? {
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
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
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
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
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
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
    let Some((request_id, origin, reason, disposition, mode, revision)) = row else {
        return Ok(lash_core_execution::TurnCancelIntentSnapshot::Absent);
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
    Ok(lash_core_execution::TurnCancelIntentSnapshot::Present {
        request: lash_core_execution::facade_support::TurnCancelRequest {
            address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
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
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
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
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
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
    let affected_rows: Vec<TurnCancelAffectedRow> = sqlx::query_as(
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
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
    load_turn_cancel_request_in_tx(tx, session_id, turn_id, true).await
}

/// One `lash_turn_cancel_requests` row: request id, origin, reason,
/// disposition, mode.
pub(super) type TurnCancelRequestRow = (String, Option<String>, Option<String>, String, String);

/// One `lash_turn_cancel_affected_inputs` row: item id, payload, disposition,
/// item kind and — for a held wake — its batch.
pub(super) type TurnCancelAffectedRow = (String, String, String, String, Option<String>);

const AFFECTED_INPUT_KIND: &str = "input";
const AFFECTED_WAKE_KIND: &str = "process_wake";

pub(super) fn turn_cancel_record_from_rows(
    session_id: &SessionId,
    turn_id: &TurnId,
    row: TurnCancelRequestRow,
    affected_rows: Vec<TurnCancelAffectedRow>,
) -> Result<lash_core_execution::TurnCancelRequestRecord, StoreError> {
    let (request_id, origin, reason, disposition, mode) = row;
    let mut outcome = lash_core_execution::TurnCancelInputOutcome::default();
    for (item_id, payload_json, applied_disposition, item_kind, batch_id) in affected_rows {
        let applied_disposition = turn_cancel_disposition_from_wire(&applied_disposition)?;
        match (item_kind.as_str(), batch_id) {
            (AFFECTED_INPUT_KIND, None) => {
                outcome
                    .affected_inputs
                    .push(lash_core_execution::TurnCancelAffectedInput {
                        input_id: item_id.into(),
                        payload: store_decode_json(&payload_json, "turn input")?,
                        disposition: applied_disposition,
                    });
            }
            (AFFECTED_WAKE_KIND, Some(batch_id)) => {
                outcome
                    .affected_wakes
                    .push(lash_core_execution::TurnCancelAffectedWake {
                        batch_id: batch_id.into(),
                        item_id,
                        wake: store_decode_json(&payload_json, "process wake")?,
                        disposition: applied_disposition,
                    });
            }
            (other, _) => {
                return Err(StoreError::Backend(format!(
                    "malformed turn cancel affected item of kind `{other}`"
                )));
            }
        }
    }
    Ok(lash_core_execution::TurnCancelRequestRecord {
        request: lash_core_execution::facade_support::TurnCancelRequest {
            address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
            request_id,
            origin,
            reason,
            undelivered: turn_cancel_disposition_from_wire(&disposition)?,
            mode: turn_cancel_mode_from_wire(&mode)?,
        },
        outcome: (!outcome.is_empty()).then_some(outcome),
    })
}

pub(super) fn turn_cancel_mode_wire(
    mode: lash_core_execution::facade_support::TurnCancelMode,
) -> &'static str {
    match mode {
        lash_core_execution::facade_support::TurnCancelMode::Immediate => "immediate",
        lash_core_execution::facade_support::TurnCancelMode::AfterStep => "after_step",
    }
}

pub(super) fn turn_cancel_mode_from_wire(
    mode: &str,
) -> Result<lash_core_execution::facade_support::TurnCancelMode, StoreError> {
    match mode {
        "immediate" => Ok(lash_core_execution::facade_support::TurnCancelMode::Immediate),
        "after_step" => Ok(lash_core_execution::facade_support::TurnCancelMode::AfterStep),
        other => Err(StoreError::Backend(format!(
            "unknown turn cancel mode `{other}`"
        ))),
    }
}

pub(super) fn turn_cancel_disposition_from_wire(
    disposition: &str,
) -> Result<lash_core_execution::TurnCancelDisposition, StoreError> {
    match disposition {
        "defer" => Ok(lash_core_execution::TurnCancelDisposition::Defer),
        "drop" => Ok(lash_core_execution::TurnCancelDisposition::Drop),
        other => Err(StoreError::Backend(format!(
            "unknown turn cancel disposition `{other}`"
        ))),
    }
}

pub(super) fn turn_cancel_disposition_wire(
    disposition: lash_core_execution::TurnCancelDisposition,
) -> &'static str {
    match disposition {
        lash_core_execution::TurnCancelDisposition::Defer => "defer",
        lash_core_execution::TurnCancelDisposition::Drop => "drop",
    }
}

pub(super) async fn append_turn_cancel_outcome_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    if !lock_turn_cancel_request_tx(tx, session_id, turn_id).await? {
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
    .bind(AFFECTED_INPUT_KIND)
    .bind(None::<&str>)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Record one wake a turn cancel deferred on the cancellation (FIG-3543).
pub(super) async fn append_turn_cancel_wake_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: &lash_core_execution::TurnCancelAffectedWake,
) -> Result<(), StoreError> {
    if !lock_turn_cancel_request_tx(tx, session_id, turn_id).await? {
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
    .bind(&affected.item_id)
    .bind(turn_cancel_disposition_wire(affected.disposition))
    .bind(encode_json(&affected.wake)?)
    .bind(AFFECTED_WAKE_KIND)
    .bind(affected.batch_id.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Lock the cancel-request row so concurrent appends serialize on the
/// ordinal next-val; `false` when there is no request to attach evidence to.
async fn lock_turn_cancel_request_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<bool, StoreError> {
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
    Ok(request_exists.is_some())
}

pub(super) async fn reconcile_turn_cancel_winner_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &TurnId,
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_tx(tx, session_id, turn_id).await?;
    if actual != *observed {
        return Ok(false);
    }
    let request = lash_core_execution::facade_support::TurnCancelRequest {
        address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    };
    let revision = match actual {
        lash_core_execution::TurnCancelIntentSnapshot::Absent => 1,
        lash_core_execution::TurnCancelIntentSnapshot::Present {
            request: ref prior,
            revision,
        } if prior == &request => revision,
        lash_core_execution::TurnCancelIntentSnapshot::Present { revision, .. } => {
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
    scope: lash_core_execution::OrphanedTurnInputScope<'_>,
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
        let ingress: lash_core_execution::TurnInputIngress =
            store_decode_json(&ingress_json, "turn-input ingress")?;
        let state = lash_core_execution::TurnInputState::from_persisted(&state, ingress)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "unknown or scope-illegal turn-input state `{state}`"
                ))
            })?;
        let claim_generation = u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            claim_generation,
        )?;
        if lash_core_execution::store_backend_support::orphaned_active_turn_input_is_repairable(
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
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    settlement: Option<&lash_core_execution::TurnCancelClosureSettlement>,
) -> Result<lash_core_execution::TurnCancelRepairResult, StoreError> {
    if load_turn_cancel_intent_snapshot_tx(tx, session_id, turn_id).await? != *observed {
        return Ok(lash_core_execution::TurnCancelRepairResult::IntentChanged);
    }
    if let Some(evidence) =
        settlement.and_then(lash_core_execution::TurnCancelClosureSettlement::base_cancellation)
        && !reconcile_turn_cancel_winner_tx(tx, session_id, turn_id, observed, evidence).await?
    {
        return Ok(lash_core_execution::TurnCancelRepairResult::IntentChanged);
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    let rows = sqlx::query(sql.pending_inputs_postgres.select_active_turn_rows.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let scope = lash_core_execution::OrphanedTurnInputScope::Turn(turn_id);
    let effective = settlement
        .and_then(lash_core_execution::TurnCancelClosureSettlement::effective_cancellation);
    let disposition = effective.map_or(lash_core_execution::TurnCancelDisposition::Defer, |e| {
        e.undelivered
    });
    let mut repairable = Vec::new();
    for row in rows {
        let input_json: String = row.get("input_json");
        let row = pending_turn_input_row(row)?;
        if lash_core_execution::store_backend_support::orphaned_active_turn_input_is_repairable(
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
        return Ok(lash_core_execution::TurnCancelRepairResult::Applied(
            Default::default(),
        ));
    }
    let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
    let deferred_ingress = encode_json(&deferred.ingress())?;
    let mut outcome = lash_core_execution::TurnCancelInputOutcome::default();
    for (input_id, payload) in repairable {
        // Two dispositions, two named statements: deferring rewrites the
        // ingress so the row stops naming a turn that is over (FIG-1573),
        // dropping is the cancel this table already has. An optional
        // `COALESCE($N, ingress_json)` assignment carried both before.
        match disposition {
            lash_core_execution::TurnCancelDisposition::Defer => {
                sqlx::query(sql.pending_inputs.defer_to_next_turn.sql())
                    .bind(session_id.as_str())
                    .bind(&input_id)
                    .bind(deferred.as_str())
                    .bind(deferred_ingress.as_str())
            }
            lash_core_execution::TurnCancelDisposition::Drop => {
                sqlx::query(sql.pending_inputs.cancel.sql())
                    .bind(session_id.as_str())
                    .bind(&input_id)
                    .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
            }
        }
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        let affected = lash_core_execution::TurnCancelAffectedInput {
            input_id: input_id.into(),
            payload,
            disposition,
        };
        if effective.is_some() {
            append_turn_cancel_outcome_tx(tx, session_id, turn_id, affected.clone()).await?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(lash_core_execution::TurnCancelRepairResult::Applied(
        outcome,
    ))
}

/// Which command-lane gate a next-turn claim reads (ADR 0101 §4). A
/// checkpoint claim never consults the command lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommandLaneGate {
    /// A claim at a turn boundary: any open session command holds every
    /// turn-lane row back, because the command lane drains first.
    Boundary,
    /// The claim of an input root whose admission already chose the turn
    /// lane at a boundary with no open command: a command enqueued since
    /// holds back only the rows after it, so the root still reaches the head
    /// it was admitted for.
    AdmittedRoot,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn claim_pending_turn_inputs_postgres_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core_execution::TurnInputClaimMode,
    gate: CommandLaneGate,
) -> Result<ClaimTransactionOutcome<Option<lash_core_execution::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let follow_on_claim = match &mode {
        lash_core_execution::TurnInputClaimMode::ActiveTurn { turn_id, .. } => {
            lash_core_execution::store::FollowOnClaim::Checkpoint { turn_id }
        }
        lash_core_execution::TurnInputClaimMode::NextTurn => {
            lash_core_execution::store::FollowOnClaim::Idle
        }
    };
    if follow_on_blocks_claim_tx(tx, session_id, follow_on_claim).await? {
        return Ok(ClaimTransactionOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let now = postgres_transaction_epoch_ms(tx).await?;
    // One named statement per filter shape production takes, picked by an
    // exhaustive match: a next-turn scan per command-lane gate, and an
    // active-turn scan per checkpoint. The mode used to be spliced into one
    // query-builder statement with a bound `$N AND state = …` disjunct and an
    // interpolated boundary predicate, neither of which a planner can seek.
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let mut query = match (&mode, gate) {
        (lash_core_execution::TurnInputClaimMode::NextTurn, CommandLaneGate::Boundary) => {
            sqlx::query(statements.claim_candidates_next_turn.sql())
        }
        (lash_core_execution::TurnInputClaimMode::NextTurn, CommandLaneGate::AdmittedRoot) => {
            sqlx::query(statements.claim_candidates_admitted_root.sql())
        }
        (lash_core_execution::TurnInputClaimMode::ActiveTurn { checkpoint, .. }, _) => {
            match checkpoint {
                lash_core_execution::CheckpointKind::AfterWork => {
                    sqlx::query(statements.claim_candidates_active_turn_after_work.sql())
                }
                lash_core_execution::CheckpointKind::BeforeCompletion => sqlx::query(
                    statements
                        .claim_candidates_active_turn_before_completion
                        .sql(),
                ),
            }
        }
    };
    query = query
        .bind(session_id.as_str())
        .bind(sql_session_lease_generation(generation)?)
        .bind(i64::try_from(max_inputs).unwrap_or(i64::MAX));
    if let lash_core_execution::TurnInputClaimMode::ActiveTurn { turn_id, .. } = &mode {
        query = query.bind(turn_id.to_string());
    }
    query = query.bind(&owner.incarnation_id);
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
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    mode: lash_core_execution::TurnInputClaimMode,
    selected: Vec<(PendingTurnInputRow, lash_core_execution::PendingTurnInput)>,
) -> Result<ClaimTransactionOutcome<Option<lash_core_execution::TurnInputClaim>>, StoreError> {
    let generation = session_execution_lease.fencing_token;
    let observations = selected
        .into_iter()
        .map(
            |(row, input)| lash_core_execution::store::claim_plan::TurnInputClaimRow {
                input,
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                claim_token: row.claim_facts().claim_token.map(str::to_string),
                claim_session_lease_generation: row.claim_session_lease_generation(),
                claim_owner_incarnation_id: row
                    .claim_facts()
                    .claim_owner_incarnation_id
                    .map(str::to_string),
            },
        )
        .collect();
    let plan = match lash_core_execution::store::claim_plan::plan_turn_input_claim(
        lash_core_execution::store::queued_work::ClaimIdDialect::TurnInput,
        session_id,
        owner,
        generation,
        now,
        mode,
        observations,
    )? {
        // Empty commits and Defer rolls back: the claim transaction carries
        // the same meaning the hand-written loop did (FIG-1065).
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Empty => {
            return Ok(ClaimTransactionOutcome::Commit(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(ClaimTransactionOutcome::Rollback(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
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
        .bind(write.state_after_claim.as_str())
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
        .bind(i64::try_from(now).unwrap_or(i64::MAX))
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the whole claim transaction rolls back and
        // no claim is reported.
        if !lash_core_execution::store_backend_support::fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::TurnInputClaimAcquisition,
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
    #[cfg(any(test, feature = "testing"))] lease_clock: Option<
        &Arc<dyn lash_core_execution::Clock>,
    >,
    session_id: &SessionId,
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core_execution::TurnInputClaimMode,
) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
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
        CommandLaneGate::Boundary,
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

/// The follow-on the head of `session_id` owes (ADR 0101 §3), read inside
/// the caller's transaction under a row lock: `FOR UPDATE` for a writer that
/// decides against it, `FOR SHARE` for a claim.
pub(super) async fn pending_follow_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    for_update: bool,
) -> Result<Option<lash_core_execution::store::PendingFollowOn>, StoreError> {
    let statement = if for_update {
        crate::session_sql::session_sql()
            .head
            .select_pending_follow_on_for_update
            .sql()
    } else {
        crate::session_sql::session_sql()
            .head
            .select_pending_follow_on_for_share
            .sql()
    };
    let json = sqlx::query_scalar::<_, Option<String>>(statement)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .flatten();
    lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
        session_id,
        json.as_deref(),
    )
}

/// Whether the head's pending follow-on refuses `claim` (ADR 0101 §3): every
/// claim but the follow-on's own is blocked while it is set.
pub(super) async fn follow_on_blocks_claim_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    claim: lash_core_execution::store::FollowOnClaim<'_>,
) -> Result<bool, StoreError> {
    Ok(lash_core_execution::store::follow_on_blocks_claim(
        pending_follow_on_tx(tx, session_id, false).await?.as_ref(),
        claim,
    )
    .is_some())
}

pub(super) async fn ensure_session_execution_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    fence: &ClaimAuthority,
) -> Result<(), StoreError> {
    super::drive_epoch::require_fence_tx(tx, session_id, &fence.drive_fence()).await
}
pub(super) fn requested_append_ancestor(
    stamp: &lash_core_execution::RuntimeTurnCommitStamp,
) -> Option<&str> {
    match &stamp.append_request_identity {
        lash_core_execution::AppendRequestIdentity::Append {
            requested_ancestor_node_id,
            ..
        } => requested_ancestor_node_id.as_deref(),
        lash_core_execution::AppendRequestIdentity::PlainCommit
        | lash_core_execution::AppendRequestIdentity::SemanticBoundary { .. } => None,
    }
}

pub(super) type AppendIdentityColumns<'a> = (Option<&'a str>, Option<i64>, Option<i32>);

pub(super) fn append_identity_columns(
    identity: &lash_core_execution::AppendRequestIdentity,
) -> Result<AppendIdentityColumns<'_>, StoreError> {
    // A semantic-boundary identity persists without a node count; the NULL
    // count is what distinguishes its family on decode (FIG-2480).
    let (encoding_version, request_hash, requested_node_count) = match identity {
        lash_core_execution::AppendRequestIdentity::PlainCommit => return Ok((None, None, None)),
        lash_core_execution::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            *encoding_version,
            request_hash.as_str(),
            Some(*requested_node_count as i64),
        ),
        lash_core_execution::AppendRequestIdentity::SemanticBoundary {
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
        let identity = lash_core_execution::AppendRequestIdentity::Append {
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
