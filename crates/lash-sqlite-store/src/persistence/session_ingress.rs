//! [`SessionIngressStore`] for [`Store`]: the one session ingress (ADR 0101).
//!
//! Every write runs in one `BEGIN IMMEDIATE` transaction, whose database write
//! lock is the session lock: admission takes its `enqueue_seq` under it, so
//! enqueue order is commit order, and every claim, release, withdrawal and
//! settlement observes the rows it decides from under the same lock. The
//! decisions themselves are the shared planners'; this module reads rows,
//! hands them over and executes the planned writes.
//!
//! Claims, reclaims and settlements are fenced by the drive: the presented
//! [`DriveFence`] must name the session's current drive epoch, read from its
//! `session_meta` row in the same transaction. [`DriveEpochStore`] is the
//! storage half of the admission seal that raises that epoch.

use super::*;
use lash_core_execution::store::session_ingress_plan::{
    IngressClaimAttempt, IngressClaimCandidate, IngressClaimPlan, IngressReclaimDecision,
    IngressRowSettlement, IngressWithdrawDecision, plan_command_claim, plan_ingress_settlement,
    plan_reclaim, plan_turn_claim, plan_withdrawal,
};
use lash_core_execution::store::{
    AdmissionId, ClaimMode, DriveEpochSeal, DriveEpochSealDecision, DriveEpochStore, DriveFence,
    IngressClaim, IngressClaimPolicy, IngressClaimSettlement, IngressEnqueueOutcome, IngressItem,
    IngressItemDraft, IngressItemRead, IngressLane, IngressReadStatus, IngressReclaimOutcome,
    IngressSettlementIntent, IngressSettlementReceipt, IngressState, IngressSuffixWithdrawOutcome,
    IngressTerminalCause, IngressUndeliveredDisposition, IngressWithdrawOutcome,
    IngressWithdrawReceipt, IngressWithdrawSelector, IngressWithdrawTarget, SessionIngressStore,
    StoredDriveEpoch, decide_drive_epoch_seal, require_current_drive_fence,
};
use lash_core_execution::store_backend_support::{
    SessionIngressAdmission, SessionIngressAdmissionFacts, SessionIngressInsert,
    SessionIngressRowColumns, SessionIngressStoredRow, decide_session_ingress_admission,
    encode_ingress_terminal_cause, sealed_drive_fence,
};
use lash_sansio::ProcessId;

fn ingress_sql() -> &'static crate::session_ingress::SessionIngressSql {
    crate::session_ingress::session_ingress_sql()
}

fn row_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionIngressRowColumns> {
    Ok(SessionIngressRowColumns {
        enqueue_seq: row.get(0)?,
        item_id: row.get(1)?,
        session_id: row.get(2)?,
        kind: row.get(3)?,
        source_key: row.get(4)?,
        delivery_scope: row.get(5)?,
        delivery_turn_id: row.get(6)?,
        delivery_min_boundary: row.get(7)?,
        submission_digest: row.get(8)?,
        payload_json: row.get(9)?,
        authority_json: row.get(10)?,
        merge_key: row.get(11)?,
        state: row.get(12)?,
        terminal_cause_json: row.get(13)?,
        enqueued_at_ms: row.get(14)?,
        terminal_at_ms: row.get(15)?,
        claim_id: row.get(16)?,
        claim_token: row.get(17)?,
        claim_admission_id: row.get(18)?,
        claim_fencing_token: row.get(19)?,
        claim_drive_epoch: row.get(20)?,
        claim_turn_id: row.get(21)?,
    })
}

