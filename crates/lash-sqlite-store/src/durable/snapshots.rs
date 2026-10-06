//! VM snapshots on SQLite: the `snapshots` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_durable::domain::{DomainRefusal, ExecKey, SnapshotRev, SnapshotRow, SnapshotWrite};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::durable::snapshots::SnapshotStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing};
use crate::conn::cached_execute;

static SQL: LazyLock<SnapshotStatements> =
    LazyLock::new(|| SnapshotStatements::render(crate::schema_layout::MAIN));

fn revision(stored: i64) -> SnapshotRev {
    SnapshotRev(u64::try_from(stored).unwrap_or(0))
}

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

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &SnapshotWrite) -> Answer<()> {
    match write {
        SnapshotWrite::Put {
            exec,
            expected,
            snapshot_ref,
            executable_identity,
            format_version,
        } => {
            let key = exec.stored();
            let found = tx
                .prepare_cached(SQL.rev.sql())?
                .query_row([&key], |row| row.get::<_, i64>(0))
                .optional()?
                .map(revision);
            if found != *expected {
                return Ok(Err(DurableError::Domain(
                    DomainRefusal::SnapshotRevConflict {
                        exec: exec.clone(),
                        expected: *expected,
                        found,
                    },
                )));
            }
            match expected {
                None => {
                    cached_execute(
                        tx,
                        SQL.insert.sql(),
                        rusqlite::params![
                            key,
                            snapshot_ref,
                            executable_identity,
                            format_version,
                            commit.epoch.0
                        ],
                    )?;
                }
                Some(expected) => {
                    tx.prepare_cached(SQL.replace.sql())?.query_row(
                        rusqlite::params![
                            key,
                            i64::try_from(expected.0).unwrap_or(i64::MAX),
                            snapshot_ref,
                            executable_identity,
                            format_version,
                            commit.epoch.0
                        ],
                        |_| Ok(()),
                    )?;
                }
            }
            Ok(Ok(()))
        }
        SnapshotWrite::Delete { exec } => {
            cached_execute(tx, SQL.delete.sql(), [exec.stored()])?;
            Ok(Ok(()))
        }
    }
}

pub(super) fn read(tx: &Connection, exec: &ExecKey) -> Answer<Option<SnapshotRow>> {
    let row = tx
        .prepare_cached(SQL.read.sql())?
        .query_row([exec.stored()], |row| {
            Ok(SnapshotRow {
                exec: exec.clone(),
                rev: revision(row.get(0)?),
                snapshot_ref: row.get(1)?,
                executable_identity: row.get(2)?,
                format_version: row.get(3)?,
                written_epoch: Epoch(row.get(4)?),
            })
        })
        .optional()?;
    Ok(Ok(row))
}
