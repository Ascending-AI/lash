//! PostgreSQL capture staging and stopped partials.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_execution::store::{
    CaptureAck, CaptureAttemptReset, CaptureBaseAdvance, CaptureBatch, CaptureFrame,
    CaptureFrameKey, CaptureInvocationKey, CaptureWriterLease, CaptureWriterLeaseRef,
    OpenCaptureWriter, SealTurnCapture, SealedCapture, StoppedPartialRead,
    StoppedPartialReadRequest, TurnCaptureStore, reduce_capture,
};
use lash_sansio::{CaptureBase, CaptureCoverage, SessionId, StoppedPartial, TurnId};
use lash_store_sql::session::{
    capture_frames::CaptureFrameStatements, capture_turns::CaptureTurnStatements,
    capture_writers::CaptureWriterStatements, stopped_partials::StoppedPartialStatements,
};
use sqlx::{Acquire, PgConnection, Postgres, Row, Transaction};

use crate::begin_guarded;
use crate::{
    PostgresStore, RuntimeCommit, StoreError, acquire_runtime_connection, store_sqlx_error,
};

fn stored_data_corrupt(record_kind: &'static str, error: impl std::fmt::Display) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind,
        message: error.to_string(),
    }
}

lash_store_sql::statements! {
    /// Capture statements only PostgreSQL issues.
    struct CapturePostgresStatements @ "capture_commit" {
        /// Clear physical turn `?2`'s staged capture (frames, writers and its
        /// counters) and read its sealed partial, if any, in one round trip.
        /// A data-modifying `WITH` runs whether or not the outer query reads
        /// it, so the head commit pays one round trip for the capture, not
        /// four. The commit's transaction rolls the clear back when the
        /// partial disagrees with the commit.
        select_partial_clearing_turn = "WITH cleared_frames AS (
                 DELETE FROM turn_capture_frames WHERE session_id = ?1 AND turn_id = ?2
             ), cleared_writers AS (
                 DELETE FROM turn_capture_writers WHERE session_id = ?1 AND turn_id = ?2
             ), cleared_turn AS (
                 DELETE FROM turn_capture_turns WHERE session_id = ?1 AND turn_id = ?2
             )
             SELECT partial_json FROM stopped_partials
                 WHERE session_id = ?1 AND turn_id = ?2";
    }
}

struct CaptureSql {
    turns: CaptureTurnStatements,
    writers: CaptureWriterStatements,
    frames: CaptureFrameStatements,
    partials: StoppedPartialStatements,
    commit: CapturePostgresStatements,
}

static SQL: LazyLock<CaptureSql> = LazyLock::new(|| {
    let dialect = lash_store_sql::Dialect::postgres();
    CaptureSql {
        turns: CaptureTurnStatements::render(dialect),
        writers: CaptureWriterStatements::render(dialect),
        frames: CaptureFrameStatements::render(dialect),
        partials: StoppedPartialStatements::render(dialect),
        commit: CapturePostgresStatements::render(dialect),
    }
});

fn number(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend("capture counter exceeds PostgreSQL range".into()))
}

fn unsigned(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt("TurnCapture", "negative counter"))
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|error| stored_data_corrupt("TurnCapture", error))
}

type TurnRow = (String, i64, i64, i64);

