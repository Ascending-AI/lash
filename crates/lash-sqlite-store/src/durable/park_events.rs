//! The operator's park feed on SQLite: the `park_events` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{
    ParkEventKind, ParkEventRow, ParkEventSeq, ParkEventWrite, RedriveAnswer, RedriveRequest,
};
use lash_durable::{DurableInstant, Woken};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SQL, actor_key, corrupt, wake_within};
use crate::conn::cached_execute;

/// `park_events`: the operator's feed of parks, redrives and ends of parked
/// actors.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS park_events (
    seq INTEGER PRIMARY KEY,
    actor_key TEXT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_park_events_kind
        CHECK (kind IN ('parked', 'redriven', 'ended')),
    reason_json TEXT NOT NULL,
    at_ms INTEGER NOT NULL
);
";

fn append(
    tx: &Connection,
    actor: &str,
    kind: ParkEventKind,
    reason_json: &str,
    at: DurableInstant,
) -> rusqlite::Result<()> {
    cached_execute(
        tx,
        SQL.park_events.append.sql(),
        rusqlite::params![actor, kind.as_str(), reason_json, at.0],
    )?;
    Ok(())
}

pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &ParkEventWrite,
) -> Answer<()> {
    let actor = commit.actor.as_str();
    match write {
        ParkEventWrite::Park { reason_json } => {
            cached_execute(
                tx,
                SQL.park.set_park.sql(),
                rusqlite::params![actor, reason_json],
            )?;
            append(tx, actor, ParkEventKind::Parked, reason_json, commit.now)?;
        }
        ParkEventWrite::Ended { reason_json } => {
            let cleared = tx
                .prepare_cached(SQL.park.clear_park.sql())?
                .query_row([actor], |_| Ok(()))
                .optional()?;
            if cleared.is_some() {
                append(tx, actor, ParkEventKind::Ended, reason_json, commit.now)?;
            }
        }
    }
    Ok(Ok(()))
}

/// Redrive a parked actor: control-wake it, then clear its park and its
/// failed activations and record the redrive. A ready actor needs no park,
/// so the wake goes first.
pub(super) fn redrive(
    tx: &Connection,
    request: &RedriveRequest,
    now: DurableInstant,
) -> Answer<(RedriveAnswer, Option<Woken>)> {
    let actor = request.actor.as_str();
    let parked = tx
        .prepare_cached(SQL.park.park_of.sql())?
        .query_row([actor], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .flatten()
        .is_some();
    if !parked {
        return Ok(Ok((RedriveAnswer::NotParked, None)));
    }
    let woken = match wake_within(tx, &request.actor, true, now)? {
        Ok((woken, _)) => woken,
        Err(error) => return Ok(Err(error)),
    };
    tx.prepare_cached(SQL.park.clear_park.sql())?
        .query_row([actor], |_| Ok(()))?;
    let reason = serde_json::json!({ "requester": request.requester }).to_string();
    append(tx, actor, ParkEventKind::Redriven, &reason, now)?;
    Ok(Ok((RedriveAnswer::Redriven, Some(woken))))
}

pub(super) fn read(
    tx: &Connection,
    after: Option<ParkEventSeq>,
    limit: usize,
) -> Answer<Vec<ParkEventRow>> {
    let after = after.map_or(0, |seq| seq.0);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = tx
        .prepare_cached(SQL.park_events.page.sql())?
        .query_map(rusqlite::params![after, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(seq, actor, kind, reason_json, at)| {
            Ok(ParkEventRow {
                seq: ParkEventSeq(seq),
                actor: actor_key(&actor)?,
                kind: ParkEventKind::parse(&kind)
                    .ok_or_else(|| corrupt("park event kind", &kind))?,
                reason_json,
                at: DurableInstant(at),
            })
        })
        .collect())
}
