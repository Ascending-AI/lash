//! Process actors, cancel, terminal and cascade on SQLite: the `processes` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{CancelAnswer, CancelRequest, ProcessActorRow, ProcessWrite, ScopeKey};
use lash_durable::{DurableInstant, Woken};
use lash_sansio::ProcessId;
use rusqlite::Connection;

use super::{Answer, Committing};

pub(super) fn apply(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &ProcessWrite,
) -> Answer<()> {
    todo!("L6 (FIG-5175): register, advance, end or cascade a process on SQLite")
}

pub(super) fn request_cancel(
    _tx: &Connection,
    _request: &CancelRequest,
    _now: DurableInstant,
) -> Answer<(CancelAnswer, Option<Woken>)> {
    todo!("L6 (FIG-5175): record a process's first cancel request and control-wake it on SQLite")
}

pub(super) fn process(_tx: &Connection, _process: &ProcessId) -> Answer<Option<ProcessActorRow>> {
    todo!("L6 (FIG-5175): read a process actor's row on SQLite")
}

pub(super) fn live_until_descendants(
    _tx: &Connection,
    _scope: &ScopeKey,
    _limit: usize,
) -> Answer<Vec<ProcessId>> {
    todo!("L6 (FIG-5175): list a scope's live Until descendants on SQLite")
}
