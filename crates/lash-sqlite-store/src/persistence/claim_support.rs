use super::*;

pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
) -> Result<lash_core::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state.kind() {
        lash_core::TurnInputStateKind::Cancelled => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input),
        ),
        lash_core::TurnInputStateKind::Completed => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(input),
        ),
        lash_core::TurnInputStateKind::PendingActive
        | lash_core::TurnInputStateKind::DeferredNextTurn
        | lash_core::TurnInputStateKind::Accepted => {
            // A claim is live only while the session-execution-lease generation it
            // pins still holds the session lease (ADR 0029).
            let live_claim = row.claim_token.is_some()
                && load_session_execution_lease_row_conn(conn, &row.session_id)?.is_some_and(
                    |lease| {
                        lease.lease_token.is_some()
                            && lease.expires_at_ms > now_epoch_ms
                            && lease.fencing_token == row.claim_session_lease_generation
                    },
                );
            if live_claim {
                return Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state.clone()),
                    input,
                });
            }
            let run_owns_input: bool = conn
                .query_row(
                    crate::turn_ingress::turn_ingress_sql()
                        .queued_runs
                        .pending_member
                        .sql(),
                    params![row.session_id.as_str(), "input", row.input_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            if run_owns_input {
                return Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                    claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state.clone()),
                    input,
                });
            }
            conn.execute(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .cancel
                    .sql(),
                params![
                    row.session_id.as_str(),
                    row.input_id.as_str(),
                    lash_core::TurnInputStateKind::Cancelled.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
            input.state = lash_core::TurnInputState::Cancelled(input.state.ingress());
            Ok(lash_core::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn checkpoint_work_pending_sqlite(
    conn: &SqliteConnection,
    now: u64,
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
    let session_id = SessionId::from(session_id.to_string());
    let turn_id = TurnId::from(turn_id.to_string());
    conn.call(move |conn| {
        let outcome: Result<bool, StoreError> = (|| {
            let family = &crate::turn_ingress::turn_ingress_sql().family_sqlite;
            // One statement per checkpoint, chosen exhaustively: the admitted
            // minimum-boundary set is what the checkpoint decides, and an
            // optional predicate over a bound boundary cannot seek an index.
            let sql = match checkpoint {
                lash_core::CheckpointKind::AfterWork => {
                    family.checkpoint_work_pending_after_work.sql()
                }
                lash_core::CheckpointKind::BeforeCompletion => {
                    family.checkpoint_work_pending_before_completion.sql()
                }
            };
            let pending: i64 = conn
                .query_row(
                    sql,
                    params![
                        session_id.as_str(),
                        now as i64,
                        sql_session_lease_generation(generation)?,
                        turn_id.as_str(),
                        max_inputs as i64,
                        max_batches as i64,
                    ],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            Ok(pending != 0)
        })();
        Ok(outcome)
    })
    .await
    .map_err(sqlite_error)?
}

#[allow(clippy::too_many_arguments)]
/// Name the refusal behind an empty candidate scan.
///
/// The candidate query enforces the delivery-boundary rule in SQL, so a scan
/// that comes back empty tells the shared claim state machine nothing. Asking
/// it again with the unfiltered ready head keeps the classification in one
/// place: whatever the head alone is refused for is what this claim is refused
/// for. With no ready head at all, a lane still holding deferred work is not an
/// exhausted lane. Both probes run only on a refusal, so a successful claim
/// pays nothing for them.
pub(super) fn sqlite_refusal_for_empty_scan(
    tx: &Connection,
    session_id: &SessionId,
    now: u64,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
) -> Result<TurnWorkEmptyScanDiagnostic, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let head_rows = {
        let mut stmt = tx
            .prepare(sql.queued_batches_sqlite.select_head_candidate.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let head_candidates = if head_rows.is_empty() {
        Vec::new()
    } else {
        let head_batches = queued_work_batches_from_conn(tx, &head_rows)?;
        head_rows
            .iter()
            .zip(head_batches.iter())
            .map(|(row, batch)| claim_candidate_from_row(row, batch))
            .collect::<Vec<_>>()
    };
    let deferred_row_pending = head_candidates.is_empty()
        && tx
            .query_row(
                sql.queued_batches_sqlite.exists_deferred.sql(),
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sqlite_error)?
            != 0;
    lash_core::store::claim_plan::classify_empty_claim_scan(
        &head_candidates,
        deferred_row_pending,
        boundary,
        policy,
        now,
    )
}

/// One claim-candidate scan: the rows as they were read, the batches they
/// hydrate to, and the candidates the shared prefix rule decides over.
///
/// The rows are carried alongside the candidates because the claimability
/// verdict is taken over the row's own claim columns, and `ClaimCandidate`
/// deliberately does not carry the lease generation.
pub(super) type QueuedWorkClaimScan = (
    Vec<QueuedBatchRow>,
    Vec<QueuedWorkBatch>,
    Vec<ClaimCandidate>,
);

// Exact selection passes its full validation span: validate every fencing
// token before writing, including candidates outside the selected prefix.
#[allow(clippy::too_many_arguments)]
pub(super) fn claim_queued_work_rows_sqlite(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    generation: u64,
    selected_rows: &[QueuedBatchRow],
    selected_batches: Vec<QueuedWorkBatch>,
    candidates: &[ClaimCandidate],
) -> Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> {
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
            return Ok(TxOutcome::Commit(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(TxOutcome::Rollback(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
    };
    for write in plan.writes() {
        let claimed = tx
            .execute(
                crate::turn_ingress::turn_ingress_sql()
                    .queued_batches
                    .claim
                    .sql(),
                params![
                    plan.session_id().as_str(),
                    write.batch_id.as_str(),
                    plan.claim_id(),
                    plan.lease_token(),
                    sql_session_lease_generation(plan.session_lease_generation())?,
                    sql_counter_value(
                        "queued_work_claim_fencing_token",
                        write.next_claim_fencing_token,
                    )?,
                ],
            )
            .map_err(sqlite_error)?;
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the claim transaction rolls back and no
        // claim is reported.
        if !lash_core::store_backend_support::fenced_write_applied(
            lash_core::store_backend_support::FencedWrite::QueuedWorkClaimAcquisition,
            crate::SQLITE_BACKEND,
            write.batch_id.as_str(),
            u64::try_from(claimed).unwrap_or(u64::MAX),
        ) {
            return Ok(TxOutcome::Rollback(None));
        }
    }
    Ok(TxOutcome::Commit(Some(plan.into_claim()?)))
}

pub(super) fn scan_queued_work_candidates_sqlite(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    max_rows: usize,
) -> Result<QueuedWorkClaimScan, StoreError> {
    let candidate_rows = {
        let mut stmt = tx
            .prepare(sqlite_queued_work_claim_candidates_sql(boundary))
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    now as i64,
                    sql_session_lease_generation(generation)?,
                    claim_scan_limit(max_rows)
                ],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    // The scan's SQL predicate and this filter are the same question, and the
    // shared verdict is the one answer to it: a row already claimed by the
    // claiming generation is not a candidate (ADR 0029).
    let candidate_rows = candidate_rows
        .into_iter()
        .filter(|row| {
            lash_core::store_backend_support::queued_work_batch_claimability(
                row.claim_facts(),
                generation,
            )
            .is_claimable()
        })
        .collect::<Vec<_>>();
    let candidate_batches = queued_work_batches_from_conn(tx, &candidate_rows)?;
    let candidates = candidate_rows
        .iter()
        .zip(candidate_batches.iter())
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Vec<_>>();
    Ok((candidate_rows, candidate_batches, candidates))
}

pub(super) fn claim_ready_queued_work_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    policy: QueuedWorkClaimPolicy,
) -> Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if policy.max_rows == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let (candidate_rows, candidate_batches, candidates) = scan_queued_work_candidates_sqlite(
        tx,
        now,
        session_id,
        generation,
        boundary,
        policy.max_rows,
    )?;
    let selected_len = match select_turn_work_claim_prefix(&candidates, boundary, &policy, now)? {
        TurnWorkClaimPrefix::Selected { len } => len,
        TurnWorkClaimPrefix::Refused { .. } => {
            return Ok(TxOutcome::Commit(None));
        }
    };
    let mut selected_batches = candidate_batches;
    selected_batches.truncate(selected_len);
    claim_queued_work_rows_sqlite(
        tx,
        now,
        session_id,
        owner,
        generation,
        &candidate_rows[..selected_len],
        selected_batches,
        &candidates[..selected_len],
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn claim_pending_turn_inputs_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<TxOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let candidate_rows = {
        // One named statement per filter shape production takes, picked by an
        // exhaustive match: a next-turn scan, and an active-turn scan per
        // checkpoint. The mode used to be spliced into one statement with a
        // bound `? AND state = …` disjunct and an interpolated boundary
        // predicate, neither of which a planner can seek.
        let sql = crate::turn_ingress::turn_ingress_sql();
        let statements = &sql.pending_inputs_sqlite;
        let mut values: Vec<rusqlite::types::Value> = vec![
            session_id.to_string().into(),
            sql_session_lease_generation(generation)?.into(),
            i64::try_from(max_inputs).unwrap_or(i64::MAX).into(),
        ];
        let statement = match &mode {
            lash_core::TurnInputClaimMode::NextTurn => statements.claim_candidates_next_turn.sql(),
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id,
                checkpoint,
            } => {
                values.push(turn_id.to_string().into());
                match checkpoint {
                    lash_core::CheckpointKind::AfterWork => {
                        statements.claim_candidates_active_turn_after_work.sql()
                    }
                    lash_core::CheckpointKind::BeforeCompletion => statements
                        .claim_candidates_active_turn_before_completion
                        .sql(),
                }
            }
        };
        let mut stmt = tx.prepare(statement).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(values.iter()),
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let selected = candidate_rows
        .into_iter()
        .take(max_inputs)
        .map(|row| Ok((row.clone(), pending_turn_input_from_row(row)?)))
        .collect::<Result<Vec<_>, StoreError>>()?;
    claim_turn_input_rows_sqlite_conn(
        tx,
        now,
        session_id,
        session_execution_lease,
        owner,
        mode,
        selected,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn claim_turn_input_rows_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    mode: lash_core::TurnInputClaimMode,
    selected: Vec<(PendingTurnInputRow, lash_core::PendingTurnInput)>,
) -> Result<TxOutcome<Option<lash_core::TurnInputClaim>>, StoreError> {
    let generation = session_execution_lease.fencing_token;
    let observations = selected
        .into_iter()
        .map(
            |(row, input)| lash_core::store::claim_plan::TurnInputClaimRow {
                input,
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                claim_token: row.claim_token.clone(),
                claim_session_lease_generation: row.claim_session_lease_generation,
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
            return Ok(TxOutcome::Commit(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(TxOutcome::Rollback(None));
        }
        lash_core::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
    };
    for write in plan.writes() {
        let claimed = tx
            .execute(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .claim
                    .sql(),
                params![
                    plan.session_id().as_str(),
                    write.input_id.as_str(),
                    write.state_after_claim.as_str(),
                    plan.claim_id(),
                    owner.owner_id.as_str(),
                    owner.incarnation_id.as_str(),
                    plan.lease_token(),
                    sql_session_lease_generation(plan.session_lease_generation())?,
                    sql_counter_value(
                        "turn_input_claim_fencing_token",
                        write.next_claim_fencing_token,
                    )?,
                ],
            )
            .map_err(sqlite_error)?;
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the whole claim transaction rolls back and
        // no claim is reported.
        if !lash_core::store_backend_support::fenced_write_applied(
            lash_core::store_backend_support::FencedWrite::TurnInputClaimAcquisition,
            crate::SQLITE_BACKEND,
            write.input_id.as_str(),
            u64::try_from(claimed).unwrap_or(u64::MAX),
        ) {
            return Ok(TxOutcome::Rollback(None));
        }
    }
    Ok(TxOutcome::Commit(Some(plan.into_claim())))
}

pub(super) async fn claim_pending_turn_inputs_sqlite(
    conn: &SqliteConnection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core::TurnInputClaimMode,
) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let session_id = SessionId::from(session_id.to_string());
    let session_execution_lease = session_execution_lease.clone();
    let owner = owner.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<TxOutcome<Option<lash_core::TurnInputClaim>>, StoreError> = (|| {
            ensure_session_execution_lease_conn(tx, &session_id, &session_execution_lease, now)?;
            let outcome = claim_pending_turn_inputs_sqlite_conn(
                tx,
                now,
                &session_id,
                &session_execution_lease,
                &owner,
                max_inputs,
                mode.clone(),
            )?;
            if let TxOutcome::Commit(input) = &outcome
                && let lash_core::TurnInputClaimMode::ActiveTurn { turn_id, .. } = &mode
            {
                super::queued_run_assignment::assign_checkpoint_members_conn(
                    tx,
                    &session_id,
                    turn_id,
                    input.as_ref(),
                    None,
                )?;
            }
            Ok(outcome)
        })(
        );
        match outcome {
            Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
            Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
            Err(err) => Ok(TxOutcome::Rollback(Err(err))),
        }
    })
    .await
    .map_err(sqlite_error)?
}

pub(super) fn load_session_execution_lease_row_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionExecutionLeaseRow>, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .leases
                .select_by_session
                .sql(),
            params![session_id.as_str()],
            |row| {
                let owner_id: Option<String> = row.get(0)?;
                let incarnation_id: Option<String> = row.get(5)?;
                Ok(SessionExecutionLeaseRow {
                    owner: lease_owner_from_columns(owner_id, incarnation_id).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    executor_id: row.get(6)?,
                    lease_token: row.get(1)?,
                    fencing_token: u64_from_sql(
                        "SessionExecutionLease",
                        "fencing_token",
                        row.get(2)?,
                    )?,
                    claimed_at_ms: u64_from_sql(
                        "SessionExecutionLease",
                        "claimed_at_ms",
                        row.get(3)?,
                    )?,
                    lease_term_ms: u64_from_sql(
                        "SessionExecutionLease",
                        "lease_term_ms",
                        row.get(7)?,
                    )?,
                    expires_at_ms: u64_from_sql(
                        "SessionExecutionLease",
                        "expires_at_ms",
                        row.get(4)?,
                    )?,
                })
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    Ok(row)
}

pub(super) fn acquire_session_execution_lease_conn(
    conn: &Connection,
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
    conn.execute(
        crate::turn_ingress::turn_ingress_sql().leases.acquire.sql(),
        params![
            session_id.as_str(),
            owner.owner_id.as_str(),
            owner.incarnation_id.as_str(),
            executor_id,
            lease_token,
            sql_fencing_token,
            now as i64,
            sql_expires_at,
            sql_lease_term
        ],
    )
    .map_err(sqlite_error)?;
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

pub(super) fn ensure_session_execution_lease_conn(
    conn: &Connection,
    session_id: &SessionId,
    fence: &SessionExecutionLeaseAuthority,
    now: u64,
) -> Result<(), StoreError> {
    let current = load_session_execution_lease_row_conn(conn, session_id)?;
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

/// An owned [`lash_core::OrphanedTurnInputScope`], because the write flow moves
/// its work into a `'static` closure.
pub(super) enum OwnedOrphanedScope {
    Turn(TurnId),
    LaneGeneration { resumable_turn_id: Option<TurnId> },
}

impl From<lash_core::OrphanedTurnInputScope<'_>> for OwnedOrphanedScope {
    fn from(scope: lash_core::OrphanedTurnInputScope<'_>) -> Self {
        match scope {
            lash_core::OrphanedTurnInputScope::Turn(turn_id) => Self::Turn(turn_id.clone()),
            lash_core::OrphanedTurnInputScope::LaneGeneration { resumable_turn_id } => {
                Self::LaneGeneration {
                    resumable_turn_id: resumable_turn_id.cloned(),
                }
            }
        }
    }
}

impl OwnedOrphanedScope {
    pub(super) fn borrow(&self) -> lash_core::OrphanedTurnInputScope<'_> {
        match self {
            Self::Turn(turn_id) => lash_core::OrphanedTurnInputScope::Turn(turn_id),
            Self::LaneGeneration { resumable_turn_id } => {
                lash_core::OrphanedTurnInputScope::LaneGeneration {
                    resumable_turn_id: resumable_turn_id.as_ref(),
                }
            }
        }
    }
}

/// Re-defer the active-turn-scoped rows `scope` proves are orphaned (FIG-1573).
///
/// The write is the commit-time re-defer's write: `deferred_next_turn` with
/// `NextTurn` ingress and cleared claim columns. Row selection is delegated to
/// [`lash_core::store_backend_support::orphaned_active_turn_input_is_repairable`]
/// rather than expressed as a `json_extract` predicate, so this backend and the
/// in-memory one cannot drift on which rows a repair may touch. `live_generation`
/// is the caller's fencing token, already re-validated on this connection.
pub(super) fn decode_stored_json<T: serde::de::DeserializeOwned>(
    json: &str,
    label: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {label}: {err}")))
}

pub(super) fn load_turn_cancel_request_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
    let json = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_sqlite
                .select_record
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.map(|json| decode_stored_json(&json, "turn cancel request"))
        .transpose()
}

pub(super) fn load_turn_cancel_intent_snapshot_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<lash_core::TurnCancelIntentSnapshot, StoreError> {
    let row = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .cancel_requests_sqlite
                .select_record_with_revision
                .sql(),
            params![session_id.as_str(), turn_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((json, revision)) = row else {
        return Ok(lash_core::TurnCancelIntentSnapshot::Absent);
    };
    let record: lash_core::TurnCancelRequestRecord =
        decode_stored_json(&json, "turn cancel request")?;
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Backend("turn cancel intent revision is negative".to_string()))?;
    if revision == 0 {
        return Err(StoreError::Backend(
            "turn cancel intent revision is zero".to_string(),
        ));
    }
    Ok(lash_core::TurnCancelIntentSnapshot::Present {
        request: record.request,
        revision,
    })
}

pub(super) fn append_turn_cancel_outcome_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    let Some(mut record) = load_turn_cancel_request_conn(conn, session_id, turn_id)? else {
        return Ok(());
    };
    record
        .outcome
        .get_or_insert_default()
        .affected_inputs
        .push(affected);
    conn.execute(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .update_record
            .sql(),
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub(super) fn reconcile_turn_cancel_winner_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    observed: &lash_core::TurnCancelIntentSnapshot,
    evidence: &lash_core::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)?;
    if actual != *observed {
        return Ok(false);
    }
    let mut record = load_turn_cancel_request_conn(conn, session_id, turn_id)?.unwrap_or(
        lash_core::TurnCancelRequestRecord {
            request: lash_core::facade_support::TurnCancelRequest {
                address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
                request_id: evidence.request_id.clone(),
                origin: evidence.origin.clone(),
                reason: evidence.reason.clone(),
                undelivered: evidence.undelivered,
                mode: evidence.mode,
            },
            outcome: None,
        },
    );
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
    record.request = request;
    let revision = i64::try_from(revision).map_err(|_| {
        StoreError::Backend("turn cancel intent revision exceeds SQLite range".to_string())
    })?;
    conn.execute(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .upsert_record
            .sql(),
        params![
            session_id.as_str(),
            turn_id.as_str(),
            encode_json(&record)?,
            revision
        ],
    )
    .map_err(sqlite_error)?;
    Ok(true)
}

pub(super) fn orphaned_active_turn_ids_conn(
    conn: &Connection,
    session_id: &SessionId,
    live_generation: u64,
    scope: lash_core::OrphanedTurnInputScope<'_>,
) -> Result<Vec<TurnId>, StoreError> {
    let candidates = {
        let mut stmt = conn
            .prepare(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_sqlite
                    .select_active_turn_claims
                    .sql(),
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let mut turn_ids = std::collections::BTreeSet::new();
    for (state, ingress_json, claim_token, claim_generation) in candidates {
        let ingress = decode_turn_input_ingress(ingress_json)?;
        let state = decode_turn_input_state(state, ingress)?;
        let claim_generation = u64_from_sql(
            "pending_turn_input",
            "claim_session_lease_generation",
            claim_generation,
        )
        .map_err(sqlite_error)?;
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

pub(super) fn repair_orphaned_active_turn_inputs_conn(
    conn: &Connection,
    session_id: &SessionId,
    live_generation: u64,
    turn_id: &TurnId,
    observed: &lash_core::TurnCancelIntentSnapshot,
    settlement: Option<&lash_core::TurnCancelClosureSettlement>,
) -> Result<lash_core::TurnCancelRepairResult, StoreError> {
    if load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)? != *observed {
        return Ok(lash_core::TurnCancelRepairResult::IntentChanged);
    }
    if let Some(evidence) =
        settlement.and_then(lash_core::TurnCancelClosureSettlement::base_cancellation)
        && !reconcile_turn_cancel_winner_conn(conn, session_id, turn_id, observed, evidence)?
    {
        return Ok(lash_core::TurnCancelRepairResult::IntentChanged);
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    let candidates = {
        let mut stmt = conn
            .prepare(sql.pending_inputs_sqlite.select_active_turn_rows.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str()],
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let scope = lash_core::OrphanedTurnInputScope::Turn(turn_id);
    let effective =
        settlement.and_then(lash_core::TurnCancelClosureSettlement::effective_cancellation);
    let disposition = effective.map_or(lash_core::TurnCancelDisposition::Defer, |e| e.undelivered);
    let mut repairable = Vec::new();
    for row in candidates {
        let ingress = decode_turn_input_ingress(row.ingress_json)?;
        let state = decode_turn_input_state(row.state, ingress)?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            &state,
            row.claim_token.is_some(),
            row.claim_session_lease_generation,
        ) {
            repairable.push((
                row.input_id,
                decode_stored_json(&row.input_json, "turn input")?,
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
        // `COALESCE(?N, ingress_json)` assignment carried both before.
        match disposition {
            lash_core::TurnCancelDisposition::Defer => conn.execute(
                sql.pending_inputs.defer_to_next_turn.sql(),
                params![
                    session_id.as_str(),
                    input_id.as_str(),
                    deferred.as_str(),
                    deferred_ingress.as_str(),
                ],
            ),
            lash_core::TurnCancelDisposition::Drop => conn.execute(
                sql.pending_inputs.cancel.sql(),
                params![
                    session_id.as_str(),
                    input_id.as_str(),
                    lash_core::TurnInputStateKind::Cancelled.as_str(),
                ],
            ),
        }
        .map_err(sqlite_error)?;
        let affected = lash_core::TurnCancelAffectedInput {
            input_id: input_id.into(),
            payload,
            disposition,
        };
        if effective.is_some() {
            append_turn_cancel_outcome_conn(conn, session_id, turn_id, affected.clone())?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(lash_core::TurnCancelRepairResult::Applied(outcome))
}

pub(super) fn release_session_execution_lease_conn(
    conn: &Connection,
    completion: &SessionExecutionLeaseAuthority,
) -> Result<bool, StoreError> {
    let released = conn
        .execute(
            crate::turn_ingress::turn_ingress_sql().leases.release.sql(),
            params![
                completion.session_id.as_str(),
                completion.owner.owner_id.as_str(),
                completion.owner.incarnation_id.as_str(),
                completion.executor_id.as_str(),
                completion.lease_token
            ],
        )
        .map_err(sqlite_error)?;
    Ok(released == 1)
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

pub(super) fn append_identity_columns(
    identity: &lash_core::AppendRequestIdentity,
) -> (Option<&str>, Option<i64>, Option<i64>) {
    match identity {
        lash_core::AppendRequestIdentity::PlainCommit => (None, None, None),
        lash_core::AppendRequestIdentity::Append {
            encoding_version,
            request_hash,
            requested_node_count,
            ..
        } => (
            Some(request_hash.as_str()),
            Some(*requested_node_count as i64),
            Some(i64::from(*encoding_version)),
        ),
        // A semantic-boundary identity persists without a node count; the
        // NULL count is what distinguishes its family on decode (FIG-2480).
        lash_core::AppendRequestIdentity::SemanticBoundary {
            operation: _,
            encoding_version,
            request_hash,
        } => (
            Some(request_hash.as_str()),
            None,
            Some(i64::from(*encoding_version)),
        ),
    }
}
