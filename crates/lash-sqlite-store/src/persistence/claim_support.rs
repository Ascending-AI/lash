use super::*;

/// Admit the root's turn-lane run and record it with its rows, bindings and
/// base, in one transaction ([`RootStore::admit_root`]).
///
/// [`RootStore::admit_root`]: lash_core_execution::store::RootStore::admit_root
pub(crate) async fn admit_root_sqlite(
    store: &crate::Store,
    request: &lash_core_execution::store::AdmitRootRequest,
) -> Result<Option<lash_core_execution::store::RootAdmission>, StoreError> {
    use lash_core_execution::store::{AdmittedHead, RootAdmission};
    let request = request.clone();
    let now = store.clock.timestamp_ms();
    let fleet = store.fleet_format;
    store
        .conn
        .write_flow(move |tx| {
            let outcome: Result<TxOutcome<Option<RootAdmission>>, StoreError> = (|| {
                ensure_session_execution_lease_conn(tx, &request.session_id, &request.lease, now)?;
                let roots = crate::session_roots::session_roots_sql();
                let existing: Option<Option<String>> = tx
                    .query_row(
                        roots.roots.select_admission.sql(),
                        params![request.session_id.as_str(), request.root.as_str()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sqlite_error)?;
                if let Some(Some(json)) = existing {
                    return Ok(TxOutcome::Commit(Some(
                        crate::session_roots::decode_root_admission(&json)?,
                    )));
                }
                if let Some(unfinished) =
                    crate::session_roots::unfinished_root_conn(tx, &request.session_id)?
                {
                    return Err(StoreError::UnfinishedRootConflict {
                        session_id: request.session_id.clone(),
                        root: unfinished.root,
                    });
                }
                let (inputs, queued) = match &request.head {
                    AdmittedHead::Input(head) => {
                        let claim = match claim_pending_turn_inputs_sqlite_conn(
                            tx,
                            now,
                            &request.session_id,
                            &request.lease,
                            &request.owner,
                            request.max_inputs,
                            lash_core_execution::TurnInputClaimMode::NextTurn,
                            CommandLaneGate::AdmittedRoot,
                        )? {
                            TxOutcome::Commit(Some(claim)) => claim,
                            TxOutcome::Commit(None) => return Ok(TxOutcome::Commit(None)),
                            TxOutcome::Rollback(_) => return Ok(TxOutcome::Rollback(None)),
                        };
                        if !claim.inputs.iter().any(|input| input.input_id == *head) {
                            return Ok(TxOutcome::Rollback(None));
                        }
                        (Some(Box::new(claim)), None)
                    }
                    AdmittedHead::Batch(head) => {
                        let claim = match claim_ready_queued_work_sqlite_conn(
                            tx,
                            now,
                            &request.session_id,
                            &request.lease,
                            &request.owner,
                            QueuedWorkClaimBoundary::Idle,
                            request.policy.clone(),
                        )? {
                            TxOutcome::Commit(Some(claim)) => claim,
                            TxOutcome::Commit(None) => return Ok(TxOutcome::Commit(None)),
                            TxOutcome::Rollback(_) => return Ok(TxOutcome::Rollback(None)),
                        };
                        if !claim.batches.iter().any(|batch| batch.batch_id == *head) {
                            return Ok(TxOutcome::Rollback(None));
                        }
                        (None, Some(Box::new(claim)))
                    }
                };
                let mut base = request.base.clone();
                base.generation = read_session_state_version_conn(tx, &request.session_id, fleet)?;
                crate::session_meta::retain_admission_base_conn(
                    tx,
                    &request.session_id,
                    base.checkpoint.as_ref(),
                )?;
                let admission = RootAdmission {
                    head: request.head.clone(),
                    inputs,
                    queued,
                    base,
                    turn_index: request.turn_index,
                    generation: request.generation.clone(),
                };
                crate::session_roots::bind_root_inputs_conn(
                    tx,
                    &request.session_id,
                    &request.root,
                    &admission.input_ids(),
                )?;
                let json = encode_json(&admission)?;
                let changed = tx
                    .execute(
                        roots.roots.write_admission.sql(),
                        params![
                            request.session_id.as_str(),
                            request.root.as_str(),
                            json,
                            request.admitted_generation.as_str()
                        ],
                    )
                    .map_err(sqlite_error)?;
                if changed != 1 {
                    return Err(StoreError::Backend(
                        "root admission was already recorded".into(),
                    ));
                }
                Ok(TxOutcome::Commit(Some(admission)))
            })();
            Ok(match outcome {
                Ok(TxOutcome::Commit(value)) => TxOutcome::Commit(Ok(value)),
                Ok(TxOutcome::Rollback(value)) => TxOutcome::Rollback(Ok(value)),
                Err(error) => TxOutcome::Rollback(Err(error)),
            })
        })
        .await
        .map_err(sqlite_error)?
}

/// Cancel one row.
pub(super) fn cancel_pending_turn_input_row_conn(
    conn: &Connection,
    row: PendingTurnInputRow,
    _now_epoch_ms: u64,
) -> Result<lash_core_execution::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state.kind() {
        lash_core_execution::runtime::TurnInputStateKind::Cancelled => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCancelled(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::Completed => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCompleted(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::PendingActive
        | lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn
        | lash_core_execution::runtime::TurnInputStateKind::Accepted => {
            // A claim is live only while its admitting drive epoch remains current.
            let live_claim = row.claim_token.is_some()
                && super::drive_epoch::drive_epoch_conn(conn, &row.session_id)?.epoch
                    == row.claim_session_lease_generation;
            if live_claim {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        claim: pending_turn_input_claim_diagnostics_from_row(
                            &row,
                            input.state.clone(),
                        ),
                        input,
                    },
                );
            }
            // A claimed row of the session's unfinished root is its own to
            // settle or release, whichever drive epoch claimed it.
            let root_holds_input = row.claim_token.is_some()
                && crate::session_roots::unfinished_root_conn(conn, &row.session_id)?.is_some();
            if root_holds_input {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        claim: pending_turn_input_claim_diagnostics_from_row(
                            &row,
                            input.state.clone(),
                        ),
                        input,
                    },
                );
            }
            crate::conn::cached_execute(
                conn,
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .cancel
                    .sql(),
                params![
                    row.session_id.as_str(),
                    row.input_id.as_str(),
                    lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
            input.state = lash_core_execution::TurnInputState::Cancelled(input.state.ingress());
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn checkpoint_work_pending_sqlite(
    conn: &SqliteConnection,
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
    let session_id = SessionId::from(session_id.to_string());
    let turn_id = TurnId::from(turn_id.to_string());
    conn.call(move |conn| {
        let outcome: Result<bool, StoreError> = (|| {
            let family = &crate::turn_ingress::turn_ingress_sql().family_sqlite;
            // One statement per checkpoint, chosen exhaustively: the admitted
            // minimum-boundary set is what the checkpoint decides, and an
            // optional predicate over a bound boundary cannot seek an index.
            let sql = match checkpoint {
                lash_core_execution::CheckpointKind::AfterWork => {
                    family.checkpoint_work_pending_after_work.sql()
                }
                lash_core_execution::CheckpointKind::BeforeCompletion => {
                    family.checkpoint_work_pending_before_completion.sql()
                }
            };
            let pending: i64 = conn
                .query_row(
                    sql,
                    params![
                        session_id.as_str(),
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

/// Name the refusal behind an empty candidate scan.
///
/// The candidate query enforces the delivery-boundary rule in SQL, so a scan
/// that comes back empty tells the shared claim state machine nothing. Asking
/// it again with the unfiltered head keeps the classification in one place:
/// whatever the head alone is refused for is what this claim is refused for.
/// The probe runs only on a refusal, so a successful claim pays nothing for
/// it.
pub(super) fn sqlite_refusal_for_empty_scan(
    tx: &Connection,
    session_id: &SessionId,
    now: u64,
    generation: u64,
    owner: &LeaseOwnerIdentity,
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
) -> Result<TurnWorkEmptyScanDiagnostic, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let head_rows = {
        let mut stmt = tx
            .prepare(sql.queued_batches.select_head_candidate.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    sql_session_lease_generation(generation)?,
                    owner.incarnation_id.as_str(),
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
    lash_core_execution::store::claim_plan::classify_empty_claim_scan(
        &head_candidates,
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
            return Ok(TxOutcome::Commit(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(TxOutcome::Rollback(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
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
                    i64::try_from(now).unwrap_or(i64::MAX),
                    plan.owner().incarnation_id.as_str(),
                ],
            )
            .map_err(sqlite_error)?;
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the claim transaction rolls back and no
        // claim is reported.
        if !lash_core_execution::store_backend_support::fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::QueuedWorkClaimAcquisition,
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
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
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
                    sql_session_lease_generation(generation)?,
                    claim_scan_limit(max_rows),
                    owner.incarnation_id.as_str()
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
            lash_core_execution::store_backend_support::queued_work_batch_claimability(
                row.claim_facts(),
                generation,
                &owner.incarnation_id,
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

/// The `enqueue_seq` of session `session_id`'s earliest next-turn input that
/// `generation` has not claimed: the turn-lane head of the input table.
pub(super) fn earliest_next_turn_candidate_seq_conn(
    tx: &Connection,
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
) -> Result<Option<u64>, StoreError> {
    earliest_candidate_seq_conn(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .earliest_next_turn_candidate_seq
            .sql(),
        session_id,
        generation,
        owner,
    )
}

fn earliest_candidate_seq_conn(
    tx: &Connection,
    sql: &str,
    session_id: &SessionId,
    generation: u64,
    owner: &LeaseOwnerIdentity,
) -> Result<Option<u64>, StoreError> {
    let seq: Option<i64> = tx
        .query_row(
            sql,
            params![
                session_id.as_str(),
                sql_session_lease_generation(generation)?,
                owner.incarnation_id.as_str(),
            ],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    seq.map(|seq| u64_from_sql("turn_lane", "enqueue_seq", seq).map_err(sqlite_error))
        .transpose()
}

pub(super) fn claim_ready_queued_work_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &ClaimAuthority,
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
        session_id,
        generation,
        owner,
        boundary,
        policy.max_rows,
    )?;
    // ADR 0101 §5: queued work accepted after an unclaimed next-turn input
    // waits behind it, at idle and at a checkpoint alike.
    let admitted = TurnLaneStop::before(earliest_next_turn_candidate_seq_conn(
        tx, session_id, generation, owner,
    )?)
    .queued_prefix(&candidates);
    let candidates = &candidates[..admitted];
    let selected_len = match select_turn_work_claim_prefix(candidates, boundary, &policy, now)? {
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
pub(super) fn claim_pending_turn_inputs_sqlite_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core_execution::TurnInputClaimMode,
    gate: CommandLaneGate,
) -> Result<TxOutcome<Option<lash_core_execution::TurnInputClaim>>, StoreError> {
    if max_inputs == 0 {
        return Ok(TxOutcome::Commit(None));
    }
    let follow_on_claim = match &mode {
        lash_core_execution::TurnInputClaimMode::ActiveTurn { turn_id, .. } => {
            lash_core_execution::store::FollowOnClaim::Checkpoint { turn_id }
        }
        lash_core_execution::TurnInputClaimMode::NextTurn => {
            lash_core_execution::store::FollowOnClaim::Idle
        }
    };
    if follow_on_blocks_claim_conn(tx, session_id, follow_on_claim)? {
        return Ok(TxOutcome::Commit(None));
    }
    let generation = session_execution_lease.fencing_token;
    let candidate_rows = {
        // One named statement per filter shape production takes, picked by an
        // exhaustive match: a next-turn scan per command-lane gate, and an
        // active-turn scan per checkpoint. The mode used to be spliced into
        // one statement with a bound `? AND state = …` disjunct and an
        // interpolated boundary predicate, neither of which a planner can
        // seek.
        let sql = crate::turn_ingress::turn_ingress_sql();
        let statements = &sql.pending_inputs_sqlite;
        let mut values: Vec<rusqlite::types::Value> = vec![
            session_id.to_string().into(),
            sql_session_lease_generation(generation)?.into(),
            i64::try_from(max_inputs).unwrap_or(i64::MAX).into(),
        ];
        let statement = match (&mode, gate) {
            (lash_core_execution::TurnInputClaimMode::NextTurn, CommandLaneGate::Boundary) => {
                statements.claim_candidates_next_turn.sql()
            }
            (lash_core_execution::TurnInputClaimMode::NextTurn, CommandLaneGate::AdmittedRoot) => {
                statements.claim_candidates_admitted_root.sql()
            }
            (
                lash_core_execution::TurnInputClaimMode::ActiveTurn {
                    turn_id,
                    checkpoint,
                },
                _,
            ) => {
                values.push(turn_id.to_string().into());
                match checkpoint {
                    lash_core_execution::CheckpointKind::AfterWork => {
                        statements.claim_candidates_active_turn_after_work.sql()
                    }
                    lash_core_execution::CheckpointKind::BeforeCompletion => statements
                        .claim_candidates_active_turn_before_completion
                        .sql(),
                }
            }
        };
        values.push(owner.incarnation_id.clone().into());
        let mut stmt = tx.prepare_cached(statement).map_err(sqlite_error)?;
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
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    mode: lash_core_execution::TurnInputClaimMode,
    selected: Vec<(PendingTurnInputRow, lash_core_execution::PendingTurnInput)>,
) -> Result<TxOutcome<Option<lash_core_execution::TurnInputClaim>>, StoreError> {
    let generation = session_execution_lease.fencing_token;
    let observations = selected
        .into_iter()
        .map(
            |(row, input)| lash_core_execution::store::claim_plan::TurnInputClaimRow {
                input,
                enqueue_seq: row.enqueue_seq,
                claim_fencing_token: row.claim_fencing_token,
                claim_token: row.claim_token.clone(),
                claim_session_lease_generation: row.claim_session_lease_generation,
                claim_owner_incarnation_id: row
                    .claim_owner
                    .as_ref()
                    .map(|owner| owner.incarnation_id.clone()),
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
            return Ok(TxOutcome::Commit(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Defer => {
            return Ok(TxOutcome::Rollback(None));
        }
        lash_core_execution::store::claim_plan::ClaimPlanDecision::Complete(plan) => plan,
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
                    i64::try_from(now).unwrap_or(i64::MAX),
                ],
            )
            .map_err(sqlite_error)?;
        // Backstop: the generation predicate stays on the write, but the
        // plan's verdict already authorized it over the locked row. A
        // disagreement is recorded as evidence and then fails closed exactly
        // as this site always did — the whole claim transaction rolls back and
        // no claim is reported.
        if !lash_core_execution::store_backend_support::fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::TurnInputClaimAcquisition,
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
    session_execution_lease: &ClaimAuthority,
    owner: &LeaseOwnerIdentity,
    max_inputs: usize,
    mode: lash_core_execution::TurnInputClaimMode,
) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let session_id = SessionId::from(session_id.to_string());
    let session_execution_lease = session_execution_lease.clone();
    let owner = owner.clone();
    conn.write_flow(move |tx| {
        let outcome: Result<TxOutcome<Option<lash_core_execution::TurnInputClaim>>, StoreError> =
            (|| {
                ensure_session_execution_lease_conn(
                    tx,
                    &session_id,
                    &session_execution_lease,
                    now,
                )?;
                let outcome = claim_pending_turn_inputs_sqlite_conn(
                    tx,
                    now,
                    &session_id,
                    &session_execution_lease,
                    &owner,
                    max_inputs,
                    mode.clone(),
                    CommandLaneGate::Boundary,
                )?;
                Ok(outcome)
            })();
        match outcome {
            Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
            Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
            Err(err) => Ok(TxOutcome::Rollback(Err(err))),
        }
    })
    .await
    .map_err(sqlite_error)?
}

/// The follow-on the head of `session_id` owes (ADR 0101 §3), read inside
/// the caller's transaction.
pub(super) fn pending_follow_on_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<lash_core_execution::store::PendingFollowOn>, StoreError> {
    let json = conn
        .query_row(
            crate::session_sql::session_sql()
                .head
                .select_pending_follow_on
                .sql(),
            params![session_id.as_str()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .flatten();
    lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
        session_id,
        json.as_deref(),
    )
}

/// Raise the owed follow-on's recovery count under the live lane `lease`
/// (ADR 0101 §3), inside the caller's write transaction. The head revision
/// does not move.
pub(super) fn raise_pending_follow_on_conn(
    conn: &Connection,
    lease: &ClaimAuthority,
    follow_on_turn_id: &lash_core_execution::TurnId,
    now: u64,
) -> Result<lash_core_execution::store::PendingFollowOn, StoreError> {
    ensure_session_execution_lease_conn(conn, &lease.session_id, lease, now)?;
    let not_pending = || StoreError::FollowOnNotPending {
        session_id: lease.session_id.clone(),
        follow_on_turn_id: follow_on_turn_id.clone(),
    };
    let pending = pending_follow_on_conn(conn, &lease.session_id)?
        .filter(|pending| pending.is_turn(follow_on_turn_id))
        .ok_or_else(not_pending)?;
    let raised = pending.raised()?;
    let updated = conn
        .execute(
            crate::session_sql::session_sql()
                .head
                .raise_pending_follow_on
                .sql(),
            params![
                lease.session_id.as_str(),
                lash_core_execution::store::pending_follow_on::encode_pending_follow_on(Some(
                    &raised
                ),)?,
                follow_on_turn_id.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    if updated != 1 {
        return Err(not_pending());
    }
    Ok(raised)
}

/// Whether the head's pending follow-on refuses `claim` (ADR 0101 §3): every
/// claim but the follow-on's own is blocked while it is set.
pub(super) fn follow_on_blocks_claim_conn(
    conn: &Connection,
    session_id: &SessionId,
    claim: lash_core_execution::store::FollowOnClaim<'_>,
) -> Result<bool, StoreError> {
    Ok(lash_core_execution::store::follow_on_blocks_claim(
        pending_follow_on_conn(conn, session_id)?.as_ref(),
        claim,
    )
    .is_some())
}

pub(super) fn ensure_session_execution_lease_conn(
    conn: &Connection,
    session_id: &SessionId,
    fence: &ClaimAuthority,
    _now: u64,
) -> Result<(), StoreError> {
    lash_core_execution::store::require_current_drive_fence(
        session_id,
        &fence.drive_fence(),
        &super::drive_epoch::drive_epoch_conn(conn, session_id)?,
    )
}

/// An owned [`lash_core_execution::OrphanedTurnInputScope`], because the write flow moves
/// its work into a `'static` closure.
pub(super) enum OwnedOrphanedScope {
    Turn(TurnId),
    LaneGeneration { resumable_turn_id: Option<TurnId> },
}

impl From<lash_core_execution::OrphanedTurnInputScope<'_>> for OwnedOrphanedScope {
    fn from(scope: lash_core_execution::OrphanedTurnInputScope<'_>) -> Self {
        match scope {
            lash_core_execution::OrphanedTurnInputScope::Turn(turn_id) => {
                Self::Turn(turn_id.clone())
            }
            lash_core_execution::OrphanedTurnInputScope::LaneGeneration { resumable_turn_id } => {
                Self::LaneGeneration {
                    resumable_turn_id: resumable_turn_id.cloned(),
                }
            }
        }
    }
}

impl OwnedOrphanedScope {
    pub(super) fn borrow(&self) -> lash_core_execution::OrphanedTurnInputScope<'_> {
        match self {
            Self::Turn(turn_id) => lash_core_execution::OrphanedTurnInputScope::Turn(turn_id),
            Self::LaneGeneration { resumable_turn_id } => {
                lash_core_execution::OrphanedTurnInputScope::LaneGeneration {
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
/// [`lash_core_execution::store_backend_support::orphaned_active_turn_input_is_repairable`]
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
) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
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
) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
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
        return Ok(lash_core_execution::TurnCancelIntentSnapshot::Absent);
    };
    let record: lash_core_execution::TurnCancelRequestRecord =
        decode_stored_json(&json, "turn cancel request")?;
    let revision = u64::try_from(revision)
        .map_err(|_| StoreError::Backend("turn cancel intent revision is negative".to_string()))?;
    if revision == 0 {
        return Err(StoreError::Backend(
            "turn cancel intent revision is zero".to_string(),
        ));
    }
    Ok(lash_core_execution::TurnCancelIntentSnapshot::Present {
        request: record.request,
        revision,
    })
}

pub(super) fn append_turn_cancel_outcome_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedInput,
) -> Result<(), StoreError> {
    let Some(mut record) = load_turn_cancel_request_conn(conn, session_id, turn_id)? else {
        return Ok(());
    };
    record
        .outcome
        .get_or_insert_default()
        .affected_inputs
        .push(affected);
    crate::conn::cached_execute(
        conn,
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests_sqlite
            .update_record
            .sql(),
        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Record one wake a turn cancel deferred on the cancellation (FIG-3543).
pub(super) fn append_turn_cancel_wake_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    affected: lash_core_execution::TurnCancelAffectedWake,
) -> Result<(), StoreError> {
    let Some(mut record) = load_turn_cancel_request_conn(conn, session_id, turn_id)? else {
        return Ok(());
    };
    record
        .outcome
        .get_or_insert_default()
        .affected_wakes
        .push(affected);
    crate::conn::cached_execute(
        conn,
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
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
) -> Result<bool, StoreError> {
    let actual = load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)?;
    if actual != *observed {
        return Ok(false);
    }
    let mut record = load_turn_cancel_request_conn(conn, session_id, turn_id)?.unwrap_or(
        lash_core_execution::TurnCancelRequestRecord {
            request: lash_core_execution::facade_support::TurnCancelRequest {
                address: lash_core_execution::facade_support::TurnAddress::new(session_id, turn_id),
                request_id: evidence.request_id.clone(),
                origin: evidence.origin.clone(),
                reason: evidence.reason.clone(),
                undelivered: evidence.undelivered,
                mode: evidence.mode,
            },
            outcome: None,
        },
    );
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
    record.request = request;
    let revision = i64::try_from(revision).map_err(|_| {
        StoreError::Backend("turn cancel intent revision exceeds SQLite range".to_string())
    })?;
    crate::conn::cached_execute(
        conn,
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
    scope: lash_core_execution::OrphanedTurnInputScope<'_>,
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

pub(super) fn repair_orphaned_active_turn_inputs_conn(
    conn: &Connection,
    session_id: &SessionId,
    live_generation: u64,
    turn_id: &TurnId,
    observed: &lash_core_execution::TurnCancelIntentSnapshot,
    settlement: Option<&lash_core_execution::TurnCancelClosureSettlement>,
) -> Result<lash_core_execution::TurnCancelRepairResult, StoreError> {
    if load_turn_cancel_intent_snapshot_conn(conn, session_id, turn_id)? != *observed {
        return Ok(lash_core_execution::TurnCancelRepairResult::IntentChanged);
    }
    if let Some(evidence) =
        settlement.and_then(lash_core_execution::TurnCancelClosureSettlement::base_cancellation)
        && !reconcile_turn_cancel_winner_conn(conn, session_id, turn_id, observed, evidence)?
    {
        return Ok(lash_core_execution::TurnCancelRepairResult::IntentChanged);
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
    let scope = lash_core_execution::OrphanedTurnInputScope::Turn(turn_id);
    let effective = settlement
        .and_then(lash_core_execution::TurnCancelClosureSettlement::effective_cancellation);
    let disposition = effective.map_or(lash_core_execution::TurnCancelDisposition::Defer, |e| {
        e.undelivered
    });
    let mut repairable = Vec::new();
    for row in candidates {
        let ingress = decode_turn_input_ingress(row.ingress_json)?;
        let state = decode_turn_input_state(row.state, ingress)?;
        if lash_core_execution::store_backend_support::orphaned_active_turn_input_is_repairable(
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
        // `COALESCE(?N, ingress_json)` assignment carried both before.
        match disposition {
            lash_core_execution::TurnCancelDisposition::Defer => crate::conn::cached_execute(
                conn,
                sql.pending_inputs.defer_to_next_turn.sql(),
                params![
                    session_id.as_str(),
                    input_id.as_str(),
                    deferred.as_str(),
                    deferred_ingress.as_str(),
                ],
            ),
            lash_core_execution::TurnCancelDisposition::Drop => crate::conn::cached_execute(
                conn,
                sql.pending_inputs.cancel.sql(),
                params![
                    session_id.as_str(),
                    input_id.as_str(),
                    lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                ],
            ),
        }
        .map_err(sqlite_error)?;
        let affected = lash_core_execution::TurnCancelAffectedInput {
            input_id: input_id.into(),
            payload,
            disposition,
        };
        if effective.is_some() {
            append_turn_cancel_outcome_conn(conn, session_id, turn_id, affected.clone())?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(lash_core_execution::TurnCancelRepairResult::Applied(
        outcome,
    ))
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

pub(super) fn append_identity_columns(
    identity: &lash_core_execution::AppendRequestIdentity,
) -> (Option<&str>, Option<i64>, Option<i64>) {
    match identity {
        lash_core_execution::AppendRequestIdentity::PlainCommit => (None, None, None),
        lash_core_execution::AppendRequestIdentity::Append {
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
        lash_core_execution::AppendRequestIdentity::SemanticBoundary {
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