async fn turn_row(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    turn: &TurnId,
) -> Result<Option<TurnRow>, StoreError> {
    sqlx::query_as(SQL.turns.select.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

/// A later root adopted this physical turn: an owed follow-on's recovery
/// runs as a root of its own (FIG-3946), on a fresh journal. The earlier
/// root's staging belonged to the execution that was lost, so it goes with
/// its writers, the base restarts with the adopting root's checkpoints, and
/// the turn reads recovered.
async fn adopt_turn(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    turn: &TurnId,
    root: &TurnId,
) -> Result<TurnRow, StoreError> {
    for statement in [SQL.frames.delete_turn.sql(), SQL.writers.delete_turn.sql()] {
        sqlx::query(statement)
            .bind(session.as_str())
            .bind(turn.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    sqlx::query(SQL.turns.adopt.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind(root.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    turn_row(tx, session, turn)
        .await?
        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))
}

async fn partial_row(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    turn: &TurnId,
) -> Result<Option<(StoppedPartial, Option<i64>)>, StoreError> {
    let row = sqlx::query(SQL.partials.select_turn.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    row.map(|row| {
        let json: String = row.try_get(6).map_err(store_sqlx_error)?;
        let committed: Option<i64> = row.try_get(7).map_err(store_sqlx_error)?;
        decode(&json).map(|partial| (partial, committed))
    })
    .transpose()
}

async fn reject_sealed(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    turn: &TurnId,
) -> Result<(), StoreError> {
    if let Some((partial, _)) = partial_row(tx, session, turn).await? {
        return Err(StoreError::CaptureSealed {
            session_id: session.clone(),
            turn_id: turn.clone(),
            sealed_through: partial.id.sealed_through,
        });
    }
    Ok(())
}

async fn latest_epoch(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    turn: &TurnId,
    invocation: &str,
) -> Result<Option<(u32, String)>, StoreError> {
    let row: Option<(i64, String)> = sqlx::query_as(SQL.writers.select_latest.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind(invocation)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    row.map(|(epoch, state)| {
        Ok((
            u32::try_from(unsigned(epoch)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
            state,
        ))
    })
    .transpose()
}

async fn require_writer(
    tx: &mut Transaction<'_, Postgres>,
    lease: &CaptureWriterLeaseRef,
) -> Result<(), StoreError> {
    let session = &lease.turn.session_id;
    let turn = &lease.turn.turn_id;
    let latest = latest_epoch(tx, session, turn, lease.invocation.as_str()).await?;
    let current_epoch = latest.as_ref().map_or(0, |(epoch, _)| *epoch);
    if latest
        .as_ref()
        .is_none_or(|(epoch, state)| *epoch != lease.attempt_epoch || state != "live")
    {
        return Err(StoreError::CaptureWriterFenced {
            session_id: session.clone(),
            turn_id: turn.clone(),
            invocation: lease.invocation.0.clone(),
            attempt_epoch: lease.attempt_epoch,
            current_epoch,
        });
    }
    let Some((_, base, _, _)) = turn_row(tx, session, turn).await? else {
        return Err(StoreError::CaptureWriterFenced {
            session_id: session.clone(),
            turn_id: turn.clone(),
            invocation: lease.invocation.0.clone(),
            attempt_epoch: lease.attempt_epoch,
            current_epoch,
        });
    };
    if unsigned(base)? != u64::from(lease.base.0) {
        return Err(StoreError::CaptureBaseStale {
            session_id: session.clone(),
            turn_id: turn.clone(),
            offered: lease.base.0,
            current: u32::try_from(unsigned(base)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
        });
    }
    Ok(())
}

async fn inherited(
    tx: &mut Transaction<'_, Postgres>,
    lease: &CaptureWriterLeaseRef,
) -> Result<Vec<(CaptureFrameKey, CaptureFrame)>, StoreError> {
    let rows = sqlx::query(SQL.frames.select_inherited.sql())
        .bind(lease.turn.session_id.as_str())
        .bind(lease.turn.turn_id.as_str())
        .bind(lease.invocation.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut result = Vec::new();
    for row in rows {
        let sequence: i64 = row.try_get(0).map_err(store_sqlx_error)?;
        let base: i64 = row.try_get(1).map_err(store_sqlx_error)?;
        let epoch: i64 = row.try_get(2).map_err(store_sqlx_error)?;
        let json: String = row.try_get(3).map_err(store_sqlx_error)?;
        if unsigned(base)? != u64::from(lease.base.0) {
            continue;
        }
        let state: Option<String> = sqlx::query_scalar(SQL.writers.select_epoch.sql())
            .bind(lease.turn.session_id.as_str())
            .bind(lease.turn.turn_id.as_str())
            .bind(lease.invocation.as_str())
            .bind(epoch)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if state.as_deref() == Some("retracted") {
            continue;
        }
        result.push((
            CaptureFrameKey {
                turn: lease.turn.clone(),
                base: lease.base,
                invocation: lease.invocation.clone(),
                attempt_epoch: u32::try_from(unsigned(epoch)?)
                    .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
                sequence: unsigned(sequence)?,
            },
            decode(&json)?,
        ));
    }
    Ok(result)
}

async fn lease_at(
    tx: &mut Transaction<'_, Postgres>,
    turn: &lash_core_execution::facade_support::TurnAddress,
    invocation: &CaptureInvocationKey,
    epoch: u32,
    base: CaptureBase,
) -> Result<CaptureWriterLease, StoreError> {
    let reference = CaptureWriterLeaseRef {
        turn: turn.clone(),
        invocation: invocation.clone(),
        attempt_epoch: epoch,
        base,
    };
    Ok(CaptureWriterLease {
        turn: turn.clone(),
        invocation: invocation.clone(),
        attempt_epoch: epoch,
        base,
        inherited: inherited(tx, &reference).await?,
    })
}

pub(crate) async fn commit_capture_tx(
    tx: &mut Transaction<'_, Postgres>,
    commit: &RuntimeCommit,
    now: u64,
) -> Result<(), StoreError> {
    let Some(turn) = commit.turn_commit.operation.turn_id() else {
        return Ok(());
    };
    let session = &commit.session_id;
    let partial = sqlx::query_scalar::<_, String>(SQL.commit.select_partial_clearing_turn.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .map(|json| decode::<StoppedPartial>(&json))
        .transpose()?;
    match (&commit.stopped_partial, partial) {
        (None, None) => Ok(()),
        (None, Some(partial)) => Err(StoreError::StoppedPartialConflict {
            session_id: session.clone(),
            turn_id: turn.clone(),
            existing: Box::new(partial.digest),
            offered: Box::new(partial.digest),
        }),
        (Some(reference), partial) => {
            let wrong_turn = reference.id.session_id != *session || reference.id.turn_id != *turn;
            let Some(partial) = partial.filter(|_| !wrong_turn) else {
                return Err(StoreError::StoppedPartialNotSealed {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                });
            };
            if partial.id != reference.id || partial.digest != reference.digest {
                return Err(StoreError::StoppedPartialConflict {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                    existing: Box::new(partial.digest),
                    offered: Box::new(reference.digest),
                });
            }
            sqlx::query(SQL.partials.commit.sql())
                .bind(session.as_str())
                .bind(turn.as_str())
                .bind(number(now)?)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            Ok(())
        }
    }
}

pub(crate) async fn delete_session_capture_tx(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
) -> Result<usize, StoreError> {
    let removed = sqlx::query(SQL.frames.delete_session.sql())
        .bind(session.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected() as usize;
    sqlx::query(SQL.writers.delete_session.sql())
        .bind(session.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(SQL.turns.delete_session.sql())
        .bind(session.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(removed)
}

pub(crate) fn retention_delete_sql() -> &'static str {
    SQL.partials.delete_retained.sql()
}

/// Fence, seal and materialize one turn's capture. `worker_lost` names a
/// seal the lost-root write makes: the worker that wrote the capture is gone,
/// so the partial carries recovery evidence and promises only the prefix it
/// acknowledged (ADR 0114 §1.3, §4.4), whatever the turn's own row recorded.
async fn seal_capture_tx(
    tx: &mut Transaction<'_, Postgres>,
    request: &SealTurnCapture,
    now: u64,
    worker_lost: bool,
) -> Result<SealedCapture, StoreError> {
    let session = &request.turn.session_id;
    let turn = &request.turn.turn_id;
    crate::runtime_persistence::ensure_session_not_deleted_tx(tx, session).await?;
    if let Some(fence) = &request.drive_fence {
        crate::runtime_persistence::drive_epoch::require_fence_tx(tx, session, fence).await?;
    }
    if let Some((partial, committed)) = partial_row(tx, session, turn).await? {
        return Ok(if committed.is_some() {
            SealedCapture::Committed(partial)
        } else {
            SealedCapture::Sealed(partial)
        });
    }
    sqlx::query(SQL.turns.insert.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind(request.root.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut row = turn_row(tx, session, turn)
        .await?
        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
    if row.0 != request.root.as_str() {
        row = adopt_turn(tx, session, turn, &request.root).await?;
    }
    let (_, base, next, recovered) = row;
    let through = unsigned(next)?.saturating_sub(1);
    if let Some(recorded) = request.recorded_watermark
        && through < recorded
    {
        return Err(StoreError::CaptureSealBelowWatermark {
            session_id: session.clone(),
            turn_id: turn.clone(),
            sealed_through: through,
            recorded,
        });
    }
    sqlx::query(SQL.writers.fence_turn.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let rows = sqlx::query(SQL.frames.select_all.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut frames = Vec::new();
    for row in rows {
        let sequence: i64 = row.try_get(0).map_err(store_sqlx_error)?;
        let frame_base: i64 = row.try_get(1).map_err(store_sqlx_error)?;
        let invocation: String = row.try_get(2).map_err(store_sqlx_error)?;
        let epoch: i64 = row.try_get(3).map_err(store_sqlx_error)?;
        let json: String = row.try_get(4).map_err(store_sqlx_error)?;
        frames.push((
            CaptureFrameKey {
                turn: request.turn.clone(),
                base: CaptureBase(
                    u32::try_from(unsigned(frame_base)?)
                        .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
                ),
                invocation: CaptureInvocationKey(invocation),
                attempt_epoch: u32::try_from(unsigned(epoch)?)
                    .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
                sequence: unsigned(sequence)?,
            },
            decode(&json)?,
        ));
    }
    let rows: Vec<(String, i64)> = sqlx::query_as(SQL.writers.select_retracted.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut retracted = BTreeSet::new();
    for (key, epoch) in rows {
        retracted.insert((
            CaptureInvocationKey(key),
            u32::try_from(unsigned(epoch)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
        ));
    }
    let recovered = recovered != 0 || worker_lost;
    let id = lash_sansio::StoppedPartialId {
        session_id: session.clone(),
        root: request.root.clone(),
        turn_id: turn.clone(),
        base: CaptureBase(
            u32::try_from(unsigned(base)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
        ),
        sealed_through: through,
    };
    let coverage = if recovered {
        CaptureCoverage::AcknowledgedPrefix
    } else {
        CaptureCoverage::Complete
    };
    let partial = reduce_capture(
        id,
        request.reason.clone(),
        recovered,
        coverage,
        &frames,
        &retracted,
    )
    .map_err(|violation| StoreError::CaptureCorrupt {
        session_id: session.clone(),
        turn_id: turn.clone(),
        violation,
    })?;
    let json = crate::encode_json(&partial)?;
    sqlx::query(SQL.partials.insert.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind(request.root.as_str())
        .bind(base)
        .bind(number(through)?)
        .bind(crate::encode_json(&request.reason)?)
        .bind(i64::from(recovered))
        .bind(partial.digest.to_hex())
        .bind(&json)
        .bind(number(json.len() as u64)?)
        .bind(number(now)?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(SealedCapture::Sealed(partial))
}

pub(crate) async fn committed_root_summary_conn(
    conn: &mut PgConnection,
    session: &SessionId,
    root: &TurnId,
) -> Result<Option<lash_sansio::StoppedPartialSummary>, StoreError> {
    let json: Option<String> = sqlx::query_scalar(SQL.partials.select_committed_by_root.sql())
        .bind(session.as_str())
        .bind(root.as_str())
        .fetch_optional(conn)
        .await
        .map_err(store_sqlx_error)?;
    json.map(|json| decode::<StoppedPartial>(&json).map(|partial| partial.summary()))
        .transpose()
}

pub(crate) async fn seal_root_terminal_capture_tx(
    tx: &mut Transaction<'_, Postgres>,
    session: &SessionId,
    root: &TurnId,
    reason: lash_sansio::StopReason,
    now: u64,
    worker_lost: bool,
) -> Result<lash_sansio::StoppedPartialSummary, StoreError> {
    if let Some(summary) = committed_root_summary_conn(&mut *tx, session, root).await? {
        return Ok(summary);
    }
    let turns: Vec<String> = sqlx::query_scalar(SQL.turns.select_by_root.sql())
        .bind(session.as_str())
        .bind(root.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if turns.len() > 1 {
        return Err(stored_data_corrupt(
            "TurnCapture",
            "multiple active physical turns for one root",
        ));
    }
    let turn = turns
        .first()
        .map_or_else(|| root.clone(), |id| TurnId::from(id.clone()));
    let request = SealTurnCapture {
        turn: lash_core_execution::facade_support::TurnAddress::new(session.clone(), turn.clone()),
        root: root.clone(),
        reason,
        recorded_watermark: None,
        drive_fence: None,
    };
    let partial = seal_capture_tx(tx, &request, now, worker_lost)
        .await?
        .into_partial();
    sqlx::query(SQL.partials.commit.sql())
        .bind(session.as_str())
        .bind(turn.as_str())
        .bind(number(now)?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for statement in [
        SQL.frames.delete_turn.sql(),
        SQL.writers.delete_turn.sql(),
        SQL.turns.delete.sql(),
    ] {
        sqlx::query(statement)
            .bind(session.as_str())
            .bind(turn.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(partial.summary())
}

#[async_trait::async_trait]
impl TurnCaptureStore for PostgresStore {
    async fn open_capture_writer(
        &self,
        request: &OpenCaptureWriter,
    ) -> Result<CaptureWriterLease, StoreError> {
        lash_core_execution::store::validate_session_id(&request.turn.session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let session = &request.turn.session_id;
        let turn = &request.turn.turn_id;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        reject_sealed(&mut tx, session, turn).await?;
        sqlx::query(SQL.turns.insert.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(request.root.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut row = turn_row(&mut tx, session, turn)
            .await?
            .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
        if row.0 != request.root.as_str() {
            row = adopt_turn(&mut tx, session, turn, &request.root).await?;
        }
        let (_, base, _, _) = row;
        let base = CaptureBase(
            u32::try_from(unsigned(base)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
        );
        let epoch = match latest_epoch(&mut tx, session, turn, request.invocation.as_str()).await? {
            Some((previous, _)) => {
                let old =
                    lease_at(&mut tx, &request.turn, &request.invocation, previous, base).await?;
                if !old.inherited.is_empty() {
                    sqlx::query(SQL.turns.mark_recovered.sql())
                        .bind(session.as_str())
                        .bind(turn.as_str())
                        .execute(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?;
                }
                sqlx::query(SQL.writers.fence_invocation.sql())
                    .bind(session.as_str())
                    .bind(turn.as_str())
                    .bind(request.invocation.as_str())
                    .execute(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?;
                previous
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Backend("capture epoch overflow".into()))?
            }
            None => 0,
        };
        sqlx::query(SQL.writers.insert.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(request.invocation.as_str())
            .bind(i64::from(epoch))
            .bind("live")
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let lease = lease_at(&mut tx, &request.turn, &request.invocation, epoch, base).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lease)
    }

    async fn append_capture_batch(&self, batch: &CaptureBatch) -> Result<CaptureAck, StoreError> {
        lash_core_execution::store::validate_session_id(&batch.lease.turn.session_id)?;
        batch.validate_bounds()?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let lease = &batch.lease;
        let session = &lease.turn.session_id;
        let turn = &lease.turn.turn_id;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        reject_sealed(&mut tx, session, turn).await?;
        require_writer(&mut tx, lease).await?;
        let encoded = batch
            .frames
            .iter()
            .map(crate::encode_json)
            .collect::<Result<Vec<_>, _>>()?;
        let existing: Vec<(i64, String)> = sqlx::query_as(SQL.frames.select_batch.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(lease.invocation.as_str())
            .bind(i64::from(lease.attempt_epoch))
            .bind(number(batch.batch_ordinal)?)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if !existing.is_empty() {
            if existing.iter().map(|(_, json)| json).ne(encoded.iter()) {
                return Err(StoreError::CaptureBatchConflict {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                    invocation: lease.invocation.0.clone(),
                    attempt_epoch: lease.attempt_epoch,
                    batch_ordinal: batch.batch_ordinal,
                });
            }
            return Ok(CaptureAck {
                first_sequence: unsigned(
                    existing
                        .first()
                        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing batch head"))?
                        .0,
                )?,
                last_sequence: unsigned(
                    existing
                        .last()
                        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing batch tail"))?
                        .0,
                )?,
            });
        }
        let (_, _, next, _) = turn_row(&mut tx, session, turn)
            .await?
            .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
        let first = unsigned(next)?;
        for (index, json) in encoded.iter().enumerate() {
            sqlx::query(SQL.frames.insert.sql())
                .bind(session.as_str())
                .bind(turn.as_str())
                .bind(number(first + index as u64)?)
                .bind(i64::from(lease.base.0))
                .bind(lease.invocation.as_str())
                .bind(i64::from(lease.attempt_epoch))
                .bind(number(batch.batch_ordinal)?)
                .bind(json)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        let last = first.saturating_add(encoded.len() as u64).saturating_sub(1);
        sqlx::query(SQL.turns.set_next_sequence.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(number(first + encoded.len() as u64)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(CaptureAck {
            first_sequence: first,
            last_sequence: last,
        })
    }

    async fn persist_attempt_reset(
        &self,
        reset: &CaptureAttemptReset,
    ) -> Result<CaptureWriterLease, StoreError> {
        lash_core_execution::store::validate_session_id(&reset.lease.turn.session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let lease = &reset.lease;
        let session = &lease.turn.session_id;
        let turn = &lease.turn.turn_id;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        reject_sealed(&mut tx, session, turn).await?;
        let latest = latest_epoch(&mut tx, session, turn, lease.invocation.as_str()).await?;
        if let Some((latest_epoch, latest_state)) = latest.as_ref()
            && *latest_epoch > lease.attempt_epoch
            && latest_state == "live"
        {
            let prior: Option<String> = sqlx::query_scalar(SQL.writers.select_epoch.sql())
                .bind(session.as_str())
                .bind(turn.as_str())
                .bind(lease.invocation.as_str())
                .bind(i64::from(lease.attempt_epoch))
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            if matches!(prior.as_deref(), Some("fenced" | "retracted")) {
                let (_, base, _, _) = turn_row(&mut tx, session, turn)
                    .await?
                    .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
                if unsigned(base)? != u64::from(lease.base.0) {
                    return Err(StoreError::CaptureBaseStale {
                        session_id: session.clone(),
                        turn_id: turn.clone(),
                        offered: lease.base.0,
                        current: u32::try_from(unsigned(base)?)
                            .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
                    });
                }
                if prior.as_deref() == Some("fenced") {
                    sqlx::query(SQL.writers.retract_fenced.sql())
                        .bind(session.as_str())
                        .bind(turn.as_str())
                        .bind(lease.invocation.as_str())
                        .bind(i64::from(lease.attempt_epoch))
                        .execute(&mut **tx)
                        .await
                        .map_err(store_sqlx_error)?;
                }
                let result = lease_at(
                    &mut tx,
                    &lease.turn,
                    &lease.invocation,
                    *latest_epoch,
                    lease.base,
                )
                .await?;
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(result);
            }
        }
        let next = lease
            .attempt_epoch
            .checked_add(1)
            .ok_or_else(|| StoreError::Backend("capture epoch overflow".into()))?;
        if latest.as_ref().is_some_and(|(epoch, _)| *epoch == next) {
            let prior: Option<String> = sqlx::query_scalar(SQL.writers.select_epoch.sql())
                .bind(session.as_str())
                .bind(turn.as_str())
                .bind(lease.invocation.as_str())
                .bind(i64::from(lease.attempt_epoch))
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            if prior.as_deref() == Some("retracted") {
                let result =
                    lease_at(&mut tx, &lease.turn, &lease.invocation, next, lease.base).await?;
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(result);
            }
        }
        require_writer(&mut tx, lease).await?;
        sqlx::query(SQL.writers.retract.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(lease.invocation.as_str())
            .bind(i64::from(lease.attempt_epoch))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(SQL.writers.insert.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(lease.invocation.as_str())
            .bind(i64::from(next))
            .bind("live")
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let result = lease_at(&mut tx, &lease.turn, &lease.invocation, next, lease.base).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }

    async fn advance_capture_base(&self, advance: &CaptureBaseAdvance) -> Result<(), StoreError> {
        lash_core_execution::store::validate_session_id(&advance.turn.session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let session = &advance.turn.session_id;
        let turn = &advance.turn.turn_id;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session).await?;
        reject_sealed(&mut tx, session, turn).await?;
        let (_, base, _, _) = turn_row(&mut tx, session, turn).await?.ok_or_else(|| {
            StoreError::CaptureBaseStale {
                session_id: session.clone(),
                turn_id: turn.clone(),
                offered: advance.to.0,
                current: 0,
            }
        })?;
        let current = u32::try_from(unsigned(base)?)
            .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?;
        if advance.to.0 == current {
            return Ok(());
        }
        if advance.to.0 != current.checked_add(1).unwrap_or(current) {
            return Err(StoreError::CaptureBaseStale {
                session_id: session.clone(),
                turn_id: turn.clone(),
                offered: advance.to.0,
                current,
            });
        }
        sqlx::query(SQL.turns.advance.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(i64::from(advance.to.0))
            .bind(base)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(SQL.frames.delete_before_base.sql())
            .bind(session.as_str())
            .bind(turn.as_str())
            .bind(i64::from(advance.to.0))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn seal_turn_capture(
        &self,
        request: &SealTurnCapture,
    ) -> Result<SealedCapture, StoreError> {
        lash_core_execution::store::validate_session_id(&request.turn.session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        let result = seal_capture_tx(&mut tx, request, self.clock.timestamp_ms(), false).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }

    async fn read_stopped_partial(
        &self,
        request: &StoppedPartialReadRequest,
    ) -> Result<StoppedPartialRead, StoreError> {
        lash_core_execution::store::validate_session_id(&request.session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, &request.session_id)
            .await?;
        let row = sqlx::query(SQL.partials.select_by_turn_or_root.sql())
            .bind(request.session_id.as_str())
            .bind(request.turn.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if let Some(row) = row {
            let json: String = row.try_get(0).map_err(store_sqlx_error)?;
            let committed: Option<i64> = row.try_get(1).map_err(store_sqlx_error)?;
            return Ok(if committed.is_some() {
                StoppedPartialRead::Available(decode(&json)?)
            } else {
                StoppedPartialRead::Pending
            });
        }
        if turn_row(&mut tx, &request.session_id, &request.turn)
            .await?
            .is_some()
        {
            return Ok(StoppedPartialRead::Pending);
        }
        let receipt_key =
            lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
                &request.session_id,
                &request.turn,
            )?;
        let committed: bool = sqlx::query_scalar(
            crate::session_sql::session_sql()
                .turn_commits
                .exists_for_turn
                .sql(),
        )
        .bind(request.session_id.as_str())
        .bind(receipt_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        Ok(if committed {
            StoppedPartialRead::NotStopped
        } else {
            StoppedPartialRead::Unknown
        })
    }
}