fn query_rows(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<SessionIngressStoredRow>, StoreError> {
    let mut statement = conn.prepare(sql).map_err(sqlite_error)?;
    let columns = statement
        .query_map(params, row_columns)
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    columns
        .into_iter()
        .map(SessionIngressRowColumns::decode)
        .collect()
}

fn query_row(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    conn.query_row(sql, params, row_columns)
        .optional()
        .map_err(sqlite_error)?
        .map(SessionIngressRowColumns::decode)
        .transpose()
}

/// The row `item_id` names, if it belongs to `session_id`.
fn session_row_by_id(
    conn: &Connection,
    session_id: &SessionId,
    item_id: &str,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    Ok(query_row(
        conn,
        ingress_sql().shared.select_by_item_id.sql(),
        params![item_id],
    )?
    .filter(|row| row.item.session_id == *session_id))
}

/// Whether turn `turn_id` of `session_id` has its final commit recorded.
fn turn_ended_conn(
    conn: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<bool, StoreError> {
    let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
        session_id, turn_id,
    )?;
    conn.query_row(
        session_sql().turn_commits.exists_for_turn.sql(),
        params![session_id.as_str(), key],
        |row| row.get::<_, bool>(0),
    )
    .map_err(sqlite_error)
}

/// The physical turn the session's unfinished logical run is at, if any.
fn running_turn_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<TurnId>, StoreError> {
    Ok(load_run_conn(conn, session_id, None)?
        .filter(|admission| admission.terminal.is_none())
        .map(|admission| admission.position.turn_id))
}

/// The session's stored drive epoch and the admission that last raised it,
/// read inside the caller's transaction.
pub(crate) fn drive_epoch_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<StoredDriveEpoch, StoreError> {
    let row = conn
        .query_row(
            session_sql().meta.select_drive_epoch.sql(),
            params![session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?
        .ok_or_else(|| StoreError::DriveEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    Ok(StoredDriveEpoch {
        epoch: u64::try_from(row.0)
            .map_err(|_| stored_data_corrupt("SessionMeta", "drive_epoch must be non-negative"))?,
        admission: row.1.map(AdmissionId::new),
        closing: row
            .2
            .map(|intent| {
                u64::try_from(intent).map_err(|_| {
                    stored_data_corrupt("SessionMeta", "closing_intent must be non-negative")
                })
            })
            .transpose()?
            .map(lash_core_execution::store::ControlIntentId::from_sequence),
    })
}

/// Refuse `fence` unless it is the session's current drive fence, read in the
/// caller's transaction.
fn require_fence_conn(
    conn: &Connection,
    session_id: &SessionId,
    fence: &DriveFence,
) -> Result<(), StoreError> {
    let current = drive_epoch_conn(conn, session_id)?;
    require_current_drive_fence(session_id, fence, &current)
}

/// Refuse `fence` unless it is current and `claim` belongs to its session.
/// The fence is checked first, so a stale fence is refused as stale.
fn require_claim_fence_conn(
    conn: &Connection,
    fence: &DriveFence,
    claim: &IngressClaim,
) -> Result<(), StoreError> {
    require_fence_conn(conn, fence.session(), fence)?;
    if claim.session_id != *fence.session() {
        return Err(StoreError::DriveFenceSessionMismatch {
            session_id: claim.session_id.clone(),
            fence_session_id: fence.session().clone(),
        });
    }
    Ok(())
}

/// Push `row` unless a row with its item id is already in `rows`.
fn push_distinct(rows: &mut Vec<SessionIngressStoredRow>, row: SessionIngressStoredRow) {
    if !rows
        .iter()
        .any(|seen| seen.item.item_id == row.item.item_id)
    {
        rows.push(row);
    }
}

fn wake_floor_conn(
    conn: &Connection,
    session_id: &SessionId,
    process_id: &ProcessId,
) -> Result<Option<u64>, StoreError> {
    conn.query_row(
        crate::process_registry::sql::process_sql()
            .fence
            .select_floor
            .sql(),
        params![session_id.as_str(), process_id.as_str()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(sqlite_error)?
    .map(|floor| {
        u64::try_from(floor).map_err(|_| {
            stored_data_corrupt(
                "WakeRedeliveryFence",
                format!("allocation_floor must be non-negative, got {floor}"),
            )
        })
    })
    .transpose()
}

/// Raise the session's redelivery floor for `process_id` to at least
/// `sequence`, in the caller's transaction, and return the floor after.
fn raise_wake_floor_conn(
    conn: &Connection,
    session_id: &SessionId,
    process_id: &ProcessId,
    sequence: u64,
) -> Result<u64, StoreError> {
    conn.execute(
        crate::process_registry::sql::process_sql()
            .fence_sqlite
            .upsert_max
            .sql(),
        params![
            session_id.as_str(),
            process_id.as_str(),
            sql_counter_value("wake_redelivery_floor", sequence)?
        ],
    )
    .map_err(sqlite_error)?;
    wake_floor_conn(conn, session_id, process_id)?.ok_or_else(|| {
        stored_data_corrupt("WakeRedeliveryFence", "a raised floor must be readable")
    })
}

/// The lane's non-terminal rows as a claim attempt sees them.
fn lane_candidates_conn(
    conn: &Connection,
    session_id: &SessionId,
    lane: IngressLane,
) -> Result<Vec<IngressClaimCandidate>, StoreError> {
    let rows = query_rows(
        conn,
        ingress_sql().shared.select_open_in_lane.sql(),
        params![session_id.as_str(), lane.as_str()],
    )?;
    rows.into_iter()
        .map(|row| {
            let addressed_turn_ended = match row.item.delivery.addressed_turn() {
                Some(turn_id) => turn_ended_conn(conn, session_id, turn_id)?,
                None => false,
            };
            let claim_turn_ended = match row
                .claim
                .as_ref()
                .and_then(|claim| claim.claim_turn_id.as_ref())
            {
                Some(turn_id) => turn_ended_conn(conn, session_id, turn_id)?,
                None => false,
            };
            Ok(row.into_candidate(addressed_turn_ended, claim_turn_ended))
        })
        .collect()
}

/// Install `plan`'s claim on each row it takes. A row another writer moved
/// since the scan fails the whole attempt.
fn install_claim_conn(
    conn: &Connection,
    session_id: &SessionId,
    plan: &IngressClaimPlan,
) -> Result<(), StoreError> {
    let stamp = plan.stamp();
    let drive_epoch = sql_counter_value("session_ingress_claim_drive_epoch", stamp.drive_epoch)?;
    for write in plan.writes() {
        let changed = conn
            .execute(
                ingress_sql().shared.claim_row.sql(),
                params![
                    session_id.as_str(),
                    write.item_id.as_str(),
                    stamp.claim_id,
                    stamp.claim_token,
                    stamp.admission.as_str(),
                    sql_counter_value(
                        "session_ingress_claim_fencing_token",
                        write.next_claim_fencing_token
                    )?,
                    drive_epoch,
                    plan.claim_turn_id().map(TurnId::as_str),
                    sql_counter_value(
                        "session_ingress_claim_fencing_token",
                        write.observed_claim_fencing_token
                    )?,
                ],
            )
            .map_err(sqlite_error)?;
        if changed != 1 {
            return Err(StoreError::Backend(format!(
                "session ingress item `{}` moved inside its claim transaction",
                write.item_id
            )));
        }
    }
    Ok(())
}

fn tombstone_state(state: IngressState) -> &'static str {
    state.as_str()
}

/// Execute one settlement inside the caller's transaction (ADR 0101 §7, §9,
/// §10, §12). The commit path runs this beside the head write; every wake
/// terminal raises its floor here, in the same transaction.
pub(crate) fn apply_session_ingress_settlement_conn(
    conn: &Connection,
    fence: &DriveFence,
    settlement: &IngressClaimSettlement,
    now: u64,
) -> Result<IngressSettlementReceipt, StoreError> {
    let session_id = &settlement.session_id;
    require_fence_conn(conn, session_id, fence)?;
    // Every row a named claim still holds, and every named row: the planner
    // refuses a settlement that leaves part of a claim unnamed.
    let mut observed = Vec::new();
    for claim in &settlement.claims {
        for row in query_rows(
            conn,
            ingress_sql().shared.select_claimed.sql(),
            params![session_id.as_str(), claim.identity.claim_id],
        )? {
            push_distinct(&mut observed, row);
        }
        for item_id in &claim.item_ids {
            if let Some(row) = session_row_by_id(conn, session_id, item_id.as_str())? {
                push_distinct(&mut observed, row);
            }
        }
    }
    let covered = match &settlement.intent {
        IngressSettlementIntent::Turn {
            cancel: Some(cancel),
            ..
        } => query_rows(
            conn,
            ingress_sql().shared.select_addressed.sql(),
            params![session_id.as_str(), cancel.turn_id.as_str()],
        )?,
        // A drain that refused a config command settles the turn-lane rows its
        // windows cover in the same transaction (FIG-3541, HoS decision 68).
        IngressSettlementIntent::Commands {
            refused_windows, ..
        } if !refused_windows.is_empty() => query_rows(
            conn,
            ingress_sql().shared.select_open_in_lane.sql(),
            params![session_id.as_str(), IngressLane::Turn.as_str()],
        )?,
        IngressSettlementIntent::Turn { cancel: None, .. }
        | IngressSettlementIntent::Commands { .. } => Vec::new(),
    };
    let observed_rows = observed
        .iter()
        .map(SessionIngressStoredRow::settlement_row)
        .collect::<Vec<_>>();
    let covered_rows = covered
        .iter()
        .map(SessionIngressStoredRow::settlement_row)
        .collect::<Vec<_>>();
    let plan = plan_ingress_settlement(settlement, fence, &observed_rows, &covered_rows)?;
    // The planned writes reach named rows and the covered rows a cancel or a
    // refused window disposes of; each is written through the claim it was
    // observed with.
    for row in covered {
        push_distinct(&mut observed, row);
    }
    let sql = &ingress_sql().shared;
    for write in &plan.writes {
        let (item_id, changed) = match write {
            IngressRowSettlement::Complete {
                item_id,
                cause,
                state,
            } => (
                item_id,
                tombstone_conn(conn, session_id, &observed, item_id, cause, *state, now)?,
            ),
            IngressRowSettlement::Drop { item_id, reason } => {
                let cause = IngressTerminalCause::Cancelled {
                    reason: reason.clone(),
                };
                (
                    item_id,
                    tombstone_conn(
                        conn,
                        session_id,
                        &observed,
                        item_id,
                        &cause,
                        IngressState::Cancelled,
                        now,
                    )?,
                )
            }
            IngressRowSettlement::Release { item_id } => {
                let claim = claim_of(&observed, item_id)?;
                let changed = conn
                    .execute(
                        sql.release_row.sql(),
                        params![
                            session_id.as_str(),
                            item_id.as_str(),
                            claim.claim_id,
                            claim.claim_token
                        ],
                    )
                    .map_err(sqlite_error)?;
                (item_id, changed)
            }
        };
        if changed != 1 {
            return Err(StoreError::IngressClaimSuperseded {
                session_id: session_id.clone(),
                claim_id: claim_of(&observed, item_id)
                    .map(|claim| claim.claim_id)
                    .unwrap_or_default(),
                item_id: item_id.to_string(),
            });
        }
    }
    let mut floors: Vec<(ProcessId, u64)> = Vec::new();
    for (process_id, sequence) in &plan.floor_raises {
        let floor = raise_wake_floor_conn(conn, session_id, process_id, *sequence)?;
        floors.retain(|(raised, _)| raised != process_id);
        floors.push((process_id.clone(), floor));
    }
    let mut affected = plan.affected;
    for record in &mut affected {
        if record.disposition == IngressUndeliveredDisposition::Drop
            && let Some((process_id, _)) = record.payload.wake_source()
        {
            record.fence_floor_after = floors
                .iter()
                .find(|(raised, _)| raised == process_id)
                .map(|(_, floor)| *floor);
        }
    }
    Ok(IngressSettlementReceipt { affected })
}

fn claim_of(
    observed: &[SessionIngressStoredRow],
    item_id: &lash_core_execution::store::IngressItemId,
) -> Result<lash_core_execution::store::IngressClaimIdentity, StoreError> {
    observed
        .iter()
        .find(|row| &row.item.item_id == item_id)
        .and_then(|row| row.claim.as_ref())
        .map(|claim| claim.identity.clone())
        .ok_or_else(|| {
            StoreError::Backend(format!(
                "session ingress settlement wrote item `{item_id}` it did not observe held"
            ))
        })
}

/// Tombstone one row a settlement moves: through the claim it was observed
/// with when it holds one, else as the open row the cancel observed.
fn tombstone_conn(
    conn: &Connection,
    session_id: &SessionId,
    observed: &[SessionIngressStoredRow],
    item_id: &lash_core_execution::store::IngressItemId,
    cause: &IngressTerminalCause,
    state: IngressState,
    now: u64,
) -> Result<usize, StoreError> {
    let cause_json = encode_ingress_terminal_cause(cause)?;
    let sql = &ingress_sql().shared;
    let held = observed
        .iter()
        .find(|row| &row.item.item_id == item_id)
        .and_then(|row| row.claim.as_ref());
    match held {
        Some(claim) => conn
            .execute(
                sql.tombstone_claimed.sql(),
                params![
                    session_id.as_str(),
                    item_id.as_str(),
                    claim.identity.claim_id,
                    claim.identity.claim_token,
                    tombstone_state(state),
                    cause_json,
                    now as i64
                ],
            )
            .map_err(sqlite_error),
        None => conn
            .execute(
                sql.tombstone_unclaimed.sql(),
                params![
                    session_id.as_str(),
                    item_id.as_str(),
                    tombstone_state(state),
                    cause_json,
                    now as i64
                ],
            )
            .map_err(sqlite_error),
    }
}

/// Withdraw one row for the host, inside the caller's transaction.
fn withdraw_row_conn(
    conn: &Connection,
    row: SessionIngressStoredRow,
    current_epoch: u64,
    selector: IngressWithdrawSelector,
    now: u64,
) -> Result<IngressWithdrawOutcome, StoreError> {
    match plan_withdrawal(&row.item, row.claim_epoch(), current_epoch, selector) {
        IngressWithdrawDecision::Held => Ok(IngressWithdrawOutcome::Held(row.item)),
        IngressWithdrawDecision::AlreadyCompleted => {
            Ok(IngressWithdrawOutcome::AlreadyCompleted(row.item))
        }
        IngressWithdrawDecision::AlreadyCancelled => {
            Ok(IngressWithdrawOutcome::AlreadyCancelled(row.item))
        }
        IngressWithdrawDecision::Withdraw {
            reason,
            floor_raise,
            mut record,
        } => {
            let cause = IngressTerminalCause::Cancelled { reason };
            let changed = conn
                .execute(
                    ingress_sql().shared.tombstone_unclaimed.sql(),
                    params![
                        row.item.session_id.as_str(),
                        row.item.item_id.as_str(),
                        tombstone_state(cause.state(row.item.kind())),
                        encode_ingress_terminal_cause(&cause)?,
                        now as i64
                    ],
                )
                .map_err(sqlite_error)?;
            if changed != 1 {
                return Err(StoreError::Backend(format!(
                    "session ingress item `{}` moved inside its withdrawal transaction",
                    row.item.item_id
                )));
            }
            if let Some((process_id, sequence)) = floor_raise {
                record.fence_floor_after = Some(raise_wake_floor_conn(
                    conn,
                    &row.item.session_id,
                    &process_id,
                    sequence,
                )?);
            }
            Ok(IngressWithdrawOutcome::Withdrawn(*record))
        }
    }
}

fn target_row_conn(
    conn: &Connection,
    session_id: &SessionId,
    target: &IngressWithdrawTarget,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    match target {
        IngressWithdrawTarget::ItemId(item_id) => {
            session_row_by_id(conn, session_id, item_id.as_str())
        }
        IngressWithdrawTarget::SourceKey(source_key) => query_row(
            conn,
            ingress_sql().shared.select_by_source_key.sql(),
            params![session_id.as_str(), source_key],
        ),
    }
}

/// Every row of `session_id`, tombstones included, for conformance probes.
pub(crate) fn session_ingress_rows_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Vec<IngressItem>, StoreError> {
    Ok(query_rows(
        conn,
        ingress_sql().shared.select_all.sql(),
        params![session_id.as_str()],
    )?
    .into_iter()
    .map(|row| row.item)
    .collect())
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

impl Store {
    async fn claim_ingress_lane(
        &self,
        fence: &DriveFence,
        lane: IngressLane,
        mode: ClaimMode,
        policy: IngressClaimPolicy,
    ) -> Result<Option<IngressClaim>, StoreError> {
        let fence = fence.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    let session_id = fence.session().clone();
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    require_fence_conn(tx, &session_id, &fence)?;
                    let candidates = lane_candidates_conn(tx, &session_id, lane)?;
                    let attempt = IngressClaimAttempt {
                        fence: &fence,
                        now_epoch_ms: now,
                    };
                    let plan = match lane {
                        IngressLane::Command => plan_command_claim(&attempt, &candidates)?,
                        IngressLane::Turn => plan_turn_claim(&attempt, mode, &policy, &candidates)?,
                    };
                    let Some(plan) = plan else {
                        return Ok(None);
                    };
                    install_claim_conn(tx, &session_id, &plan)?;
                    Ok(Some(plan.into_claim()))
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl SessionIngressStore for Store {
    async fn enqueue_ingress_item(
        &self,
        draft: IngressItemDraft,
    ) -> Result<IngressEnqueueOutcome, StoreError> {
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    let session_id = draft.session_id().clone();
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    super::ensure_session_not_closing_conn(tx, &session_id)?;
                    let digest = draft.submission_digest().map_err(|error| {
                        StoreError::RecordEncodingFailed {
                            record_kind: "SessionIngressSubmission".to_string(),
                            message: error.to_string(),
                        }
                    })?;
                    let sql = ingress_sql();
                    let by_source_key = match draft.source_key() {
                        Some(source_key) => query_row(
                            tx,
                            sql.shared.select_by_source_key.sql(),
                            params![session_id.as_str(), source_key],
                        )?,
                        None => None,
                    };
                    let by_item_id = match draft.provisioned_item_id() {
                        Some(item_id) => query_row(
                            tx,
                            sql.shared.select_by_item_id.sql(),
                            params![item_id.as_str()],
                        )?,
                        None => None,
                    };
                    let wake_floor = match draft.payload().wake_source() {
                        Some((process_id, _)) => wake_floor_conn(tx, &session_id, process_id)?,
                        None => None,
                    };
                    let turn_address_known = match draft.delivery().addressed_turn() {
                        Some(turn_id) => {
                            running_turn_conn(tx, &session_id)?.as_ref() == Some(turn_id)
                                || turn_ended_conn(tx, &session_id, turn_id)?
                        }
                        None => true,
                    };
                    let facts = SessionIngressAdmissionFacts {
                        by_source_key: by_source_key.map(|row| row.item),
                        by_item_id: by_item_id.map(|row| row.item),
                        wake_floor,
                        turn_address_known,
                    };
                    match decide_session_ingress_admission(&draft, &digest, facts)? {
                        SessionIngressAdmission::Answer(outcome) => Ok(*outcome),
                        SessionIngressAdmission::WakeConflict {
                            process_id,
                            sequence,
                            existing_item_id,
                        } => {
                            raise_wake_floor_conn(tx, &session_id, &process_id, sequence)?;
                            Ok(IngressEnqueueOutcome::Conflict { existing_item_id })
                        }
                        SessionIngressAdmission::Insert => {
                            let item_id = draft.admitted_item_id(now, nonce);
                            let insert = SessionIngressInsert::of(&draft, digest)?;
                            tx.query_row(
                                sql.sqlite.insert.sql(),
                                params![
                                    item_id.as_str(),
                                    session_id.as_str(),
                                    insert.lane,
                                    insert.kind,
                                    insert.source_key,
                                    insert.delivery_scope,
                                    insert.delivery_turn_id,
                                    insert.delivery_min_boundary,
                                    insert.submission_digest,
                                    insert.payload_json,
                                    insert.authority_json,
                                    insert.merge_key,
                                    insert.wake_process_id,
                                    insert
                                        .wake_sequence
                                        .map(|sequence| sql_counter_value(
                                            "session_ingress_wake_sequence",
                                            sequence
                                        ))
                                        .transpose()?,
                                    sql_counter_value("session_ingress_enqueued_at_ms", now)?,
                                ],
                                |row| row.get::<_, i64>(0),
                            )
                            .map_err(sqlite_error)?;
                            let row = session_row_by_id(tx, &session_id, item_id.as_str())?
                                .ok_or_else(|| {
                                    StoreError::Backend(
                                        "session ingress insert disappeared".to_string(),
                                    )
                                })?;
                            Ok(IngressEnqueueOutcome::Inserted(row.item))
                        }
                    }
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_session_commands(
        &self,
        fence: &DriveFence,
    ) -> Result<Option<IngressClaim>, StoreError> {
        self.claim_ingress_lane(
            fence,
            IngressLane::Command,
            ClaimMode::Idle,
            IngressClaimPolicy::bounded(1),
        )
        .await
    }

    async fn claim_turn_items(
        &self,
        fence: &DriveFence,
        mode: ClaimMode,
        policy: &IngressClaimPolicy,
    ) -> Result<Option<IngressClaim>, StoreError> {
        self.claim_ingress_lane(fence, IngressLane::Turn, mode, *policy)
            .await
    }

    async fn reclaim_ingress_claim(
        &self,
        fence: &DriveFence,
        claim: &IngressClaim,
    ) -> Result<IngressReclaimOutcome, StoreError> {
        let fence = fence.clone();
        let claim = claim.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    let session_id = fence.session().clone();
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    require_claim_fence_conn(tx, &fence, &claim)?;
                    let mut observed = Vec::with_capacity(claim.items.len());
                    for item in &claim.items {
                        if let Some(row) =
                            session_row_by_id(tx, &session_id, item.item_id.as_str())?
                        {
                            observed.push(row.into_candidate(false, false));
                        }
                    }
                    let attempt = IngressClaimAttempt {
                        fence: &fence,
                        now_epoch_ms: now,
                    };
                    match plan_reclaim(&attempt, &claim, &observed)? {
                        IngressReclaimDecision::Held(stored) => {
                            Ok(IngressReclaimOutcome::Reclaimed(stored))
                        }
                        IngressReclaimDecision::Ceded => Ok(IngressReclaimOutcome::Ceded),
                        IngressReclaimDecision::Reclaim(plan) => {
                            install_claim_conn(tx, &session_id, &plan)?;
                            Ok(IngressReclaimOutcome::Reclaimed(Box::new(
                                plan.into_claim(),
                            )))
                        }
                    }
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn abandon_ingress_claim(
        &self,
        fence: &DriveFence,
        claim: &IngressClaim,
    ) -> Result<(), StoreError> {
        let fence = fence.clone();
        let claim = claim.clone();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    require_claim_fence_conn(tx, &fence, &claim)?;
                    if claim.drive_epoch != fence.epoch() {
                        return Err(StoreError::StaleDriveFence {
                            session_id: claim.session_id.clone(),
                            fence_epoch: claim.drive_epoch,
                            current_epoch: fence.epoch(),
                        });
                    }
                    let identity = claim.identity();
                    tx.execute(
                        ingress_sql().shared.release_claim.sql(),
                        params![
                            claim.session_id.as_str(),
                            identity.claim_id,
                            identity.claim_token,
                            sql_counter_value("claim_drive_epoch", fence.epoch())?
                        ],
                    )
                    .map(|_| ())
                    .map_err(sqlite_error)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn withdraw_ingress_items(
        &self,
        session_id: &SessionId,
        targets: &[IngressWithdrawTarget],
    ) -> Result<Vec<IngressWithdrawReceipt>, StoreError> {
        let session_id = session_id.clone();
        let targets = targets.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let current_epoch = drive_epoch_conn(tx, &session_id)?.epoch;
                    let mut receipts = Vec::with_capacity(targets.len());
                    for target in targets {
                        let outcome = match target_row_conn(tx, &session_id, &target)? {
                            Some(row) => {
                                withdraw_row_conn(tx, row, current_epoch, target.selector(), now)?
                            }
                            None => IngressWithdrawOutcome::NotFound,
                        };
                        receipts.push(IngressWithdrawReceipt { target, outcome });
                    }
                    Ok(receipts)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn withdraw_ingress_suffix(
        &self,
        session_id: &SessionId,
        anchor: &IngressWithdrawTarget,
    ) -> Result<IngressSuffixWithdrawOutcome, StoreError> {
        let session_id = session_id.clone();
        let anchor = anchor.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let Some(anchor_row) = target_row_conn(tx, &session_id, &anchor)? else {
                        return Ok(IngressSuffixWithdrawOutcome::AnchorNotFound { anchor });
                    };
                    let current_epoch = drive_epoch_conn(tx, &session_id)?.epoch;
                    let rows = query_rows(
                        tx,
                        ingress_sql().shared.select_suffix.sql(),
                        params![
                            session_id.as_str(),
                            anchor_row.item.lane().as_str(),
                            sql_counter_value(
                                "session_ingress_enqueue_seq",
                                anchor_row.item.enqueue_seq
                            )?
                        ],
                    )?;
                    let outcomes = rows
                        .into_iter()
                        .map(|row| {
                            withdraw_row_conn(
                                tx,
                                row,
                                current_epoch,
                                IngressWithdrawSelector::Suffix,
                                now,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(IngressSuffixWithdrawOutcome::Outcomes { anchor, outcomes })
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_ingress_items(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<IngressItemRead>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let current_epoch = drive_epoch_conn(conn, &session_id)?.epoch;
                    let rows = query_rows(
                        conn,
                        ingress_sql().shared.select_open.sql(),
                        params![session_id.as_str()],
                    )?;
                    Ok(rows
                        .into_iter()
                        .map(|row| {
                            let status = match row.claim_epoch() {
                                Some(drive_epoch) if drive_epoch == current_epoch => {
                                    IngressReadStatus::Held { drive_epoch }
                                }
                                _ => IngressReadStatus::Pending,
                            };
                            IngressItemRead {
                                item: row.item,
                                status,
                            }
                        })
                        .collect())
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn vacuum_session_ingress(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write_flow(move |tx| {
                commit(
                    tx.execute(
                        ingress_sql().shared.vacuum.sql(),
                        params![session_id.as_str()],
                    )
                    .map(|removed| removed as u64)
                    .map_err(sqlite_error),
                )
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[async_trait::async_trait]
impl DriveEpochStore for Store {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
    ) -> Result<DriveEpochSeal, StoreError> {
        let session_id = session_id.clone();
        let admission = admission.clone();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let stored = drive_epoch_conn(tx, &session_id)?;
                    match decide_drive_epoch_seal(&session_id, &stored, &admission, observed_epoch)
                    {
                        DriveEpochSealDecision::Answer(seal) => Ok(seal),
                        DriveEpochSealDecision::Raise { next } => {
                            let changed = tx
                                .execute(
                                    session_sql().meta.seal_drive_epoch.sql(),
                                    params![
                                        session_id.as_str(),
                                        sql_counter_value("drive_epoch", observed_epoch)?,
                                        sql_counter_value("drive_epoch", next)?,
                                        admission.as_str()
                                    ],
                                )
                                .map_err(sqlite_error)?;
                            if changed != 1 {
                                return Ok(DriveEpochSeal::Superseded {
                                    epoch: drive_epoch_conn(tx, &session_id)?.epoch,
                                });
                            }
                            Ok(DriveEpochSeal::Sealed(sealed_drive_fence(
                                session_id.clone(),
                                next,
                                admission.clone(),
                            )))
                        }
                    }
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(drive_epoch_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }
}
