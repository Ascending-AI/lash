//! Run records on SQLite: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use lash_durable::domain::{OwnerKey, RunRecordRow, RunRecordWrite};
use rusqlite::Connection;

use super::{Answer, Committing};

/// `run_records`, created by I0 (FIG-5194); its statements are V0's.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS run_records (
    owner_key TEXT NOT NULL,
    run_seq INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_run_records_kind
        CHECK (kind IN ('admit', 'x_start', 'x_outcome', 'decide', 'present', 'retry')),
    call_id TEXT,
    record_json TEXT NOT NULL,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (owner_key, run_seq, ordinal)
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_run_records_outcome
    ON run_records (owner_key, run_seq, call_id)
    WHERE kind = 'x_outcome' AND call_id IS NOT NULL;
";

pub(super) fn apply(
    _tx: &Connection,
    _commit: &Committing<'_>,
    _write: &RunRecordWrite,
) -> Answer<()> {
    todo!("V0 (FIG-5170): append or prune run records on SQLite")
}

pub(super) fn read(_tx: &Connection, _owner: &OwnerKey) -> Answer<Vec<RunRecordRow>> {
    todo!("V0 (FIG-5170): read an owner's run records on SQLite")
}
