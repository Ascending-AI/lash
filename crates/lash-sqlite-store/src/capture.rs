//! SQLite capture staging, sealing and committed reads.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_execution::store::reduce_capture;
use lash_core_execution::store::{
    CaptureAck, CaptureAttemptReset, CaptureBaseAdvance, CaptureBatch, CaptureFrame,
    CaptureFrameKey, CaptureInvocationKey, CaptureWriterLease, CaptureWriterLeaseRef,
    OpenCaptureWriter, SealTurnCapture, SealedCapture, StoppedPartialRead,
    StoppedPartialReadRequest, TurnCaptureStore,
};
use lash_sansio::{CaptureBase, CaptureCoverage, SessionId, StoppedPartial, TurnId};
use lash_store_sql::session::{
    capture_frames::CaptureFrameStatements, capture_turns::CaptureTurnStatements,
    capture_writers::CaptureWriterStatements, stopped_partials::StoppedPartialStatements,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::conn::TxOutcome;
use crate::{Store, StoreError, sqlite_error, stored_data_corrupt};

struct CaptureSql {
    turns: CaptureTurnStatements,
    writers: CaptureWriterStatements,
    frames: CaptureFrameStatements,
    partials: StoppedPartialStatements,
}

static SQL: LazyLock<CaptureSql> = LazyLock::new(|| {
    let dialect = lash_store_sql::Dialect::sqlite_unqualified();
    CaptureSql {
        turns: CaptureTurnStatements::render(dialect),
        writers: CaptureWriterStatements::render(dialect),
        frames: CaptureFrameStatements::render(dialect),
        partials: StoppedPartialStatements::render(dialect),
    }
});

pub(crate) fn retention_delete_sql() -> &'static str {
    SQL.partials.delete_retained.sql()
}

fn number(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend("capture counter exceeds SQLite range".into()))
}

fn unsigned(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt("TurnCapture", "negative counter"))
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|error| stored_data_corrupt("TurnCapture", error))
}

type TurnRow = (String, i64, i64, i64);

