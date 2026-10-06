//! Waits, keyed promises and timers on SQLite: the `waits` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L5 (FIG-5173). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{ResolveAnswer, WaitId, WaitResolution, WaitRow, WaitWrite};
use lash_durable::{ActorKey, DurableInstant, Woken};
use rusqlite::Connection;

use super::{Answer, Committing};

pub(super) fn apply(_tx: &Connection, _commit: &Committing<'_>, _write: &WaitWrite) -> Answer<()> {
    todo!("L5 (FIG-5173): pin, settle or revoke waits on SQLite")
}

pub(super) fn resolve(
    _tx: &Connection,
    _resolution: &WaitResolution,
    _now: DurableInstant,
) -> Answer<(ResolveAnswer, Option<Woken>)> {
    todo!("L5 (FIG-5173): resolve a wait from pending, first winner, and wake its owner on SQLite")
}

pub(super) fn pending(_tx: &Connection, _owner: &ActorKey) -> Answer<Vec<WaitRow>> {
    todo!("L5 (FIG-5173): read an actor's pending waits on SQLite")
}

pub(super) fn wait(_tx: &Connection, _id: &WaitId) -> Answer<Option<WaitRow>> {
    todo!("L5 (FIG-5173): read one wait on SQLite")
}
