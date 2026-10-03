//! [`ShiftEpochStore`] for [`PostgresStore`]: the storage half of the
//! admission seal that raises a session's shift epoch, and the shift-fence
//! check every fenced commit runs in its own transaction (ADR 0105 §2).
//!
//! The shift epoch lives on the session's `session_meta` row; a presented
//! [`ShiftFence`] must name it, read in the same transaction as the write it
//! fences.

use super::*;
use lash_core_execution::store::{
    AdmissionId, RunHold, RunStartNonce, SessionFault, SessionFaultRecord, ShiftEpochSeal,
    ShiftEpochSealDecision, ShiftEpochStore, ShiftFence, StoredShiftEpoch, decide_run_hold,
    decide_shift_epoch_seal, require_current_shift_fence,
};
use lash_core_execution::store_backend_support::sealed_shift_fence;

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// The session's stored shift epoch and the admission that last raised it,
/// read inside the caller's transaction.
pub(crate) async fn shift_epoch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<StoredShiftEpoch, StoreError> {
    read_shift_epoch(tx, session_id, session_sql().meta.select_shift_epoch.sql()).await
}

/// The session's stored shift epoch, read inside the caller's transaction and
/// row-locked until it ends: a seal in flight is waited for, and a later seal
/// waits for the write the read decides (FIG-4200).
pub(crate) async fn shift_epoch_locked_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<StoredShiftEpoch, StoreError> {
    read_shift_epoch(
        tx,
        session_id,
        session_sql().meta_postgres.select_shift_epoch_locked.sql(),
    )
    .await
}

/// The session's stored shift epoch, read by `statement`: the plain read or
/// its row-locked fork.
async fn read_shift_epoch(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
    statement: &str,
) -> Result<StoredShiftEpoch, StoreError> {
    let row = sqlx::query(statement)
        .bind(session_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?
        .ok_or_else(|| StoreError::ShiftEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    let epoch: i64 = row.try_get(0).map_err(store_sqlx_error)?;
    let admission: Option<String> = row.try_get(1).map_err(store_sqlx_error)?;
    let run_start: Option<String> = row.try_get(2).map_err(store_sqlx_error)?;
    let closing: Option<i64> = row.try_get(3).map_err(store_sqlx_error)?;
    StoredShiftEpoch::from_stored(
        u64_from_sql("SessionMeta", "shift_epoch", epoch)?,
        admission,
        run_start,
        closing
            .map(|intent| u64_from_sql("SessionMeta", "closing_intent", intent))
            .transpose()?
            .map(lash_core_execution::store::ControlIntentId::from_sequence),
        row.try_get(4).map_err(store_sqlx_error)?,
        SessionFault::from_stored_columns(
            session_id,
            row.try_get(5).map_err(store_sqlx_error)?,
            row.try_get(6).map_err(store_sqlx_error)?,
        )?,
    )
}

/// The session's standing fault (ADR 0109 §9).
async fn session_fault_conn(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
) -> Result<Option<SessionFault>, StoreError> {
    sqlx::query(session_sql().meta.select_fault.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?
        .map(|row| {
            let json: String = row.try_get(0).map_err(store_sqlx_error)?;
            let at_ms: i64 = row.try_get(1).map_err(store_sqlx_error)?;
            SessionFault::from_stored(session_id.clone(), &json, at_ms)
        })
        .transpose()
}

/// Refuse `fence` unless it is the session's current shift fence, read in the
/// caller's transaction and row-locked until it ends: a seal in flight is
/// waited for and its epoch read, and a later seal waits for the write this
/// check fences (FIG-4044).
pub(super) async fn require_fence_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    fence: &ShiftFence,
) -> Result<(), StoreError> {
    let current = read_shift_epoch(
        tx,
        session_id,
        session_sql().meta_postgres.select_shift_epoch_locked.sql(),
    )
    .await?;
    require_current_shift_fence(session_id, fence, &current)
}

/// Refuse `fence` unless it is the session's current shift fence, for a read
/// that writes nothing and so holds no lock (FIG-3927 N4).
pub(super) async fn require_fence_conn(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
    fence: &ShiftFence,
) -> Result<(), StoreError> {
    let current = read_shift_epoch(
        connection,
        session_id,
        session_sql().meta.select_shift_epoch.sql(),
    )
    .await?;
    require_current_shift_fence(session_id, fence, &current)
}

/// Whether a successor's seal superseded the shift fence `commit` presents:
/// the fence of the admission its run was sealed under (ADR 0105 §2). A
/// superseded fence writes nothing. The caller answers a commit it already
/// stored from its receipt and refuses every other one with the returned
/// [`StoreError::StaleShiftFence`]: a shift that runs several runs in one
/// journal replays an earlier run's commit after a later run's seal
/// (FIG-4498). A commit that settles ingress must present a fence
/// ([`RuntimeCommit::validate_ingress_settlement`]).
///
/// [`RuntimeCommit::validate_ingress_settlement`]: lash_core_execution::store::RuntimeCommit::validate_ingress_settlement
pub(super) async fn commit_fence_superseded_tx(
    tx: &mut PgTx<'_>,
    commit: &lash_core_execution::store::RuntimeCommit,
) -> Result<Option<StoreError>, StoreError> {
    commit.validate_ingress_settlement()?;
    let Some(fence) = commit.shift_fence.as_ref() else {
        return Ok(None);
    };
    match require_fence_tx(tx, &commit.session_id, fence).await {
        Ok(()) => Ok(None),
        Err(superseded @ StoreError::StaleShiftFence { .. }) => Ok(Some(superseded)),
        Err(error) => Err(error),
    }
}

impl PostgresStore {
    /// Open a seal transaction for `session_id`, refusing a deleted session.
    async fn begin_seal_tx<'c>(
        &self,
        connection: &'c mut sqlx::pool::PoolConnection<sqlx::Postgres>,
        session_id: &SessionId,
    ) -> Result<crate::guarded_tx::GuardedTx<'c>, StoreError> {
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        Ok(tx)
    }
}

