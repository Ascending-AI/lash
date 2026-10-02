//! [`DriveEpochStore`] for [`SqliteStore`]: the storage half of the admission seal
//! that raises a session's drive epoch, and the drive-fence checks every
//! fenced commit runs in its own transaction (ADR 0105 §2).
//!
//! The drive epoch lives on the session's `session_meta` row; a presented
//! [`DriveFence`] must name it, read in the same transaction as the write it
//! fences.

use super::*;
use lash_core_execution::store::{
    AdmissionId, DriveEpochSeal, DriveEpochSealDecision, DriveEpochStore, DriveFence, RootHold,
    RootStartNonce, StoredDriveEpoch, decide_drive_epoch_seal, decide_root_hold,
    require_current_drive_fence,
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
    StoredDriveEpoch::from_stored(
        u64::try_from(row.0)
            .map_err(|_| stored_data_corrupt("SessionMeta", "drive_epoch must be non-negative"))?,
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
    )
}

/// Refuse `fence` unless it is the session's current drive fence, read in the
/// caller's transaction.
pub(super) fn require_fence_conn(
    conn: &Connection,
    session_id: &SessionId,
    fence: &DriveFence,
) -> Result<(), StoreError> {
    let current = drive_epoch_conn(conn, session_id)?;
    require_current_drive_fence(session_id, fence, &current)
}

/// Whether a successor's seal superseded the drive fence `commit` presents:
/// the fence of the admission its root was sealed under (ADR 0105 §2). A
/// superseded fence writes nothing. The caller answers a commit it already
/// stored from its receipt and refuses every other one with the returned
/// [`StoreError::StaleDriveFence`]: a drive that runs several roots in one
/// journal replays an earlier root's commit after a later root's seal
/// (FIG-4498). A commit that settles ingress must present a fence
/// ([`RuntimeCommit::validate_ingress_settlement`]).
///
/// [`RuntimeCommit::validate_ingress_settlement`]: lash_core_execution::store::RuntimeCommit::validate_ingress_settlement
pub(super) fn commit_fence_superseded_conn(
    conn: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
) -> Result<Option<StoreError>, StoreError> {
    commit.validate_ingress_settlement()?;
    let Some(fence) = commit.drive_fence.as_ref() else {
        return Ok(None);
    };
    match require_fence_conn(conn, &commit.session_id, fence) {
        Ok(()) => Ok(None),
        Err(superseded @ StoreError::StaleDriveFence { .. }) => Ok(Some(superseded)),
        Err(error) => Err(error),
    }
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

#[async_trait::async_trait]
impl DriveEpochStore for SqliteStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
        hold: Option<&RootHold>,
    ) -> Result<DriveEpochSeal, StoreError> {
        let session_id = session_id.clone();
        let admission = admission.clone();
        let root_start = root_start.clone();
        let hold = hold.cloned();
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
                            // The recorded executor of the root keeps its
                            // fence (FIG-4814).
                            if let Some(hold) = &hold
                                && let Some(refused) = decide_root_hold(
                                    hold,
                                    stored.epoch,
                                    crate::session_roots::held_root_conn(
                                        tx,
                                        &session_id,
                                        &hold.root,
                                    )?
                                    .as_ref(),
                                    crate::session_roots::unfinished_root_conn(tx, &session_id)?
                                        .as_ref(),
                                )
                            {
                                return Ok(refused);
                            }
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
                            if let Some(hold) = &hold {
                                crate::session_roots::record_root_hold_conn(tx, &session_id, hold)?;
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
