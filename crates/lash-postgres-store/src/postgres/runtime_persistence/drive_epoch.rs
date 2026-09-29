//! [`DriveEpochStore`] for [`PostgresStore`]: the storage half of the
//! admission seal that raises a session's drive epoch, and the drive-fence
//! check every fenced commit runs in its own transaction (ADR 0105 §2).
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

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// The session's stored drive epoch and the admission that last raised it,
/// read inside the caller's transaction.
pub(crate) async fn drive_epoch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<StoredDriveEpoch, StoreError> {
    read_drive_epoch(tx, session_id, session_sql().meta.select_drive_epoch.sql()).await
}

/// The session's stored drive epoch, read by `statement`: the plain read or
/// its row-locked fork.
async fn read_drive_epoch(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
    statement: &str,
) -> Result<StoredDriveEpoch, StoreError> {
    let row = sqlx::query(statement)
        .bind(session_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?
        .ok_or_else(|| StoreError::DriveEpochUnavailable {
            session_id: session_id.clone(),
        })?;
    let epoch: i64 = row.try_get(0).map_err(store_sqlx_error)?;
    let admission: Option<String> = row.try_get(1).map_err(store_sqlx_error)?;
    let root_start: Option<String> = row.try_get(2).map_err(store_sqlx_error)?;
    let closing: Option<i64> = row.try_get(3).map_err(store_sqlx_error)?;
    Ok(StoredDriveEpoch {
        control_pending: row.try_get(4).map_err(store_sqlx_error)?,
        epoch: u64_from_sql("SessionMeta", "drive_epoch", epoch)?,
        admission: admission.map(AdmissionId::new),
        root_start: root_start.map(RootStartNonce::new),
        closing: closing
            .map(|intent| u64_from_sql("SessionMeta", "closing_intent", intent))
            .transpose()?
            .map(lash_core_execution::store::ControlIntentId::from_sequence),
    })
}

/// Refuse `fence` unless it is the session's current drive fence, read in the
/// caller's transaction and row-locked until it ends: a seal in flight is
/// waited for and its epoch read, and a later seal waits for the write this
/// check fences (FIG-4044).
pub(super) async fn require_fence_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    fence: &DriveFence,
) -> Result<(), StoreError> {
    let current = read_drive_epoch(
        tx,
        session_id,
        session_sql().meta_postgres.select_drive_epoch_locked.sql(),
    )
    .await?;
    require_current_drive_fence(session_id, fence, &current)
}

/// Refuse `fence` unless it is the session's current drive fence, for a read
/// that writes nothing and so holds no lock (FIG-3927 N4).
pub(super) async fn require_fence_conn(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
    fence: &DriveFence,
) -> Result<(), StoreError> {
    let current = read_drive_epoch(
        connection,
        session_id,
        session_sql().meta.select_drive_epoch.sql(),
    )
    .await?;
    require_current_drive_fence(session_id, fence, &current)
}

/// Refuse `commit` unless the fence it presents is current, in its own
/// transaction before anything is read or written: the drive fence of the
/// admission its root was sealed under, which a successor's seal makes stale
/// (ADR 0105 §2). A commit that settles ingress must present one
/// ([`RuntimeCommit::validate_ingress_settlement`]).
///
/// [`RuntimeCommit::validate_ingress_settlement`]: lash_core_execution::store::RuntimeCommit::validate_ingress_settlement
pub(super) async fn require_commit_fences_tx(
    tx: &mut PgTx<'_>,
    commit: &lash_core_execution::store::RuntimeCommit,
) -> Result<(), StoreError> {
    commit.validate_ingress_settlement()?;
    match commit.drive_fence.as_ref() {
        Some(fence) => require_fence_tx(tx, &commit.session_id, fence).await,
        None => Ok(()),
    }
}

impl PostgresStore {
    /// Open a seal transaction for `session_id`, refusing a deleted session.
    async fn begin_seal_tx<'c>(
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
}

#[async_trait::async_trait]
impl DriveEpochStore for PostgresStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &AdmissionId,
        observed_epoch: u64,
        root_start: &RootStartNonce,
    ) -> Result<DriveEpochSeal, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = self.begin_seal_tx(&mut connection, session_id).await?;
        let stored = drive_epoch_tx(&mut tx, session_id).await?;
        let seal = match decide_drive_epoch_seal(
            session_id,
            &stored,
            admission,
            observed_epoch,
            root_start,
        ) {
            DriveEpochSealDecision::Answer(seal) => seal,
            DriveEpochSealDecision::Raise { next } => {
                let changed = sqlx::query(session_sql().meta.seal_drive_epoch.sql())
                    .bind(session_id.as_str())
                    .bind(sql_counter_value("drive_epoch", observed_epoch)?)
                    .bind(sql_counter_value("drive_epoch", next)?)
                    .bind(admission.as_str())
                    .bind(root_start.as_str())
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
