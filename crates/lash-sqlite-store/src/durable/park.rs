//! Current park diagnostics and control redrive, committed under the actor fence.

use lash_durable::domain::{RedriveAnswer, RedriveRequest, SessionParksRedrive};
use lash_durable::{ActorKey, ActorState, DurableInstant, Woken};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, SQL, actor_key};
use crate::conn::cached_execute;

pub(super) fn set(tx: &Connection, actor: &str, reason: &Option<String>) -> Answer<()> {
    cached_execute(
        tx,
        SQL.park.set_park.sql(),
        rusqlite::params![actor, reason],
    )?;
    Ok(Ok(()))
}

/// Redrive only the observed actual park, advancing revision and mailbox
/// together with readiness and diagnostic cleanup. A claimed actor refuses.
pub(super) fn redrive(
    tx: &Connection,
    request: &RedriveRequest,
    now: DurableInstant,
) -> Answer<(RedriveAnswer, Option<Woken>)> {
    let actor = request.actor.as_str();
    let park = tx
        .prepare_cached(SQL.park.park_of.sql())?
        .query_row([actor], |row| row.get::<_, Option<String>>(0))
        .optional()?
        .flatten();
    let Some(park) = park else {
        return Ok(Ok((RedriveAnswer::NotParked, None)));
    };
    let changed = tx
        .prepare_cached(SQL.park.redrive_parked.sql())?
        .query_row(rusqlite::params![actor, now.0, park], |_| Ok(()))
        .optional()?;
    if changed.is_none() {
        return Ok(Ok((RedriveAnswer::NotParked, None)));
    }
    Ok(Ok((
        RedriveAnswer::Redriven,
        Some(Woken {
            actor: request.actor.clone(),
            state: ActorState::Ready,
            owner: None,
        }),
    )))
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
