//! VM snapshots on SQLite: the `snapshots` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{ExecKey, SnapshotRow, SnapshotWrite};
use rusqlite::Connection;

use super::{Answer, Committing};

/// `exec_snapshots`, created by I0 (FIG-5194); its statements are V0's.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS exec_snapshots (
    exec_key TEXT PRIMARY KEY,
    rev INTEGER NOT NULL CONSTRAINT ck_exec_snapshots_rev CHECK (rev >= 1),
    snapshot_ref TEXT NOT NULL,
    executable_identity TEXT NOT NULL,
    format_version INTEGER NOT NULL,
    written_epoch INTEGER NOT NULL
);
";

pub(super) fn apply(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &SnapshotWrite,
) -> Answer<()> {
    todo!("V0 (FIG-5170): compare-and-set an execution's snapshot on SQLite")
}

pub(super) fn read(_tx: &Connection, _exec: &ExecKey) -> Answer<Option<SnapshotRow>> {
    todo!("V0 (FIG-5170): read an execution's latest snapshot on SQLite")
}
