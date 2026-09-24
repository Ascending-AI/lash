//! [`SessionIngressStore`] for [`PostgresSessionStore`]: the one session
//! ingress (ADR 0101).
//!
//! Every ingress transaction first takes the session history lock, so the
//! session's ingress writers are serialized: admission draws its
//! `enqueue_seq` under that lock, so enqueue order is per-session commit
//! order, and every claim, release, withdrawal and settlement observes the
//! rows it decides from under the same lock. The claim scan therefore reads a
//! fixed head set and never skips a locked row. The decisions themselves are
//! the shared planners'; this module reads rows, hands them over and executes
//! the planned writes.
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
    IngressClaim, IngressClaimIdentity, IngressClaimPolicy, IngressClaimSettlement,
    IngressEnqueueOutcome, IngressItem, IngressItemDraft, IngressItemId, IngressItemRead,
    IngressLane, IngressReadStatus, IngressReclaimOutcome, IngressSettlementIntent,
    IngressSettlementReceipt, IngressSuffixWithdrawOutcome, IngressTerminalCause,
    IngressUndeliveredDisposition, IngressWithdrawOutcome, IngressWithdrawReceipt,
    IngressWithdrawSelector, IngressWithdrawTarget, SessionIngressStore, StoredDriveEpoch,
    decide_drive_epoch_seal, require_current_drive_fence,
};
use lash_core_execution::store_backend_support::{
    SessionIngressAdmission, SessionIngressAdmissionFacts, SessionIngressInsert,
    SessionIngressRowColumns, SessionIngressStoredRow, decide_session_ingress_admission,
    encode_ingress_terminal_cause, sealed_drive_fence,
};
use lash_sansio::ProcessId;

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

fn ingress_sql() -> &'static crate::session_ingress::SessionIngressSql {
    crate::session_ingress::session_ingress_sql()
}

fn row_columns(row: &sqlx::postgres::PgRow) -> Result<SessionIngressRowColumns, StoreError> {
    let get = store_sqlx_error;
    Ok(SessionIngressRowColumns {
        enqueue_seq: row.try_get(0).map_err(get)?,
        item_id: row.try_get(1).map_err(get)?,
        session_id: row.try_get(2).map_err(get)?,
        kind: row.try_get(3).map_err(get)?,
        source_key: row.try_get(4).map_err(get)?,
        delivery_scope: row.try_get(5).map_err(get)?,
        delivery_turn_id: row.try_get(6).map_err(get)?,
        delivery_min_boundary: row.try_get(7).map_err(get)?,
        submission_digest: row.try_get(8).map_err(get)?,
        payload_json: row.try_get(9).map_err(get)?,
        authority_json: row.try_get(10).map_err(get)?,
        merge_key: row.try_get(11).map_err(get)?,
        state: row.try_get(12).map_err(get)?,
        terminal_cause_json: row.try_get(13).map_err(get)?,
        enqueued_at_ms: row.try_get(14).map_err(get)?,
        terminal_at_ms: row.try_get(15).map_err(get)?,
        claim_id: row.try_get(16).map_err(get)?,
        claim_token: row.try_get(17).map_err(get)?,
        claim_admission_id: row.try_get(18).map_err(get)?,
        claim_fencing_token: row.try_get(19).map_err(get)?,
        claim_drive_epoch: row.try_get(20).map_err(get)?,
        claim_turn_id: row.try_get(21).map_err(get)?,
    })
}

