//! The operator's park feed on SQLite: the `park_events` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by L6 (FIG-5175). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{ParkEventRow, ParkEventSeq, ParkEventWrite};
use rusqlite::Connection;

use super::{Answer, Committing};

pub(super) fn apply(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &ParkEventWrite,
) -> Answer<()> {
    todo!("L6 (FIG-5175): append to the park feed on SQLite")
}

pub(super) fn read(
    _tx: &Connection,
    _after: Option<ParkEventSeq>,
    _limit: usize,
) -> Answer<Vec<ParkEventRow>> {
    todo!("L6 (FIG-5175): read the park feed on SQLite")
}
