//! Current park diagnostics and control redrive, committed under the actor fence.

use lash_durable::domain::{RedriveAnswer, RedriveRequest};
use lash_durable::{DurableInstant, Woken};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, SQL, wake_within};
use crate::conn::cached_execute;

pub(super) fn set(tx: &Connection, actor: &str, reason: &Option<String>) -> Answer<()> {
    cached_execute(
        tx,
        SQL.park.set_park.sql(),
        rusqlite::params![actor, reason],
    )?;
    Ok(Ok(()))
}

/// Redrive a parked actor: control-wake it, then clear its park and its
/// failed activations. A ready actor needs no park,
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
    Ok(Ok((RedriveAnswer::Redriven, Some(woken))))
}