fn decode_rows(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<SessionIngressStoredRow>, StoreError> {
    rows.iter().map(|row| row_columns(row)?.decode()).collect()
}

fn decode_row(
    row: Option<sqlx::postgres::PgRow>,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    row.map(|row| row_columns(&row)?.decode()).transpose()
}

async fn rows_by_session_tx(
    tx: &mut PgTx<'_>,
    sql: &str,
    session_id: &SessionId,
) -> Result<Vec<SessionIngressStoredRow>, StoreError> {
    decode_rows(
        sqlx::query(sql)
            .bind(session_id.as_str())
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
    )
}

async fn row_by_source_key_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    source_key: &str,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    decode_row(
        sqlx::query(ingress_sql().shared.select_by_source_key.sql())
            .bind(session_id.as_str())
            .bind(source_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
    )
}

async fn row_by_item_id_tx(
    tx: &mut PgTx<'_>,
    item_id: &str,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    decode_row(
        sqlx::query(ingress_sql().shared.select_by_item_id.sql())
            .bind(item_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
    )
}

/// The row `item_id` names, if it belongs to `session_id`.
async fn session_row_by_id_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    item_id: &str,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    Ok(row_by_item_id_tx(tx, item_id)
        .await?
        .filter(|row| row.item.session_id == *session_id))
}

/// Whether turn `turn_id` of `session_id` has its final commit recorded.
async fn turn_ended_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> Result<bool, StoreError> {
    let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
        session_id, turn_id,
    )?;
    sqlx::query_scalar::<_, bool>(session_sql().turn_commits.exists_for_turn.sql())
        .bind(session_id.as_str())
        .bind(key)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

/// The physical turn the session's unfinished logical run is at, if any.
async fn running_turn_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<Option<TurnId>, StoreError> {
    Ok(load_run_tx(tx, session_id, None)
        .await?
        .filter(|admission| admission.terminal.is_none())
        .map(|admission| admission.position.turn_id))
}

/// The session's stored drive epoch and the admission that last raised it,
/// read inside the caller's transaction.
async fn drive_epoch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<StoredDriveEpoch, StoreError> {
    let row = sqlx::query(session_sql().meta.select_drive_epoch.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .ok_or_else(|| StoreError::DriveEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    let epoch: i64 = row.try_get(0).map_err(store_sqlx_error)?;
    let admission: Option<String> = row.try_get(1).map_err(store_sqlx_error)?;
    Ok(StoredDriveEpoch {
        epoch: u64_from_sql("SessionMeta", "drive_epoch", epoch)?,
        admission: admission.map(AdmissionId::new),
    })
}

/// Refuse `fence` unless it is the session's current drive fence, read in the
/// caller's transaction.
async fn require_fence_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    fence: &DriveFence,
) -> Result<(), StoreError> {
    let current = drive_epoch_tx(tx, session_id).await?;
    require_current_drive_fence(session_id, fence, &current)
}

/// Refuse `fence` unless it is current and `claim` belongs to its session.
/// The fence is checked first, so a stale fence is refused as stale.
async fn require_claim_fence_tx(
    tx: &mut PgTx<'_>,
    fence: &DriveFence,
    claim: &IngressClaim,
) -> Result<(), StoreError> {
    require_fence_tx(tx, fence.session(), fence).await?;
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

async fn wake_floor_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    process_id: &ProcessId,
) -> Result<Option<u64>, StoreError> {
    sqlx::query_scalar::<_, i64>(crate::process_sql::process_sql().fence.select_floor.sql())
        .bind(session_id.as_str())
        .bind(process_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .map(|floor| u64_from_sql("WakeRedeliveryFence", "allocation_floor", floor))
        .transpose()
}

/// Raise the session's redelivery floor for `process_id` to at least
/// `sequence`, in the caller's transaction, and return the floor after.
async fn raise_wake_floor_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    process_id: &ProcessId,
    sequence: u64,
) -> Result<u64, StoreError> {
    sqlx::query(
        crate::process_sql::process_sql()
            .fence_postgres
            .upsert_max
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(process_id.as_str())
    .bind(sql_counter_value("wake_redelivery_floor", sequence)?)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    wake_floor_tx(tx, session_id, process_id)
        .await?
        .ok_or_else(|| StoreError::StoredDataCorrupt {
            record_kind: "WakeRedeliveryFence",
            message: "a raised floor must be readable".to_string(),
        })
}

/// The lane's non-terminal rows as a claim attempt sees them.
async fn lane_candidates_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    lane: IngressLane,
) -> Result<Vec<IngressClaimCandidate>, StoreError> {
    let rows = decode_rows(
        sqlx::query(ingress_sql().shared.select_open_in_lane.sql())
            .bind(session_id.as_str())
            .bind(lane.as_str())
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
    )?;
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let addressed_turn_ended = match row.item.delivery.addressed_turn() {
            Some(turn_id) => turn_ended_tx(tx, session_id, turn_id).await?,
            None => false,
        };
        let claim_turn = row
            .claim
            .as_ref()
            .and_then(|claim| claim.claim_turn_id.clone());
        let claim_turn_ended = match claim_turn {
            Some(turn_id) => turn_ended_tx(tx, session_id, &turn_id).await?,
            None => false,
        };
        candidates.push(row.into_candidate(addressed_turn_ended, claim_turn_ended));
    }
    Ok(candidates)
}

/// Install `plan`'s claim on each row it takes. A row another writer moved
/// since the scan fails the whole attempt.
async fn install_claim_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    plan: &IngressClaimPlan,
) -> Result<(), StoreError> {
    let stamp = plan.stamp();
    let drive_epoch = sql_counter_value("session_ingress_claim_drive_epoch", stamp.drive_epoch)?;
    for write in plan.writes() {
        let changed = sqlx::query(ingress_sql().shared.claim_row.sql())
            .bind(session_id.as_str())
            .bind(write.item_id.as_str())
            .bind(&stamp.claim_id)
            .bind(&stamp.claim_token)
            .bind(stamp.admission.as_str())
            .bind(sql_counter_value(
                "session_ingress_claim_fencing_token",
                write.next_claim_fencing_token,
            )?)
            .bind(drive_epoch)
            .bind(plan.claim_turn_id().map(TurnId::as_str))
            .bind(sql_counter_value(
                "session_ingress_claim_fencing_token",
                write.observed_claim_fencing_token,
            )?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        if changed != 1 {
            return Err(StoreError::Backend(format!(
                "session ingress item `{}` moved inside its claim transaction",
                write.item_id
            )));
        }
    }
    Ok(())
}

fn claim_of(
    observed: &[SessionIngressStoredRow],
    item_id: &IngressItemId,
) -> Result<IngressClaimIdentity, StoreError> {
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
async fn tombstone_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    observed: &[SessionIngressStoredRow],
    item_id: &IngressItemId,
    cause: &IngressTerminalCause,
    now: u64,
) -> Result<u64, StoreError> {
    let cause_json = encode_ingress_terminal_cause(cause)?;
    let sql = &ingress_sql().shared;
    let now = sql_counter_value("session_ingress_terminal_at_ms", now)?;
    let held = observed
        .iter()
        .find(|row| &row.item.item_id == item_id)
        .and_then(|row| row.claim.as_ref());
    let query = match held {
        Some(held) => {
            let claim = held.identity.clone();
            sqlx::query(sql.tombstone_claimed.sql())
                .bind(session_id.as_str())
                .bind(item_id.as_str())
                .bind(claim.claim_id)
                .bind(claim.claim_token)
                .bind(cause.state().as_str())
                .bind(cause_json)
                .bind(now)
        }
        None => sqlx::query(sql.tombstone_unclaimed.sql())
            .bind(session_id.as_str())
            .bind(item_id.as_str())
            .bind(cause.state().as_str())
            .bind(cause_json)
            .bind(now),
    };
    Ok(query
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected())
}

/// Execute one settlement inside the caller's transaction, which already
/// holds the session history lock (ADR 0101 §7, §9, §10, §12). The commit
/// path runs this beside the head write; every wake terminal raises its
/// floor here, in the same transaction.
pub(crate) async fn apply_session_ingress_settlement_tx(
    tx: &mut PgTx<'_>,
    fence: &DriveFence,
    settlement: &IngressClaimSettlement,
    now: u64,
) -> Result<IngressSettlementReceipt, StoreError> {
    let session_id = &settlement.session_id;
    require_fence_tx(tx, session_id, fence).await?;
    // Every row a named claim still holds, and every named row: the planner
    // refuses a settlement that leaves part of a claim unnamed.
    let mut observed = Vec::new();
    for claim in &settlement.claims {
        for row in decode_rows(
            sqlx::query(ingress_sql().shared.select_claimed.sql())
                .bind(session_id.as_str())
                .bind(&claim.identity.claim_id)
                .fetch_all(&mut **tx)
                .await
                .map_err(store_sqlx_error)?,
        )? {
            push_distinct(&mut observed, row);
        }
        for item_id in &claim.item_ids {
            if let Some(row) = session_row_by_id_tx(tx, session_id, item_id.as_str()).await? {
                push_distinct(&mut observed, row);
            }
        }
    }
    let addressed = match &settlement.intent {
        IngressSettlementIntent::Turn {
            cancel: Some(cancel),
            ..
        } => decode_rows(
            sqlx::query(ingress_sql().shared.select_addressed.sql())
                .bind(session_id.as_str())
                .bind(cancel.turn_id.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(store_sqlx_error)?,
        )?,
        IngressSettlementIntent::Turn { cancel: None, .. }
        | IngressSettlementIntent::Commands { .. } => Vec::new(),
    };
    let observed_rows = observed
        .iter()
        .map(SessionIngressStoredRow::settlement_row)
        .collect::<Vec<_>>();
    let addressed_rows = addressed
        .iter()
        .map(SessionIngressStoredRow::settlement_row)
        .collect::<Vec<_>>();
    let plan = plan_ingress_settlement(settlement, fence, &observed_rows, &addressed_rows)?;
    // The planned writes reach named rows and the addressed rows a cancel
    // disposes of; each is written through the claim it was observed with.
    for row in addressed {
        push_distinct(&mut observed, row);
    }
    for write in &plan.writes {
        let (item_id, changed) = match write {
            IngressRowSettlement::Complete { item_id, cause } => (
                item_id,
                tombstone_tx(tx, session_id, &observed, item_id, cause, now).await?,
            ),
            IngressRowSettlement::Drop { item_id, reason } => {
                let cause = IngressTerminalCause::Cancelled {
                    reason: reason.clone(),
                };
                (
                    item_id,
                    tombstone_tx(tx, session_id, &observed, item_id, &cause, now).await?,
                )
            }
            IngressRowSettlement::Release { item_id } => {
                let claim = claim_of(&observed, item_id)?;
                let changed = sqlx::query(ingress_sql().shared.release_row.sql())
                    .bind(session_id.as_str())
                    .bind(item_id.as_str())
                    .bind(claim.claim_id)
                    .bind(claim.claim_token)
                    .execute(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .rows_affected();
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
        let floor = raise_wake_floor_tx(tx, session_id, process_id, *sequence).await?;
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

/// Withdraw one row for the host, inside the caller's transaction.
async fn withdraw_row_tx(
    tx: &mut PgTx<'_>,
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
            let changed = sqlx::query(ingress_sql().shared.tombstone_unclaimed.sql())
                .bind(row.item.session_id.as_str())
                .bind(row.item.item_id.as_str())
                .bind(cause.state().as_str())
                .bind(encode_ingress_terminal_cause(&cause)?)
                .bind(sql_counter_value("session_ingress_terminal_at_ms", now)?)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .rows_affected();
            if changed != 1 {
                return Err(StoreError::Backend(format!(
                    "session ingress item `{}` moved inside its withdrawal transaction",
                    row.item.item_id
                )));
            }
            if let Some((process_id, sequence)) = floor_raise {
                record.fence_floor_after = Some(
                    raise_wake_floor_tx(tx, &row.item.session_id, &process_id, sequence).await?,
                );
            }
            Ok(IngressWithdrawOutcome::Withdrawn(*record))
        }
    }
}

async fn target_row_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    target: &IngressWithdrawTarget,
) -> Result<Option<SessionIngressStoredRow>, StoreError> {
    match target {
        IngressWithdrawTarget::ItemId(item_id) => {
            session_row_by_id_tx(tx, session_id, item_id.as_str()).await
        }
        IngressWithdrawTarget::SourceKey(source_key) => {
            row_by_source_key_tx(tx, session_id, source_key).await
        }
    }
}

/// Every row of `session_id`, tombstones included, for conformance probes.
pub(crate) async fn session_ingress_rows_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<Vec<IngressItem>, StoreError> {
    Ok(
        rows_by_session_tx(tx, ingress_sql().shared.select_all.sql(), session_id)
            .await?
            .into_iter()
            .map(|row| row.item)
            .collect(),
    )
}

impl PostgresSessionStore {
    /// Open an ingress transaction for `session_id`: the session history
    /// lock, then the deleted-session refusal.
    async fn begin_ingress_tx<'c>(
        &self,
        connection: &'c mut sqlx::pool::PoolConnection<sqlx::Postgres>,
        session_id: &SessionId,
    ) -> Result<PgTx<'c>, StoreError> {
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        Ok(tx)
    }

    async fn claim_ingress_lane(
        &self,
        fence: &DriveFence,
        lane: IngressLane,
        mode: ClaimMode,
        policy: IngressClaimPolicy,
    ) -> Result<Option<IngressClaim>, StoreError> {
        let session_id = fence.session().clone();
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, &session_id).await?;
        require_fence_tx(&mut tx, &session_id, fence).await?;
        let candidates = lane_candidates_tx(&mut tx, &session_id, lane).await?;
        let attempt = IngressClaimAttempt {
            fence,
            now_epoch_ms: now,
        };
        let plan = match lane {
            IngressLane::Command => plan_command_claim(&attempt, &candidates)?,
            IngressLane::Turn => plan_turn_claim(&attempt, mode, &policy, &candidates)?,
        };
        let claim = match plan {
            Some(plan) => {
                install_claim_tx(&mut tx, &session_id, &plan).await?;
                Some(plan.into_claim())
            }
            None => None,
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(claim)
    }

    /// Execute one settlement in a transaction of its own, for the
    /// conformance seam.
    pub(crate) async fn settle_session_ingress(
        &self,
        fence: &DriveFence,
        settlement: IngressClaimSettlement,
    ) -> Result<IngressSettlementReceipt, StoreError> {
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self
            .begin_ingress_tx(&mut connection, &settlement.session_id)
            .await?;
        let receipt = apply_session_ingress_settlement_tx(&mut tx, fence, &settlement, now).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(receipt)
    }

    /// Every row of `session_id`, for the conformance seam.
    pub(crate) async fn session_ingress_rows(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<IngressItem>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let rows = session_ingress_rows_tx(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(rows)
    }
}

#[async_trait::async_trait]
impl SessionIngressStore for PostgresSessionStore {
    async fn enqueue_ingress_item(
        &self,
        draft: IngressItemDraft,
    ) -> Result<IngressEnqueueOutcome, StoreError> {
        let session_id = draft.session_id().clone();
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, &session_id).await?;
        let digest =
            draft
                .submission_digest()
                .map_err(|error| StoreError::RecordEncodingFailed {
                    record_kind: "SessionIngressSubmission".to_string(),
                    message: error.to_string(),
                })?;
        let by_source_key = match draft.source_key() {
            Some(source_key) => row_by_source_key_tx(&mut tx, &session_id, source_key).await?,
            None => None,
        };
        let by_item_id = match draft.provisioned_item_id() {
            Some(item_id) => row_by_item_id_tx(&mut tx, item_id.as_str()).await?,
            None => None,
        };
        let wake_floor = match draft.payload().wake_source() {
            Some((process_id, _)) => wake_floor_tx(&mut tx, &session_id, process_id).await?,
            None => None,
        };
        let turn_address_known = match draft.delivery().addressed_turn() {
            Some(turn_id) => {
                running_turn_tx(&mut tx, &session_id).await?.as_ref() == Some(turn_id)
                    || turn_ended_tx(&mut tx, &session_id, turn_id).await?
            }
            None => true,
        };
        let facts = SessionIngressAdmissionFacts {
            by_source_key: by_source_key.map(|row| row.item),
            by_item_id: by_item_id.map(|row| row.item),
            wake_floor,
            turn_address_known,
        };
        let outcome = match decide_session_ingress_admission(&draft, &digest, facts)? {
            SessionIngressAdmission::Answer(outcome) => *outcome,
            SessionIngressAdmission::WakeConflict {
                process_id,
                sequence,
                existing_item_id,
            } => {
                raise_wake_floor_tx(&mut tx, &session_id, &process_id, sequence).await?;
                IngressEnqueueOutcome::Conflict { existing_item_id }
            }
            SessionIngressAdmission::Insert => {
                let enqueue_seq: i64 =
                    sqlx::query_scalar(ingress_sql().postgres.select_next_enqueue_seq.sql())
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(store_sqlx_error)?;
                let nonce = u64_from_sql("SessionIngressItem", "enqueue_seq", enqueue_seq)?;
                let item_id = draft.admitted_item_id(now, nonce);
                let insert = SessionIngressInsert::of(&draft, digest)?;
                sqlx::query(ingress_sql().postgres.insert.sql())
                    .bind(enqueue_seq)
                    .bind(item_id.as_str())
                    .bind(session_id.as_str())
                    .bind(insert.lane)
                    .bind(insert.kind)
                    .bind(insert.source_key)
                    .bind(insert.delivery_scope)
                    .bind(insert.delivery_turn_id)
                    .bind(insert.delivery_min_boundary)
                    .bind(insert.submission_digest)
                    .bind(insert.payload_json)
                    .bind(insert.authority_json)
                    .bind(insert.merge_key)
                    .bind(insert.wake_process_id)
                    .bind(
                        insert
                            .wake_sequence
                            .map(|sequence| {
                                sql_counter_value("session_ingress_wake_sequence", sequence)
                            })
                            .transpose()?,
                    )
                    .bind(sql_counter_value("session_ingress_enqueued_at_ms", now)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?;
                let row = session_row_by_id_tx(&mut tx, &session_id, item_id.as_str())
                    .await?
                    .ok_or_else(|| {
                        StoreError::Backend("session ingress insert disappeared".to_string())
                    })?;
                IngressEnqueueOutcome::Inserted(row.item)
            }
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(outcome)
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
        let session_id = fence.session().clone();
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, &session_id).await?;
        require_claim_fence_tx(&mut tx, fence, claim).await?;
        let mut observed = Vec::with_capacity(claim.items.len());
        for item in &claim.items {
            if let Some(row) =
                session_row_by_id_tx(&mut tx, &session_id, item.item_id.as_str()).await?
            {
                observed.push(row.into_candidate(false, false));
            }
        }
        let attempt = IngressClaimAttempt {
            fence,
            now_epoch_ms: now,
        };
        let outcome = match plan_reclaim(&attempt, claim, &observed)? {
            IngressReclaimDecision::Held(stored) => IngressReclaimOutcome::Reclaimed(stored),
            IngressReclaimDecision::Ceded => IngressReclaimOutcome::Ceded,
            IngressReclaimDecision::Reclaim(plan) => {
                install_claim_tx(&mut tx, &session_id, &plan).await?;
                IngressReclaimOutcome::Reclaimed(Box::new(plan.into_claim()))
            }
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(outcome)
    }

    async fn abandon_ingress_claim(
        &self,
        fence: &DriveFence,
        claim: &IngressClaim,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self
            .begin_ingress_tx(&mut connection, fence.session())
            .await?;
        require_claim_fence_tx(&mut tx, fence, claim).await?;
        if claim.drive_epoch != fence.epoch() {
            return Err(StoreError::StaleDriveFence {
                session_id: claim.session_id.clone(),
                fence_epoch: claim.drive_epoch,
                current_epoch: fence.epoch(),
            });
        }
        sqlx::query(ingress_sql().shared.release_claim.sql())
            .bind(claim.session_id.as_str())
            .bind(&claim.claim_id)
            .bind(&claim.claim_token)
            .bind(sql_counter_value("claim_drive_epoch", fence.epoch())?)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }

    async fn withdraw_ingress_items(
        &self,
        session_id: &SessionId,
        targets: &[IngressWithdrawTarget],
    ) -> Result<Vec<IngressWithdrawReceipt>, StoreError> {
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, session_id).await?;
        let current_epoch = drive_epoch_tx(&mut tx, session_id).await?.epoch;
        let mut receipts = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome = match target_row_tx(&mut tx, session_id, target).await? {
                Some(row) => {
                    withdraw_row_tx(&mut tx, row, current_epoch, target.selector(), now).await?
                }
                None => IngressWithdrawOutcome::NotFound,
            };
            receipts.push(IngressWithdrawReceipt {
                target: target.clone(),
                outcome,
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(receipts)
    }

    async fn withdraw_ingress_suffix(
        &self,
        session_id: &SessionId,
        anchor: &IngressWithdrawTarget,
    ) -> Result<IngressSuffixWithdrawOutcome, StoreError> {
        let now = self.clock.timestamp_ms();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, session_id).await?;
        let Some(anchor_row) = target_row_tx(&mut tx, session_id, anchor).await? else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(IngressSuffixWithdrawOutcome::AnchorNotFound {
                anchor: anchor.clone(),
            });
        };
        let current_epoch = drive_epoch_tx(&mut tx, session_id).await?.epoch;
        let rows = decode_rows(
            sqlx::query(ingress_sql().shared.select_suffix.sql())
                .bind(session_id.as_str())
                .bind(anchor_row.item.lane().as_str())
                .bind(sql_counter_value(
                    "session_ingress_enqueue_seq",
                    anchor_row.item.enqueue_seq,
                )?)
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?,
        )?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(
                withdraw_row_tx(
                    &mut tx,
                    row,
                    current_epoch,
                    IngressWithdrawSelector::Suffix,
                    now,
                )
                .await?,
            );
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(IngressSuffixWithdrawOutcome::Outcomes {
            anchor: anchor.clone(),
            outcomes,
        })
    }

    async fn list_ingress_items(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<IngressItemRead>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let current_epoch = drive_epoch_tx(&mut tx, session_id).await?.epoch;
        let rows =
            rows_by_session_tx(&mut tx, ingress_sql().shared.select_open.sql(), session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
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
    }

    async fn vacuum_session_ingress(&self, session_id: &SessionId) -> Result<u64, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, session_id).await?;
        let removed = sqlx::query(ingress_sql().shared.vacuum.sql())
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(removed)
    }
}

#[async_trait::async_trait]
impl DriveEpochStore for PostgresSessionStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
    ) -> Result<DriveEpochSeal, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_ingress_tx(&mut connection, session_id).await?;
        let stored = drive_epoch_tx(&mut tx, session_id).await?;
        let seal = match decide_drive_epoch_seal(session_id, &stored, admission, observed_epoch) {
            DriveEpochSealDecision::Answer(seal) => seal,
            DriveEpochSealDecision::Raise { next } => {
                let changed = sqlx::query(session_sql().meta.seal_drive_epoch.sql())
                    .bind(session_id.as_str())
                    .bind(sql_counter_value("drive_epoch", observed_epoch)?)
                    .bind(sql_counter_value("drive_epoch", next)?)
                    .bind(admission.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .rows_affected();
                if changed == 1 {
                    DriveEpochSeal::Sealed(sealed_drive_fence(
                        session_id.clone(),
                        next,
                        admission.clone(),
                    ))
                } else {
                    DriveEpochSeal::Superseded {
                        epoch: drive_epoch_tx(&mut tx, session_id).await?.epoch,
                    }
                }
            }
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(seal)
    }

    async fn drive_epoch(&self, session_id: &SessionId) -> Result<StoredDriveEpoch, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let stored = drive_epoch_tx(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(stored)
    }
}
