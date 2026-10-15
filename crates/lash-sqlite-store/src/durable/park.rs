//! Current park diagnostics and control redrive, committed under the actor fence.

use lash_durable::domain::{RedriveAnswer, RedriveRequest, SessionParksRedrive};
use lash_durable::{ActorKey, ActorState, DurableInstant, Woken};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, SQL, actor_key, wake_within};
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

/// Redrive, for a boot, every session parked with a reason `redrive` names
/// (FIG-6310), each as [`redrive`] would and only while the park read here
/// still stands: the sessions redriven, by key.
pub(super) fn redrive_session_parks(
    tx: &Connection,
    redrive: &SessionParksRedrive,
    now: DurableInstant,
) -> Answer<(Vec<ActorKey>, Vec<Woken>)> {
    let parked = tx
        .prepare_cached(SQL.park.parked_sessions.sql())?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut redriven = Vec::new();
    let mut woken = Vec::new();
    for (key, park) in parked {
        let Some(park) = park.filter(|park| redrive.redrives(park)) else {
            continue;
        };
        let readied = tx
            .prepare_cached(SQL.park.redrive_parked.sql())?
            .query_row(rusqlite::params![key, now.0, park], |_| Ok(()))
            .optional()?;
        if readied.is_none() {
            continue;
        }
        let actor = match actor_key(&key) {
            Ok(actor) => actor,
            Err(error) => return Ok(Err(error)),
        };
        woken.push(Woken {
            actor: actor.clone(),
            state: ActorState::Ready,
            owner: None,
        });
        redriven.push(actor);
    }
    Ok(Ok((redriven, woken)))
}
