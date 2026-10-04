//! [`ShiftEpochStore`] for [`SqliteStore`]: the storage half of the admission seal
//! that raises a session's shift epoch, and the shift-fence checks every
//! fenced commit runs in its own transaction (ADR 0105 §2).
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

/// The session's stored shift epoch and the admission that last raised it,
/// read inside the caller's transaction.
pub(crate) fn shift_epoch_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<StoredShiftEpoch, StoreError> {
    let row = conn
        .query_row(
            session_sql().meta.select_shift_epoch.sql(),
            params![session_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?
        .ok_or_else(|| StoreError::ShiftEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    StoredShiftEpoch::from_stored(
        u64::try_from(row.0)
            .map_err(|_| stored_data_corrupt("SessionMeta", "shift_epoch must be non-negative"))?,
        row.1,
        row.2,
        row.3
            .map(|intent| {
                u64::try_from(intent).map_err(|_| {
                    stored_data_corrupt("SessionMeta", "closing_intent must be non-negative")
                })
            })
            .transpose()?
            .map(lash_core_execution::store::ControlIntentId::from_sequence),
        row.4,
        SessionFault::from_stored_columns(session_id, row.5, row.6)?,
    )
}

/// The session's standing fault (ADR 0109 §9), read inside the caller's
/// transaction.
fn session_fault_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionFault>, StoreError> {
    conn.query_row(
        session_sql().meta.select_fault.sql(),
        params![session_id.as_str()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )
    .optional()
    .map_err(sqlite_error)?
    .map(|(json, at_ms)| SessionFault::from_stored(session_id.clone(), &json, at_ms))
    .transpose()
}

/// Refuse `fence` unless it is the session's current shift fence, read in the
/// caller's transaction.
pub(super) fn require_fence_conn(
    conn: &Connection,
    session_id: &SessionId,
    fence: &ShiftFence,
) -> Result<(), StoreError> {
    let current = shift_epoch_conn(conn, session_id)?;
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
pub(super) fn commit_fence_superseded_conn(
    conn: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
) -> Result<Option<StoreError>, StoreError> {
    commit.validate_ingress_settlement()?;
    let Some(fence) = commit.shift_fence.as_ref() else {
        return Ok(None);
    };
    match require_fence_conn(conn, &commit.session_id, fence) {
        Ok(()) => Ok(None),
        Err(superseded @ StoreError::StaleShiftFence { .. }) => Ok(Some(superseded)),
        Err(error) => Err(error),
    }
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

pub(crate) fn seal_shift_epoch_conn(
    tx: &Connection,
    session_id: &SessionId,
    admission: &AdmissionId,
    observed_epoch: u64,
    run_start: &RunStartNonce,
    hold: Option<&RunHold>,
) -> Result<ShiftEpochSeal, StoreError> {
    ensure_session_not_deleted_conn(tx, session_id)?;
    let stored = shift_epoch_conn(tx, session_id)?;
    match decide_shift_epoch_seal(session_id, &stored, admission, observed_epoch, run_start) {
        ShiftEpochSealDecision::Answer(seal) => Ok(seal),
        ShiftEpochSealDecision::Raise { next } => {
            // The recorded executor of the run keeps its
            // fence (FIG-4814).
            if let Some(hold) = hold
                && let Some(refused) = decide_run_hold(
                    hold,
                    stored.epoch,
                    crate::session_runs::held_run_conn(tx, session_id, &hold.run)?.as_ref(),
                    crate::session_runs::unfinished_run_conn(tx, session_id)?.as_ref(),
                    super::turn_cancel::pending_follow_on_conn(tx, session_id)?.as_ref(),
                )
            {
                return Ok(refused);
            }
            let changed = tx
                .execute(
                    session_sql().meta.seal_shift_epoch.sql(),
                    params![
                        session_id.as_str(),
                        sql_counter_value("shift_epoch", observed_epoch)?,
                        sql_counter_value("shift_epoch", next)?,
                        admission.as_str(),
                        run_start.as_str()
                    ],
                )
                .map_err(sqlite_error)?;
            if changed != 1 {
                return Ok(ShiftEpochSeal::Superseded {
                    epoch: shift_epoch_conn(tx, session_id)?.epoch,
                });
            }
            if let Some(hold) = hold {
                crate::session_runs::record_run_hold_conn(tx, session_id, hold)?;
            }
            Ok(ShiftEpochSeal::Sealed(sealed_shift_fence(
                session_id.clone(),
                next,
                admission.clone(),
            )))
        }
    }
}

#[async_trait::async_trait]
impl ShiftEpochStore for SqliteStore {
    async fn seal_shift_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        run_start: &RunStartNonce,
        hold: Option<&RunHold>,
    ) -> Result<ShiftEpochSeal, StoreError> {
        let session_id = session_id.clone();
        let admission = admission.clone();
        let run_start = run_start.clone();
        let hold = hold.cloned();
        self.conn
            .write_flow(move |tx| {
                commit(seal_shift_epoch_conn(
                    tx,
                    &session_id,
                    &admission,
                    observed_epoch,
                    &run_start,
                    hold.as_ref(),
                ))
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn shift_epoch(&self, session_id: &SessionId) -> Result<StoredShiftEpoch, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(shift_epoch_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn record_session_fault(
        &self,
        session_id: &SessionId,
        record: &SessionFaultRecord,
        at_ms: u64,
    ) -> Result<Option<SessionFault>, StoreError> {
        let session_id = session_id.clone();
        let fault_json = record.to_stored()?;
        let at_ms = sql_counter_value("fault_at_ms", at_ms)?;
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    let changed = tx
                        .execute(
                            session_sql().meta.record_fault.sql(),
                            params![session_id.as_str(), fault_json, at_ms],
                        )
                        .map_err(sqlite_error)?;
                    if changed == 1 {
                        crate::catalog::catalog_reads::record_session_terminal(
                            tx,
                            &session_id,
                            Some(&fault_json),
                            at_ms,
                        )?;
                    }
                    session_fault_conn(tx, &session_id)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn session_fault(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionFault>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(session_fault_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn list_session_faults(
        &self,
        after: Option<&SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<SessionFault>, StoreError> {
        let after = after.map_or_else(String::new, |id| id.as_str().to_owned());
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows: Vec<(String, String, i64)> = self
            .conn
            .call(move |conn| {
                let mut select = conn.prepare_cached(session_sql().meta.list_faults.sql())?;
                select
                    .query_map(params![after, limit], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .collect()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(session_id, json, at_ms)| {
                SessionFault::from_stored(SessionId::parse(session_id)?, &json, at_ms)
            })
            .collect()
    }

    async fn clear_session_fault(&self, session_id: &SessionId) -> Result<bool, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .write_flow(move |tx| {
                commit(
                    tx.execute(
                        session_sql().meta.clear_fault.sql(),
                        params![session_id.as_str()],
                    )
                    .map(|changed| changed == 1)
                    .map_err(sqlite_error),
                )
            })
            .await
            .map_err(sqlite_error)?
    }
}
