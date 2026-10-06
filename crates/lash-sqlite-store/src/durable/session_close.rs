//! The session's closing state on SQLite: the `session_close` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6b (FIG-5176). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::SessionCloseWrite;
use rusqlite::Connection;

use super::{Answer, Committing};

pub(super) fn apply(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &SessionCloseWrite,
) -> Answer<()> {
    todo!("L6b (FIG-5176): record a session close step on SQLite")
}