#[async_trait::async_trait]
impl ShiftEpochStore for PostgresStore {
    async fn seal_shift_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        run_start: &RunStartNonce,
        hold: Option<&RunHold>,
    ) -> Result<ShiftEpochSeal, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = self.begin_seal_tx(&mut connection, session_id).await?;
        // A seal that names its run reads who holds it: the row lock
        // orders the read against another seal and against a run admission,
        // whose fence check takes the same lock (FIG-4814).
        let stored = match hold {
            Some(_) => shift_epoch_locked_tx(&mut tx, session_id).await?,
            None => shift_epoch_tx(&mut tx, session_id).await?,
        };
        let refused = match hold {
            Some(hold) => decide_run_hold(
                hold,
                stored.epoch,
                crate::session_runs::held_run_conn(&mut tx, session_id, &hold.run)
                    .await?
                    .as_ref(),
                crate::session_runs::unfinished_run_conn(&mut tx, session_id)
                    .await?
                    .as_ref(),
                super::turn_cancel::pending_follow_on_tx(&mut tx, session_id, false)
                    .await?
                    .as_ref(),
            ),
            None => None,
        };
        let decision =
            decide_shift_epoch_seal(session_id, &stored, admission, observed_epoch, run_start);
        let seal = match (decision, refused) {
            (ShiftEpochSealDecision::Answer(seal), _) => seal,
            // The recorded executor of the run keeps its fence.
            (ShiftEpochSealDecision::Raise { .. }, Some(refused)) => refused,
            (ShiftEpochSealDecision::Raise { next }, None) => {
                let changed = sqlx::query(session_sql().meta.seal_shift_epoch.sql())
                    .bind(session_id.as_str())
                    .bind(sql_counter_value("shift_epoch", observed_epoch)?)
                    .bind(sql_counter_value("shift_epoch", next)?)
                    .bind(admission.as_str())
                    .bind(run_start.as_str())
                    .execute(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .rows_affected();
                if changed == 1 {
                    if let Some(hold) = hold {
                        crate::session_runs::record_run_hold_tx(&mut tx, session_id, hold).await?;
                    }
                    ShiftEpochSeal::Sealed(sealed_shift_fence(
                        session_id.clone(),
                        next,
                        admission.clone(),
                    ))
                } else {
                    ShiftEpochSeal::Superseded {
                        epoch: shift_epoch_tx(&mut tx, session_id).await?.epoch,
                    }
                }
            }
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(seal)
    }

    async fn shift_epoch(&self, session_id: &SessionId) -> Result<StoredShiftEpoch, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let stored = shift_epoch_tx(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(stored)
    }

    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<SessionFault>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let changed = sqlx::query(session_sql().meta.record_fault.sql())
            .bind(session_id.as_str())
            .bind(record.to_stored()?)
            .bind(sql_counter_value("fault_at_ms", at_ms)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if changed.rows_affected() == 1 {
            crate::session_factory::record_session_terminal(
                &mut tx,
                session_id,
                Some(&record.to_stored()?),
                sql_counter_value("fault_at_ms", at_ms)?,
            )
            .await?;
        }
        let stored = session_fault_conn(&mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(stored)
    }

    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionFault>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        session_fault_conn(&mut connection, session_id).await
    }

    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SessionFault>, StoreError> {
        let rows = sqlx::query(session_sql().meta.list_faults.sql())
            .bind(after.map_or("", SessionId::as_str))
            .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(store_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let session_id: String = row.try_get(0).map_err(store_sqlx_error)?;
                let json: String = row.try_get(1).map_err(store_sqlx_error)?;
                let at_ms: i64 = row.try_get(2).map_err(store_sqlx_error)?;
                SessionFault::from_stored(SessionId::parse(session_id)?, &json, at_ms)
            })
            .collect()
    }

    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let changed = sqlx::query(session_sql().meta.clear_fault.sql())
            .bind(session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(changed == 1)
    }
}
