use super::*;

pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
) -> Result<lash_core::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state {
        lash_core::TurnInputState::Cancelled => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCancelled(input),
        ),
        lash_core::TurnInputState::Completed => Ok(
            lash_core::PendingTurnInputCancelOutcome::AlreadyCompleted(input),
        ),
        lash_core::TurnInputState::Accepted => {
            Ok(lash_core::PendingTurnInputCancelOutcome::AlreadyClaimed {
                claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state),
                input,
            })
        }
        lash_core::TurnInputState::PendingActive | lash_core::TurnInputState::DeferredNextTurn => {
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
                    claim: pending_turn_input_claim_diagnostics_from_row(&row, input.state),
                    input,
                });
            }
            conn.execute(
                "UPDATE pending_turn_inputs
                 SET state = ?3,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE session_id = ?1 AND input_id = ?2",
                params![
                    row.session_id.as_str(),
                    row.input_id.as_str(),
                    lash_core::TurnInputState::Cancelled.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
            input.state = lash_core::TurnInputState::Cancelled;
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
            let head_candidate = sqlite_queued_work_head_candidate_cte(
                QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            );
            let admitted_min_boundary = lash_core::store_backend_support::admitted_min_boundary_sql(
                "json_extract(ingress_json, '$.min_boundary')",
                checkpoint,
            );
            let accepted_state = lash_core::store_backend_support::state_sql_literal(
                lash_core::TurnInputState::Accepted,
            );
            let sql = format!(
                "WITH {head_candidate}
                 SELECT (
                    ?6 > 0 AND EXISTS (
                        SELECT 1
                        FROM pending_turn_inputs
                        WHERE session_id = ?1
                          AND state IN (?4, {accepted_state})
                          AND (claim_token IS NULL OR claim_session_lease_generation <> ?3)
                          AND json_extract(ingress_json, '$.scope') = 'active_turn'
                          AND json_extract(ingress_json, '$.turn_id') = ?5
                          AND {admitted_min_boundary}
                        LIMIT 1
                    )
                ) OR (
                    ?7 > 0 AND EXISTS (
                        SELECT 1
                        FROM queued_work_head_candidate AS head
                        JOIN queued_work_items AS item
                          ON item.batch_id = head.head_batch_id
                        WHERE json_extract(item.payload_json, '$.type') <> 'session_command'
                        LIMIT 1
                    )
                )"
            );
            let pending: i64 = conn
                .query_row(
                    &sql,
                    params![
                        session_id.as_str(),
                        now as i64,
                        sql_session_lease_generation(generation)?,
                        lash_core::TurnInputState::PendingActive.as_str(),
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
    let head_rows = {
        let mut stmt = tx
            .prepare(&format!(
                "SELECT {QUEUED_WORK_COLUMNS}
                 FROM queued_work_batches
                 WHERE {SQLITE_QUEUED_WORK_HEAD_CANDIDATE_PREDICATE}
                 ORDER BY enqueue_seq ASC
                 LIMIT 1",
                QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
            ))
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
    if !head_rows.is_empty() {
        let head_batches = queued_work_batches_from_conn(tx, &head_rows)?;
        let head_candidates = head_rows
            .iter()
            .zip(head_batches.iter())
            .map(|(row, batch)| claim_candidate_from_row(row, batch))
            .collect::<Vec<_>>();
        let head_prefix = select_turn_work_claim_prefix(&head_candidates, boundary, policy, now)?;
        return Ok(TurnWorkEmptyScanDiagnostic::from(head_prefix));
    }
    let deferred_row_pending = tx
        .query_row(
            "SELECT EXISTS (
                 SELECT 1
                 FROM queued_work_batches
                 WHERE session_id = ?1
                   AND available_at_ms > ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?3
                   )
             )",
            params![
                session_id.as_str(),
                now as i64,
                sql_session_lease_generation(generation)?
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sqlite_error)?
        != 0;
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
pub(super) fn claim_queued_work_rows_sqlite(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    generation: u64,
    selected_batches: Vec<QueuedWorkBatch>,
    candidates: &[ClaimCandidate],
) -> Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> {
    if selected_batches.is_empty() {
        return Ok(TxOutcome::Commit(None));
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
        let claimed = tx
            .execute(
                "UPDATE queued_work_batches
                 SET claim_id = ?3,
                     claim_token = ?4,
                     claim_fencing_token = ?6,
                     claim_session_lease_generation = ?5
                 WHERE session_id = ?1
                   AND batch_id = ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?5
                   )",
                params![
                    session_id.as_str(),
                    row.batch_id.as_str(),
                    lease.claim_id.as_str(),
                    lease.lease_token,
                    sql_session_lease_generation(lease.session_lease_generation)?,
                    sql_fencing_token,
                ],
            )
            .map_err(sqlite_error)?;
        if claimed == 0 {
            return Ok(TxOutcome::Rollback(None));
        }
    }
    Ok(TxOutcome::Commit(Some(QueuedWorkClaim {
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
        ),
    })))
}

pub(super) fn scan_queued_work_candidates_sqlite(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    generation: u64,
    boundary: QueuedWorkClaimBoundary,
    max_rows: usize,
) -> Result<(Vec<QueuedWorkBatch>, Vec<ClaimCandidate>), StoreError> {
    let candidate_rows = {
        let mut stmt = tx
            .prepare(&sqlite_queued_work_claim_candidates_sql(boundary))
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
    let candidate_rows = candidate_rows
        .into_iter()
        .filter(|row| row.claim_token.is_none() || row.claim_session_lease_generation != generation)
        .collect::<Vec<_>>();
    let candidate_batches = queued_work_batches_from_conn(tx, &candidate_rows)?;
    let candidates = candidate_rows
        .iter()
        .zip(candidate_batches.iter())
        .map(|(row, batch)| claim_candidate_from_row(row, batch))
        .collect::<Vec<_>>();
    Ok((candidate_batches, candidates))
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
    let (candidate_batches, candidates) = scan_queued_work_candidates_sqlite(
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
    let active_turn = matches!(mode, lash_core::TurnInputClaimMode::ActiveTurn { .. });
    let wanted_state = match &mode {
        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
            lash_core::TurnInputState::PendingActive
        }
        lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
    };
    let candidate_rows = {
        let accepted_state = lash_core::store_backend_support::state_sql_literal(
            lash_core::TurnInputState::Accepted,
        );
        let mut sql = format!(
            "SELECT {PENDING_TURN_INPUT_COLUMNS}
                 FROM pending_turn_inputs
                 WHERE session_id = ?
                   AND (state = ? OR (? AND state = {accepted_state}))
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?
                   )"
        );
        let mut values: Vec<rusqlite::types::Value> = vec![
            session_id.to_string().into(),
            wanted_state.as_str().to_string().into(),
            i64::from(active_turn).into(),
            sql_session_lease_generation(generation)?.into(),
        ];
        if let lash_core::TurnInputClaimMode::ActiveTurn {
            turn_id,
            checkpoint,
        } = &mode
        {
            sql.push_str(
                " AND json_extract(ingress_json, '$.scope') = 'active_turn'
                  AND json_extract(ingress_json, '$.turn_id') = ?",
            );
            values.push(turn_id.to_string().into());
            sql.push_str(" AND ");
            sql.push_str(
                &lash_core::store_backend_support::admitted_min_boundary_sql(
                    "json_extract(ingress_json, '$.min_boundary')",
                    *checkpoint,
                ),
            );
        }
        sql.push_str(" ORDER BY enqueue_seq ASC LIMIT ?");
        values.push(i64::try_from(max_inputs).unwrap_or(i64::MAX).into());
        let mut stmt = tx.prepare(&sql).map_err(sqlite_error)?;
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
    let Some((head, _)) = selected.first() else {
        return Ok(TxOutcome::Commit(None));
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
        let claimed = tx
            .execute(
                "UPDATE pending_turn_inputs
                 SET state = ?3,
                     claim_id = ?4,
                     claim_owner_id = ?5,
                     claim_owner_incarnation_id = ?6,
                     claim_token = ?7,
                     claim_fencing_token = ?9,
                     claim_session_lease_generation = ?8
                 WHERE session_id = ?1
                   AND input_id = ?2
                   AND (
                        claim_token IS NULL
                        OR claim_session_lease_generation <> ?8
                   )",
                params![
                    session_id.as_str(),
                    row.input_id.as_str(),
                    state_after_claim.as_str(),
                    lease.claim_id.as_str(),
                    owner.owner_id.as_str(),
                    owner.incarnation_id.as_str(),
                    lease.lease_token,
                    sql_session_lease_generation(lease.session_lease_generation)?,
                    sql_fencing_token,
                ],
            )
            .map_err(sqlite_error)?;
        if claimed == 0 {
            return Ok(TxOutcome::Rollback(None));
        }
        input.state = state_after_claim;
        inputs.push(input);
    }
    Ok(TxOutcome::Commit(Some(lash_core::TurnInputClaim {
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
    })))
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
            claim_pending_turn_inputs_sqlite_conn(
                tx,
                now,
                &session_id,
                &session_execution_lease,
                &owner,
                max_inputs,
                mode,
            )
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
            "SELECT lease_owner_id, lease_token, lease_fencing_token,
                    lease_claimed_at_ms, lease_expires_at_ms,
                    lease_owner_incarnation_id, lease_executor_id, lease_term_ms
             FROM session_execution_leases
             WHERE session_id = ?1",
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
        "INSERT INTO session_execution_leases (
            session_id, lease_owner_id, lease_owner_incarnation_id, lease_executor_id,
            lease_token, lease_fencing_token,
            lease_claimed_at_ms, lease_expires_at_ms, lease_term_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(session_id) DO UPDATE SET
            lease_owner_id = excluded.lease_owner_id,
            lease_owner_incarnation_id = excluded.lease_owner_incarnation_id,
            lease_executor_id = excluded.lease_executor_id,
            lease_token = excluded.lease_token,
            lease_fencing_token = excluded.lease_fencing_token,
            lease_claimed_at_ms = excluded.lease_claimed_at_ms,
            lease_expires_at_ms = excluded.lease_expires_at_ms,
            lease_term_ms = excluded.lease_term_ms",
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
            "SELECT record_json FROM turn_cancel_requests
             WHERE session_id = ?1 AND turn_id = ?2",
            params![session_id.as_str(), turn_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.map(|json| decode_stored_json(&json, "turn cancel request"))
        .transpose()
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
        "UPDATE turn_cancel_requests SET record_json = ?3
         WHERE session_id = ?1 AND turn_id = ?2",
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub(super) fn reconcile_turn_cancel_winner_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    evidence: &lash_core::facade_support::TurnCancellationEvidence,
) -> Result<(), StoreError> {
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
    record.request = lash_core::facade_support::TurnCancelRequest {
        address: lash_core::facade_support::TurnAddress::new(session_id, turn_id),
        request_id: evidence.request_id.clone(),
        origin: evidence.origin.clone(),
        reason: evidence.reason.clone(),
        undelivered: evidence.undelivered,
        mode: evidence.mode,
    };
    conn.execute(
        "INSERT INTO turn_cancel_requests (session_id, turn_id, record_json)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id, turn_id) DO UPDATE SET record_json = excluded.record_json",
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
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
                "SELECT state, ingress_json, claim_token, claim_session_lease_generation
                 FROM pending_turn_inputs
                 WHERE session_id = ?1 AND state IN (?2, ?3) ORDER BY enqueue_seq ASC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    lash_core::TurnInputState::PendingActive.as_str(),
                    lash_core::TurnInputState::Accepted.as_str(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let mut turn_ids = std::collections::BTreeSet::new();
    for (state, ingress_json, claim_token, claim_generation) in candidates {
        let state = decode_turn_input_state(state)?;
        let ingress = decode_turn_input_ingress(ingress_json)?;
        let claim_generation = u64_from_sql(
            "pending_turn_input",
            "claim_session_lease_generation",
            claim_generation,
        )
        .map_err(sqlite_error)?;
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

pub(super) fn repair_orphaned_active_turn_inputs_conn(
    conn: &Connection,
    session_id: &SessionId,
    live_generation: u64,
    turn_id: &TurnId,
    decision: &lash_core::TurnCancelRepairDecision,
) -> Result<lash_core::TurnCancelInputOutcome, StoreError> {
    if matches!(
        decision,
        lash_core::TurnCancelRepairDecision::NoCancellationIntent
    ) && load_turn_cancel_request_conn(conn, session_id, turn_id)?.is_some()
    {
        return Ok(Default::default());
    }
    if let lash_core::TurnCancelRepairDecision::CancellationWon(evidence) = decision {
        reconcile_turn_cancel_winner_conn(conn, session_id, turn_id, evidence)?;
    }
    let candidates = {
        let mut stmt = conn
            .prepare(
                "SELECT input_id, state, ingress_json, input_json, claim_token, claim_session_lease_generation
                 FROM pending_turn_inputs
                 WHERE session_id = ?1 AND state IN (?2, ?3) ORDER BY enqueue_seq ASC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    lash_core::TurnInputState::PendingActive.as_str(),
                    lash_core::TurnInputState::Accepted.as_str(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let scope = lash_core::OrphanedTurnInputScope::Turn(turn_id);
    let disposition = decision.disposition();
    let mut repairable = Vec::new();
    for (input_id, state, ingress_json, input_json, claim_token, claim_generation) in candidates {
        let state = decode_turn_input_state(state)?;
        let ingress = decode_turn_input_ingress(ingress_json)?;
        let claim_generation = u64_from_sql(
            "pending_turn_input",
            "claim_session_lease_generation",
            claim_generation,
        )
        .map_err(sqlite_error)?;
        if lash_core::store_backend_support::orphaned_active_turn_input_is_repairable(
            scope,
            live_generation,
            state,
            &ingress,
            claim_token.is_some(),
            claim_generation,
        ) {
            repairable.push((input_id, decode_stored_json(&input_json, "turn input")?));
        }
    }
    if repairable.is_empty() {
        return Ok(Default::default());
    }
    let next_turn_ingress = encode_json(&lash_core::TurnInputIngress::NextTurn)?;
    let mut stmt = conn
        .prepare(
            "UPDATE pending_turn_inputs
             SET state = ?3,
                 ingress_json = COALESCE(?4, ingress_json),
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE session_id = ?1 AND input_id = ?2",
        )
        .map_err(sqlite_error)?;
    let mut outcome = lash_core::TurnCancelInputOutcome::default();
    for (input_id, payload) in repairable {
        stmt.execute(params![
            session_id.as_str(),
            input_id.as_str(),
            match disposition {
                lash_core::TurnCancelDisposition::Defer =>
                    lash_core::TurnInputState::DeferredNextTurn.as_str(),
                lash_core::TurnCancelDisposition::Drop =>
                    lash_core::TurnInputState::Cancelled.as_str(),
            },
            match disposition {
                lash_core::TurnCancelDisposition::Defer => Some(next_turn_ingress.as_str()),
                lash_core::TurnCancelDisposition::Drop => None,
            }
        ])
        .map_err(sqlite_error)?;
        let affected = lash_core::TurnCancelAffectedInput {
            input_id,
            payload,
            disposition,
        };
        if matches!(
            decision,
            lash_core::TurnCancelRepairDecision::CancellationWon(_)
        ) {
            append_turn_cancel_outcome_conn(conn, session_id, turn_id, affected.clone())?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(outcome)
}

pub(super) fn release_session_execution_lease_conn(
    conn: &Connection,
    completion: &SessionExecutionLeaseAuthority,
) -> Result<bool, StoreError> {
    let released = conn
        .execute(
            "UPDATE session_execution_leases
         SET lease_owner_id = NULL,
             lease_owner_incarnation_id = NULL,
             lease_executor_id = NULL,
             lease_token = NULL,
             lease_claimed_at_ms = 0,
             lease_term_ms = 0,
             lease_expires_at_ms = 0
         WHERE session_id = ?1
           AND lease_owner_id = ?2
           AND lease_owner_incarnation_id = ?3
           AND lease_executor_id = ?4
           AND lease_token = ?5",
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
