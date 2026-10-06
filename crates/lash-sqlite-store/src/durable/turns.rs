//! Turn phase state, the session commit and turn-cancel mail on SQLite: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::Woken;
use lash_durable::domain::{
    SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnRow, TurnWrite,
};
use lash_sansio::SessionId;
use rusqlite::Connection;

use super::{Answer, Committing};

pub(super) fn apply(_tx: &Connection, _commit: &Committing<'_>, _write: &TurnWrite) -> Answer<()> {
    todo!("V0 (FIG-5170): write the turn phase row on SQLite")
}

pub(super) fn apply_session_commit(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &SessionCommitWrite,
) -> Answer<()> {
    todo!("V0 (FIG-5170): commit the turn to its session head on SQLite")
}

pub(super) fn request_cancel(
    _tx: &Connection,
    _request: &TurnCancelRequest,
    _now: lash_durable::DurableInstant,
) -> Answer<(TurnCancelAnswer, Option<Woken>)> {
    todo!("L3 (FIG-5172): record a turn cancel request and wake the session on SQLite")
}

pub(super) fn turn(_tx: &Connection, _session: &SessionId) -> Answer<Option<TurnRow>> {
    todo!("V0 (FIG-5170): read the session's unfinished turn on SQLite")
}
