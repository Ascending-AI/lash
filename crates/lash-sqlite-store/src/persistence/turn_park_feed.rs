//! The turn park feed's in-transaction append (FIG-3659).
//!
//! Every park write and every park clear runs the same two steps inside the
//! transaction that changes `turn_parks`: bump `turn_park_clock` to allocate
//! the event's `seq` — the write transaction's lock orders writers, so `seq`
//! order is commit order — then insert the `turn_park_events` row. A clear
//! that deletes no park row appends nothing, so the hot commit path pays
//! nothing.

use rusqlite::params;

use super::*;

/// Allocate one turn park feed sequence: the `bump` then the read-back run
/// back to back under the write lock (the PostgreSQL backend folds the pair
/// into one `RETURNING` round trip).
fn allocate_turn_park_seq_conn(conn: &rusqlite::Connection) -> Result<i64, StoreError> {
    let clock = &crate::turn_ingress::turn_ingress_sql().turn_park_clock;
    let bumped = conn.execute(clock.bump.sql(), []).map_err(sqlite_error)?;
    if bumped != 1 {
        return Err(StoreError::Backend(format!(
            "turn park clock bump touched {bumped} rows, expected its one seed row"
        )));
    }
    conn.query_row(clock.select_current.sql(), [], |row| row.get(0))
        .map_err(sqlite_error)
}

/// Insert the feed row at sequence `seq`, naming the park `park_id`
/// transitioned in `session_id`'s turn `turn_id` at `at_ms`.
fn insert_turn_park_event_conn(
    conn: &rusqlite::Connection,
    seq: i64,
    session_id: &SessionId,
    turn_id: &str,
    park_id: i64,
    kind: &lash_core_execution::store::ParkEventKind,
    at_ms: i64,
    build_generation: Option<&str>,
) -> Result<(), StoreError> {
    let (cause, reason_json) = kind.encode_columns();
    conn.execute(
        crate::turn_ingress::turn_ingress_sql()
            .turn_park_events
            .insert_event
            .sql(),
        params![
            seq,
            session_id.as_str(),
            turn_id,
            park_id,
            kind.kind_code(),
            cause,
            reason_json,
            at_ms,
            build_generation
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Append the `Parked` event that opens a park, returning the feed sequence
/// the park record stores as its `park_id`. `build_generation` stamps the
/// drain generation of the build whose checkpoint the park resumes
/// (FIG-3795).
pub(crate) fn log_turn_parked_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    turn_id: &str,
    reason: &lash_core_execution::store::ParkReason,
    at_ms: i64,
    build_generation: Option<&str>,
) -> Result<i64, StoreError> {
    let seq = allocate_turn_park_seq_conn(conn)?;
    insert_turn_park_event_conn(
        conn,
        seq,
        session_id,
        turn_id,
        seq,
        &lash_core_execution::store::ParkEventKind::Parked {
            reason: reason.clone(),
        },
        at_ms,
        build_generation,
    )?;
    Ok(seq)
}

/// Append the event a park clear writes — `Unparked` or `Cancelled` — naming
/// the park `park_id` the delete returned. A closing transition names no
/// checkpoint, so its `park_build_generation` is NULL.
pub(crate) fn log_turn_park_closed_conn(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    turn_id: &str,
    park_id: i64,
    kind: &lash_core_execution::store::ParkEventKind,
    at_ms: i64,
) -> Result<(), StoreError> {
    let seq = allocate_turn_park_seq_conn(conn)?;
    insert_turn_park_event_conn(conn, seq, session_id, turn_id, park_id, kind, at_ms, None)
}
