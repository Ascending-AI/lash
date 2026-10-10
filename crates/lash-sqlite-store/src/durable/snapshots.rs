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

use super::{Answer, Committing, integer};
use crate::conn::cached_execute;

static SQL: LazyLock<SnapshotStatements> =
    LazyLock::new(|| SnapshotStatements::render(crate::schema_layout::MAIN));

fn revision(stored: i64) -> rusqlite::Result<SnapshotRev> {
    integer(stored).map(SnapshotRev)
}

/// `exec_snapshots`, created by I0 (FIG-5194); its statements are V0's.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS exec_snapshots (
    exec_key TEXT PRIMARY KEY,
    rev INTEGER NOT NULL CONSTRAINT ck_exec_snapshots_rev CHECK (rev >= 1),
    snapshot_ref TEXT NOT NULL,
    executable_identity TEXT NOT NULL,
    format_version INTEGER NOT NULL CONSTRAINT ck_exec_snapshots_format_version
        CHECK (format_version BETWEEN 0 AND 4294967295),
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
                .map(revision)
                .transpose()?;
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
                            integer::<i64>(expected.0)?,
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
                rev: revision(row.get(0)?)?,
                snapshot_ref: row.get(1)?,
                executable_identity: row.get(2)?,
                format_version: row.get(3)?,
                written_epoch: Epoch(row.get(4)?),
            })
        })
        .optional()?;
    Ok(Ok(row))
}

/// Every snapshot whose execution key starts with `prefix`, by key.
pub(super) fn under(tx: &Connection, prefix: &str) -> Answer<Vec<SnapshotRow>> {
    let rows = tx
        .prepare_cached(SQL.under.sql())?
        .query_map([prefix], |row| {
            Ok((
                row.get::<_, String>(0)?,
                revision(row.get(1)?)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
                Epoch(row.get(5)?),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(
            |(key, rev, snapshot_ref, executable_identity, format_version, written_epoch)| {
                Ok(SnapshotRow {
                    exec: ExecKey::parse(&key)
                        .map_err(|error| super::corrupt("execution key", &error.0))?,
                    rev,
                    snapshot_ref,
                    executable_identity,
                    format_version,
                    written_epoch,
                })
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_durable::domain::CellId;
    use lash_durable::{ActorKey, DurableInstant};

    #[test]
    fn snapshots_preserve_full_format_version_and_refuse_oversized_revision() {
        let conn = Connection::open_in_memory().expect("open snapshots");
        conn.execute_batch(TABLES).expect("create snapshots");
        for version in [-1_i64, i64::from(u32::MAX) + 1] {
            let error = conn
                .execute(
                    "INSERT INTO exec_snapshots VALUES ('bad', 1, 'snapshot', 'identity', ?1, 1)",
                    [version],
                )
                .expect_err("format versions are u32");
            assert!(
                error
                    .to_string()
                    .contains("ck_exec_snapshots_format_version")
            );
        }

        let actor = ActorKey::session("session").expect("actor");
        let commit = Committing {
            actor: &actor,
            epoch: Epoch(1),
            now: DurableInstant(1),
            fleet: lash_core_execution::FleetFormat::current(),
            blob_profile: crate::BuiltinBlobProfile::default(),
        };
        let exec = ExecKey::Cell("session".into(), "turn".into(), CellId::new("cell"));
        let put = |expected| SnapshotWrite::Put {
            exec: exec.clone(),
            expected,
            snapshot_ref: "snapshot".into(),
            executable_identity: "identity".into(),
            format_version: u32::MAX,
        };
        apply(&conn, &commit, &put(None))
            .expect("insert")
            .expect("accepted");
        let before = read(&conn, &exec)
            .expect("read")
            .expect("decoded")
            .expect("snapshot");
        assert_eq!(before.format_version, u32::MAX);
        assert!(!matches!(
            apply(&conn, &commit, &put(Some(SnapshotRev(u64::MAX)))),
            Ok(Ok(()))
        ));
        assert_eq!(
            read(&conn, &exec).expect("read").expect("decoded"),
            Some(before)
        );
    }
}
