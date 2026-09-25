//! A session's park, root-addressed (FIG-3600 S7, D2 §1.3): the one record
//! a parked root holds, read and written inside the caller's transaction.
//!
//! Every write is decided by [`decide_turn_park_write`] against the stored
//! park, after the root's terminal evidence was found absent: a root with
//! terminal evidence never parks (P2), so a zombie execution of a root an
//! operator already ended leaves nothing behind.

use lash_core_execution::store::{
    ControlIntentId, StoreError, StoredTurnParkHead, TurnPark, TurnParkEventKind, TurnParkWrite,
    TurnParkWriteDecision, UnparkCause, decide_turn_park_write,
};
use lash_sansio::SessionId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::persistence::turn_park_feed::{log_turn_park_closed_conn, log_turn_parked_conn};
use crate::{sqlite_error, stored_data_corrupt};

fn turn_parks() -> &'static lash_store_sql::turn_ingress::turn_parks::TurnParkStatements {
    &crate::turn_ingress::turn_ingress_sql().turn_parks
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// `session_id`'s park, read on `conn`.
pub(crate) fn turn_park_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<TurnPark>, StoreError> {
    type Row = (
        String,
        i64,
        String,
        String,
        i64,
        i64,
        i64,
        Option<String>,
        Option<i64>,
    );
    let row: Option<Row> = conn
        .query_row(
            turn_parks().select_by_session.sql(),
            params![session_id.as_str()],
            |row| {
                Ok((
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(
        |(
            root,
            park_id,
            reason_code,
            reason_json,
            since_ms,
            last_refused_ms,
            attempts,
            engine_ref,
            resume_intent,
        )| {
            let stored = |field: &str, value: i64| {
                u64::try_from(value)
                    .map_err(|_| stored_data_corrupt("TurnPark", format!("negative {field}")))
            };
            TurnPark::decode(
                session_id.clone(),
                root.into(),
                lash_core_execution::store::ParkId::from_feed_sequence(stored("park_id", park_id)?),
                &reason_code,
                &reason_json,
                stored("since_ms", since_ms)?,
                stored("last_refused_ms", last_refused_ms)?,
                u32::try_from(attempts).unwrap_or(u32::MAX),
                engine_ref,
                resume_intent
                    .map(|intent| stored("resume_intent", intent))
                    .transpose()?,
            )
        },
    )
    .transpose()
}

/// The head a park write is decided against: the stored park, and whether
/// the redrive it names is still open.
fn stored_head(conn: &Connection, park: &TurnPark) -> Result<StoredTurnParkHead, StoreError> {
    let redrive_open = match park.resume_intent {
        Some(intent) => crate::session_roots::load_intent_conn(conn, intent)?
            .is_some_and(|intent| intent.state.is_open()),
        None => false,
    };
    Ok(StoredTurnParkHead {
        root: park.turn_id.clone(),
        engine: park.engine.clone(),
        redrive_open,
        redrive_requested: park.resume_intent.is_some(),
    })
}

/// Record `write` on `conn` (inside the caller's transaction): refuse a
/// root with terminal evidence (P2), then open, supersede, re-park or attach
/// the engine's handle as [`decide_turn_park_write`] rules.
pub(crate) fn record_turn_park_conn(
    conn: &Connection,
    write: &TurnParkWrite,
) -> Result<TurnPark, StoreError> {
    let session_id = &write.session_id;
    if let Some(terminal) =
        crate::session_roots::root_terminal_conn(conn, session_id, &write.turn_id)?
    {
        return Err(StoreError::RootAlreadyTerminal {
            session_id: session_id.clone(),
            root: write.turn_id.clone(),
            by: Box::new(terminal.cause),
        });
    }
    let reason_code = write.reason.code().as_str();
    let reason_json =
        serde_json::to_string(&write.reason).map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "TurnPark".to_string(),
            message: error.to_string(),
        })?;
    let at_ms = sql_i64("turn park instant", write.at_ms)?;
    let engine_ref = write
        .engine
        .as_ref()
        .map(|engine| engine.as_str().to_string());
    let stored = turn_park_conn(conn, session_id)?;
    let head = stored
        .as_ref()
        .map(|park| stored_head(conn, park))
        .transpose()?;
    match (decide_turn_park_write(head.as_ref(), write), stored) {
        (TurnParkWriteDecision::Unchanged, Some(park)) => return Ok(park),
        (TurnParkWriteDecision::AttachEngine, Some(mut park)) => {
            conn.execute(
                turn_parks().attach_engine.sql(),
                params![session_id.as_str(), write.turn_id.as_str(), engine_ref],
            )
            .map_err(sqlite_error)?;
            park.engine.clone_from(&write.engine);
            return Ok(park);
        }
        (TurnParkWriteDecision::Repark, Some(mut park)) => {
            // A same-root re-park keeps `park_id` and `since_ms`, refreshes
            // the reason and `last_refused_ms`, counts the refusal and
            // clears a requested redrive — no feed event.
            conn.execute(
                turn_parks().update_same_turn.sql(),
                params![
                    session_id.as_str(),
                    write.turn_id.as_str(),
                    reason_code,
                    reason_json,
                    at_ms,
                    engine_ref
                ],
            )
            .map_err(sqlite_error)?;
            park.reason = write.reason.clone();
            park.last_refused_ms = write.at_ms;
            park.attempts = park.attempts.saturating_add(1);
            park.resume_intent = None;
            if write.engine.is_some() {
                park.engine.clone_from(&write.engine);
            }
            return Ok(park);
        }
        (TurnParkWriteDecision::Supersede, Some(superseded)) => {
            // A different root's park supersedes the stored one: close it on
            // the feed, then open the new park.
            let deleted: Option<(String, i64)> = conn
                .query_row(
                    turn_parks().delete_for_supersede_returning.sql(),
                    params![session_id.as_str(), write.turn_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?;
            let Some((superseded_root, superseded_park_id)) = deleted else {
                return Err(StoreError::Backend(format!(
                    "turn park supersede of `{}` in session `{session_id}` deleted no row",
                    superseded.turn_id
                )));
            };
            log_turn_park_closed_conn(
                conn,
                session_id,
                &superseded_root,
                superseded_park_id,
                &TurnParkEventKind::Unparked {
                    cause: UnparkCause::Superseded,
                },
                at_ms,
            )?;
        }
        (TurnParkWriteDecision::Open, None) => {}
        (decision, stored) => {
            return Err(StoreError::Backend(format!(
                "turn park write decided {decision:?} against stored park {:?}",
                stored.map(|park| park.park_id)
            )));
        }
    }
    let park_id = log_turn_parked_conn(
        conn,
        session_id,
        write.turn_id.as_str(),
        &write.reason,
        at_ms,
    )?;
    conn.execute(
        turn_parks().insert.sql(),
        params![
            session_id.as_str(),
            write.turn_id.as_str(),
            park_id,
            reason_code,
            reason_json,
            at_ms,
            at_ms,
            1,
            engine_ref
        ],
    )
    .map_err(sqlite_error)?;
    Ok(TurnPark {
        session_id: session_id.clone(),
        turn_id: write.turn_id.clone(),
        reason: write.reason.clone(),
        park_id: lash_core_execution::store::ParkId::from_feed_sequence(
            u64::try_from(park_id).unwrap_or_default(),
        ),
        since_ms: write.at_ms,
        last_refused_ms: write.at_ms,
        attempts: 1,
        engine: write.engine.clone(),
        resume_intent: None,
    })
}

/// Record redrive `intent` on session `session_id`'s park `park_id`, on
/// `conn` (inside the redrive's store half).
pub(crate) fn set_resume_intent_conn(
    conn: &Connection,
    session_id: &SessionId,
    park_id: lash_core_execution::store::ParkId,
    intent: ControlIntentId,
) -> Result<(), StoreError> {
    conn.execute(
        turn_parks().set_resume_intent.sql(),
        params![
            session_id.as_str(),
            sql_i64("park id", park_id.feed_sequence())?,
            sql_i64("control intent id", intent.sequence())?
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
