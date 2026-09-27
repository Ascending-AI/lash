//! [`DriveEpochStore`] for [`Store`]: the storage half of the admission seal
//! that raises a session's drive epoch, and the drive-fence checks every
//! fenced commit runs in its own transaction (ADR 0105 §2).
//!
//! The drive epoch lives on the session's `session_meta` row; a presented
//! [`DriveFence`] must name it, read in the same transaction as the write it
//! fences.

use super::*;
use lash_core_execution::store::{
    AdmissionId, DriveEpochSeal, DriveEpochSealDecision, DriveEpochStore, DriveFence,
    RootStartNonce, StoredDriveEpoch, decide_drive_epoch_seal, require_current_drive_fence,
};
use lash_core_execution::store_backend_support::sealed_drive_fence;

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
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?
        .ok_or_else(|| StoreError::DriveEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    Ok(StoredDriveEpoch {
        control_pending: row.4,
        epoch: u64::try_from(row.0)
            .map_err(|_| stored_data_corrupt("SessionMeta", "drive_epoch must be non-negative"))?,
        admission: row.1.map(AdmissionId::new),
        root_start: row.2.map(RootStartNonce::new),
        closing: row
            .3
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

/// Refuse `commit` unless every fence it presents is current, in its own
/// transaction before anything is read or written: the execution lane it
/// borrows (a queued run's commit must borrow one), and the drive fence of
/// the admission its root was sealed under, which a successor's seal makes
/// stale (ADR 0105 §2).
pub(super) fn require_commit_fences_conn(
    conn: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
    now: u64,
) -> Result<(), StoreError> {
    if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
        super::claim_support::ensure_session_execution_lease_conn(
            conn,
            &commit.session_id,
            fence,
            now,
        )?;
    }
    if commit.queued_run.is_some() && commit.session_execution_lease_fence.is_none() {
        return Err(StoreError::SessionExecutionLeaseExpired {
            session_id: commit.session_id.clone(),
        });
    }
    match commit.drive_fence.as_ref() {
        Some(fence) => require_fence_conn(conn, &commit.session_id, fence),
        None => Ok(()),
    }
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

#[async_trait::async_trait]
impl DriveEpochStore for Store {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
    ) -> Result<DriveEpochSeal, StoreError> {
        let session_id = session_id.clone();
        let admission = admission.clone();
        let root_start = root_start.clone();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let stored = drive_epoch_conn(tx, &session_id)?;
                    match decide_drive_epoch_seal(
                        &session_id,
                        &stored,
                        &admission,
                        observed_epoch,
                        &root_start,
                    ) {
                        DriveEpochSealDecision::Answer(seal) => Ok(seal),
                        DriveEpochSealDecision::Raise { next } => {
                            let changed = tx
                                .execute(
                                    session_sql().meta.seal_drive_epoch.sql(),
                                    params![
                                        session_id.as_str(),
                                        sql_counter_value("drive_epoch", observed_epoch)?,
                                        sql_counter_value("drive_epoch", next)?,
                                        admission.as_str(),
                                        root_start.as_str()
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