fn turn_row(
    conn: &Connection,
    session: &SessionId,
    turn: &TurnId,
) -> Result<Option<TurnRow>, StoreError> {
    conn.query_row(
        SQL.turns.select.sql(),
        params![session.as_str(), turn.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .map_err(sqlite_error)
}

fn partial_row(
    conn: &Connection,
    session: &SessionId,
    turn: &TurnId,
) -> Result<Option<(StoppedPartial, Option<i64>)>, StoreError> {
    let row: Option<(String, Option<i64>)> = conn
        .query_row(
            SQL.partials.select_turn.sql(),
            params![session.as_str(), turn.as_str()],
            |row| Ok((row.get(6)?, row.get(7)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(|(json, committed)| decode(&json).map(|partial| (partial, committed)))
        .transpose()
}

fn reject_sealed(conn: &Connection, session: &SessionId, turn: &TurnId) -> Result<(), StoreError> {
    if let Some((partial, _)) = partial_row(conn, session, turn)? {
        return Err(StoreError::CaptureSealed {
            session_id: session.clone(),
            turn_id: turn.clone(),
            sealed_through: partial.id.sealed_through,
        });
    }
    Ok(())
}

fn latest_epoch(
    conn: &Connection,
    session: &SessionId,
    turn: &TurnId,
    invocation: &str,
) -> Result<Option<(u32, String)>, StoreError> {
    let row: Option<(i64, String)> = conn
        .query_row(
            SQL.writers.select_latest.sql(),
            params![session.as_str(), turn.as_str(), invocation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(|(epoch, state)| {
        Ok((
            u32::try_from(unsigned(epoch)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
            state,
        ))
    })
    .transpose()
}

fn require_writer(conn: &Connection, lease: &CaptureWriterLeaseRef) -> Result<(), StoreError> {
    let session = &lease.turn.session_id;
    let turn = &lease.turn.turn_id;
    let latest = latest_epoch(conn, session, turn, lease.invocation.as_str())?;
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
    let Some((_, base, _, _)) = turn_row(conn, session, turn)? else {
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

fn inherited(
    conn: &Connection,
    lease: &CaptureWriterLeaseRef,
) -> Result<Vec<(CaptureFrameKey, CaptureFrame)>, StoreError> {
    let mut stmt = conn
        .prepare_cached(SQL.frames.select_inherited.sql())
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(
            params![
                lease.turn.session_id.as_str(),
                lease.turn.turn_id.as_str(),
                lease.invocation.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(sqlite_error)?;
    let mut result = Vec::new();
    for row in rows {
        let (sequence, base, epoch, json) = row.map_err(sqlite_error)?;
        if unsigned(base)? != u64::from(lease.base.0) {
            continue;
        }
        let state: Option<String> = conn
            .query_row(
                SQL.writers.select_epoch.sql(),
                params![
                    lease.turn.session_id.as_str(),
                    lease.turn.turn_id.as_str(),
                    lease.invocation.as_str(),
                    epoch
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
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

fn lease_at(
    conn: &Connection,
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
        inherited: inherited(conn, &reference)?,
    })
}

pub(crate) fn commit_capture_conn(
    conn: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
    now: u64,
) -> Result<(), StoreError> {
    let Some(turn) = commit.turn_commit.operation.turn_id() else {
        return Ok(());
    };
    let session = &commit.session_id;
    if let Some(reference) = &commit.stopped_partial {
        if reference.id.session_id != *session || reference.id.turn_id != *turn {
            return Err(StoreError::StoppedPartialNotSealed {
                session_id: session.clone(),
                turn_id: turn.clone(),
            });
        }
        let Some((partial, _)) = partial_row(conn, session, turn)? else {
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
        conn.execute(
            SQL.partials.commit.sql(),
            params![session.as_str(), turn.as_str(), number(now)?],
        )
        .map_err(sqlite_error)?;
    } else if let Some((partial, _)) = partial_row(conn, session, turn)? {
        return Err(StoreError::StoppedPartialConflict {
            session_id: session.clone(),
            turn_id: turn.clone(),
            existing: Box::new(partial.digest),
            offered: Box::new(partial.digest),
        });
    }
    conn.execute(
        SQL.frames.delete_turn.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    conn.execute(
        SQL.writers.delete_turn.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    conn.execute(
        SQL.turns.delete.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

pub(crate) fn delete_session_capture_conn(
    conn: &Connection,
    session: &SessionId,
) -> Result<usize, StoreError> {
    let removed = conn
        .execute(SQL.frames.delete_session.sql(), params![session.as_str()])
        .map_err(sqlite_error)?;
    for sql in [
        SQL.writers.delete_session.sql(),
        SQL.turns.delete_session.sql(),
    ] {
        conn.execute(sql, params![session.as_str()])
            .map_err(sqlite_error)?;
    }
    Ok(removed)
}

/// Fence, seal and materialize one turn's capture. `worker_lost` names a
/// seal the lost-root write makes: the worker that wrote the capture is gone,
/// so the partial carries recovery evidence and promises only the prefix it
/// acknowledged (ADR 0114 §1.3, §4.4), whatever the turn's own row recorded.
pub(crate) fn seal_capture_conn(
    conn: &Connection,
    request: &SealTurnCapture,
    now: u64,
    worker_lost: bool,
) -> Result<SealedCapture, StoreError> {
    let session = &request.turn.session_id;
    let turn = &request.turn.turn_id;
    crate::persistence::ensure_session_not_deleted_conn(conn, session)?;
    if let Some((partial, committed)) = partial_row(conn, session, turn)? {
        return Ok(if committed.is_some() {
            SealedCapture::Committed(partial)
        } else {
            SealedCapture::Sealed(partial)
        });
    }
    conn.execute(
        SQL.turns.insert.sql(),
        params![session.as_str(), turn.as_str(), request.root.as_str()],
    )
    .map_err(sqlite_error)?;
    let (root, base, next, recovered) = turn_row(conn, session, turn)?
        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
    if root != request.root.as_str() {
        return Err(stored_data_corrupt("TurnCapture", "root mismatch"));
    }
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
    conn.execute(
        SQL.writers.fence_turn.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    let mut frame_stmt = conn
        .prepare_cached(SQL.frames.select_all.sql())
        .map_err(sqlite_error)?;
    let rows = frame_stmt
        .query_map(params![session.as_str(), turn.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut frames = Vec::new();
    for row in rows {
        let (sequence, frame_base, invocation, epoch, json) = row.map_err(sqlite_error)?;
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
    drop(frame_stmt);
    let mut writer_stmt = conn
        .prepare_cached(SQL.writers.select_retracted.sql())
        .map_err(sqlite_error)?;
    let rows = writer_stmt
        .query_map(params![session.as_str(), turn.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(sqlite_error)?;
    let mut retracted = BTreeSet::new();
    for row in rows {
        let (key, epoch) = row.map_err(sqlite_error)?;
        retracted.insert((
            CaptureInvocationKey(key),
            u32::try_from(unsigned(epoch)?)
                .map_err(|_| stored_data_corrupt("TurnCapture", "epoch overflow"))?,
        ));
    }
    drop(writer_stmt);
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
    conn.execute(
        SQL.partials.insert.sql(),
        params![
            session.as_str(),
            turn.as_str(),
            request.root.as_str(),
            base,
            number(through)?,
            crate::encode_json(&request.reason)?,
            i64::from(recovered),
            partial.digest.to_hex(),
            json,
            number(json.len() as u64)?,
            number(now)?
        ],
    )
    .map_err(sqlite_error)?;
    Ok(SealedCapture::Sealed(partial))
}

pub(crate) fn committed_root_summary_conn(
    conn: &Connection,
    session: &SessionId,
    root: &TurnId,
) -> Result<Option<lash_sansio::StoppedPartialSummary>, StoreError> {
    let json: Option<String> = conn
        .query_row(
            SQL.partials.select_committed_by_root.sql(),
            params![session.as_str(), root.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.map(|json| decode::<StoppedPartial>(&json).map(|partial| partial.summary()))
        .transpose()
}

pub(crate) fn seal_root_terminal_capture_conn(
    conn: &Connection,
    session: &SessionId,
    root: &TurnId,
    reason: lash_sansio::StopReason,
    now: u64,
    worker_lost: bool,
) -> Result<lash_sansio::StoppedPartialSummary, StoreError> {
    if let Some(summary) = committed_root_summary_conn(conn, session, root)? {
        return Ok(summary);
    }
    let mut stmt = conn
        .prepare_cached(SQL.turns.select_by_root.sql())
        .map_err(sqlite_error)?;
    let turns = stmt
        .query_map(params![session.as_str(), root.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    drop(stmt);
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
    };
    let partial = seal_capture_conn(conn, &request, now, worker_lost)?.into_partial();
    conn.execute(
        SQL.partials.commit.sql(),
        params![session.as_str(), turn.as_str(), number(now)?],
    )
    .map_err(sqlite_error)?;
    conn.execute(
        SQL.frames.delete_turn.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    conn.execute(
        SQL.writers.delete_turn.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    conn.execute(
        SQL.turns.delete.sql(),
        params![session.as_str(), turn.as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(partial.summary())
}

#[async_trait::async_trait]
impl TurnCaptureStore for Store {
    async fn open_capture_writer(
        &self,
        request: &OpenCaptureWriter,
    ) -> Result<CaptureWriterLease, StoreError> {
        self.bind_session(&request.turn.session_id)?;
        let request = request.clone();
        self.conn
            .write_flow(move |tx| {
                let result = (|| {
                    let session = &request.turn.session_id;
                    let turn = &request.turn.turn_id;
                    crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    reject_sealed(tx, session, turn)?;
                    tx.execute(
                        SQL.turns.insert.sql(),
                        params![session.as_str(), turn.as_str(), request.root.as_str()],
                    )
                    .map_err(sqlite_error)?;
                    let (root, base, _, _) = turn_row(tx, session, turn)?
                        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
                    if root != request.root.as_str() {
                        return Err(stored_data_corrupt("TurnCapture", "root mismatch"));
                    }
                    let epoch = match latest_epoch(tx, session, turn, request.invocation.as_str())?
                    {
                        Some((previous, _)) => {
                            let old = lease_at(
                                tx,
                                &request.turn,
                                &request.invocation,
                                previous,
                                CaptureBase(u32::try_from(unsigned(base)?).map_err(|_| {
                                    stored_data_corrupt("TurnCapture", "base overflow")
                                })?),
                            )?;
                            if !old.inherited.is_empty() {
                                tx.execute(
                                    SQL.turns.mark_recovered.sql(),
                                    params![session.as_str(), turn.as_str()],
                                )
                                .map_err(sqlite_error)?;
                            }
                            tx.execute(
                                SQL.writers.fence_invocation.sql(),
                                params![
                                    session.as_str(),
                                    turn.as_str(),
                                    request.invocation.as_str()
                                ],
                            )
                            .map_err(sqlite_error)?;
                            previous.checked_add(1).ok_or_else(|| {
                                StoreError::Backend("capture epoch overflow".into())
                            })?
                        }
                        None => 0,
                    };
                    tx.execute(
                        SQL.writers.insert.sql(),
                        params![
                            session.as_str(),
                            turn.as_str(),
                            request.invocation.as_str(),
                            i64::from(epoch),
                            "live"
                        ],
                    )
                    .map_err(sqlite_error)?;
                    lease_at(
                        tx,
                        &request.turn,
                        &request.invocation,
                        epoch,
                        CaptureBase(
                            u32::try_from(unsigned(base)?)
                                .map_err(|_| stored_data_corrupt("TurnCapture", "base overflow"))?,
                        ),
                    )
                })();
                Ok(match result {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn append_capture_batch(&self, batch: &CaptureBatch) -> Result<CaptureAck, StoreError> {
        self.bind_session(&batch.lease.turn.session_id)?;
        batch.validate_bounds()?;
        let batch = batch.clone();
        self.conn
            .write_flow(move |tx| {
                let result = (|| {
                    let lease = &batch.lease;
                    let session = &lease.turn.session_id;
                    let turn = &lease.turn.turn_id;
                    crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    reject_sealed(tx, session, turn)?;
                    require_writer(tx, lease)?;
                    let encoded = batch
                        .frames
                        .iter()
                        .map(crate::encode_json)
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut stmt = tx
                        .prepare_cached(SQL.frames.select_batch.sql())
                        .map_err(sqlite_error)?;
                    let existing = stmt
                        .query_map(
                            params![
                                session.as_str(),
                                turn.as_str(),
                                lease.invocation.as_str(),
                                i64::from(lease.attempt_epoch),
                                number(batch.batch_ordinal)?
                            ],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                        )
                        .map_err(sqlite_error)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sqlite_error)?;
                    drop(stmt);
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
                                    .ok_or_else(|| {
                                        stored_data_corrupt("TurnCapture", "missing batch head")
                                    })?
                                    .0,
                            )?,
                            last_sequence: unsigned(
                                existing
                                    .last()
                                    .ok_or_else(|| {
                                        stored_data_corrupt("TurnCapture", "missing batch tail")
                                    })?
                                    .0,
                            )?,
                        });
                    }
                    let (_, _, next, _) = turn_row(tx, session, turn)?
                        .ok_or_else(|| stored_data_corrupt("TurnCapture", "missing turn row"))?;
                    let first = unsigned(next)?;
                    for (index, json) in encoded.iter().enumerate() {
                        tx.execute(
                            SQL.frames.insert.sql(),
                            params![
                                session.as_str(),
                                turn.as_str(),
                                number(first + index as u64)?,
                                i64::from(lease.base.0),
                                lease.invocation.as_str(),
                                i64::from(lease.attempt_epoch),
                                number(batch.batch_ordinal)?,
                                json
                            ],
                        )
                        .map_err(sqlite_error)?;
                    }
                    let last = first.saturating_add(encoded.len() as u64).saturating_sub(1);
                    tx.execute(
                        SQL.turns.set_next_sequence.sql(),
                        params![
                            session.as_str(),
                            turn.as_str(),
                            number(first + encoded.len() as u64)?
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(CaptureAck {
                        first_sequence: first,
                        last_sequence: last,
                    })
                })();
                Ok(match result {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn persist_attempt_reset(
        &self,
        reset: &CaptureAttemptReset,
    ) -> Result<CaptureWriterLease, StoreError> {
        self.bind_session(&reset.lease.turn.session_id)?;
        let reset = reset.clone();
        self.conn
            .write_flow(move |tx| {
                let result = (|| {
                    let lease = &reset.lease;
                    let session = &lease.turn.session_id;
                    let turn = &lease.turn.turn_id;
                    crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    reject_sealed(tx, session, turn)?;
                    let latest = latest_epoch(tx, session, turn, lease.invocation.as_str())?;
                    if let Some((latest_epoch, latest_state)) = latest.as_ref()
                        && *latest_epoch > lease.attempt_epoch
                        && latest_state == "live"
                    {
                        let prior: Option<String> = tx
                            .query_row(
                                SQL.writers.select_epoch.sql(),
                                params![
                                    session.as_str(),
                                    turn.as_str(),
                                    lease.invocation.as_str(),
                                    i64::from(lease.attempt_epoch)
                                ],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if matches!(prior.as_deref(), Some("fenced" | "retracted")) {
                            let (_, base, _, _) =
                                turn_row(tx, session, turn)?.ok_or_else(|| {
                                    stored_data_corrupt("TurnCapture", "missing turn row")
                                })?;
                            if unsigned(base)? != u64::from(lease.base.0) {
                                return Err(StoreError::CaptureBaseStale {
                                    session_id: session.clone(),
                                    turn_id: turn.clone(),
                                    offered: lease.base.0,
                                    current: u32::try_from(unsigned(base)?).map_err(|_| {
                                        stored_data_corrupt("TurnCapture", "base overflow")
                                    })?,
                                });
                            }
                            if prior.as_deref() == Some("fenced") {
                                tx.execute(
                                    SQL.writers.retract_fenced.sql(),
                                    params![
                                        session.as_str(),
                                        turn.as_str(),
                                        lease.invocation.as_str(),
                                        i64::from(lease.attempt_epoch)
                                    ],
                                )
                                .map_err(sqlite_error)?;
                            }
                            return lease_at(
                                tx,
                                &lease.turn,
                                &lease.invocation,
                                *latest_epoch,
                                lease.base,
                            );
                        }
                    }
                    let next = lease
                        .attempt_epoch
                        .checked_add(1)
                        .ok_or_else(|| StoreError::Backend("capture epoch overflow".into()))?;
                    if latest.as_ref().is_some_and(|(epoch, _)| *epoch == next) {
                        let prior: Option<String> = tx
                            .query_row(
                                SQL.writers.select_epoch.sql(),
                                params![
                                    session.as_str(),
                                    turn.as_str(),
                                    lease.invocation.as_str(),
                                    i64::from(lease.attempt_epoch)
                                ],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if prior.as_deref() == Some("retracted") {
                            return lease_at(tx, &lease.turn, &lease.invocation, next, lease.base);
                        }
                    }
                    require_writer(tx, lease)?;
                    tx.execute(
                        SQL.writers.retract.sql(),
                        params![
                            session.as_str(),
                            turn.as_str(),
                            lease.invocation.as_str(),
                            i64::from(lease.attempt_epoch)
                        ],
                    )
                    .map_err(sqlite_error)?;
                    tx.execute(
                        SQL.writers.insert.sql(),
                        params![
                            session.as_str(),
                            turn.as_str(),
                            lease.invocation.as_str(),
                            i64::from(next),
                            "live"
                        ],
                    )
                    .map_err(sqlite_error)?;
                    lease_at(tx, &lease.turn, &lease.invocation, next, lease.base)
                })();
                Ok(match result {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn advance_capture_base(&self, advance: &CaptureBaseAdvance) -> Result<(), StoreError> {
        self.bind_session(&advance.turn.session_id)?;
        let advance = advance.clone();
        self.conn
            .write_flow(move |tx| {
                let result = (|| {
                    let session = &advance.turn.session_id;
                    let turn = &advance.turn.turn_id;
                    crate::persistence::ensure_session_not_deleted_conn(tx, session)?;
                    reject_sealed(tx, session, turn)?;
                    let (_, base, _, _) = turn_row(tx, session, turn)?.ok_or_else(|| {
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
                    tx.execute(
                        SQL.turns.advance.sql(),
                        params![
                            session.as_str(),
                            turn.as_str(),
                            i64::from(advance.to.0),
                            base
                        ],
                    )
                    .map_err(sqlite_error)?;
                    tx.execute(
                        SQL.frames.delete_before_base.sql(),
                        params![session.as_str(), turn.as_str(), i64::from(advance.to.0)],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })();
                Ok(match result {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn seal_turn_capture(
        &self,
        request: &SealTurnCapture,
    ) -> Result<SealedCapture, StoreError> {
        self.bind_session(&request.turn.session_id)?;
        let request = request.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let result = seal_capture_conn(tx, &request, now, false);
                Ok(match result {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn read_stopped_partial(
        &self,
        request: &StoppedPartialReadRequest,
    ) -> Result<StoppedPartialRead, StoreError> {
        self.bind_session(&request.session_id)?;
        let request = request.clone();
        self.conn.call(move |conn| {
            let result = (|| {
                crate::persistence::ensure_session_not_deleted_conn(conn, &request.session_id)?;
                let row: Option<(String, Option<i64>)> = conn.query_row(SQL.partials.select_by_turn_or_root.sql(),
                    params![request.session_id.as_str(), request.turn.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(sqlite_error)?;
                if let Some((json, committed)) = row {
                    return Ok(if committed.is_some() { StoppedPartialRead::Available(decode(&json)?) } else { StoppedPartialRead::Pending });
                }
                if turn_row(conn, &request.session_id, &request.turn)?.is_some() { return Ok(StoppedPartialRead::Pending); }
                let receipt_key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(&request.session_id, &request.turn)?;
                let committed: bool = conn.query_row(crate::session_sql::session_sql().turn_commits.exists_for_turn.sql(),
                    params![request.session_id.as_str(), receipt_key], |row| row.get(0)).map_err(sqlite_error)?;
                Ok(if committed { StoppedPartialRead::NotStopped } else { StoppedPartialRead::Unknown })
            })();
            result.map_err(crate::sqlite_conversion_error)
        }).await.map_err(sqlite_error)
    }
}
