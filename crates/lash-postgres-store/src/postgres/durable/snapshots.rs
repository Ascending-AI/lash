//! VM snapshots on PostgreSQL: the `snapshots` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{DomainRefusal, ExecKey, SnapshotRev, SnapshotRow, SnapshotWrite};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::Dialect;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, sqlx_failure};

static SQL: LazyLock<SnapshotStatements> =
    LazyLock::new(|| SnapshotStatements::render(Dialect::postgres()));

fn revision(stored: i64) -> SnapshotRev {
    SnapshotRev(u64::try_from(stored).unwrap_or(0))
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &SnapshotWrite,
) -> Result<(), DurableError> {
    match write {
        SnapshotWrite::Put {
            exec,
            expected,
            snapshot_ref,
            executable_identity,
            format_version,
        } => {
            let key = exec.stored();
            let found: Option<i64> = sqlx::query_scalar(SQL.rev.sql())
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            let found = found.map(revision);
            if found != *expected {
                return Err(DurableError::Domain(DomainRefusal::SnapshotRevConflict {
                    exec: exec.clone(),
                    expected: *expected,
                    found,
                }));
            }
            let format_version = i32::try_from(*format_version).unwrap_or(i32::MAX);
            match expected {
                None => {
                    sqlx::query(SQL.insert.sql())
                        .bind(&key)
                        .bind(snapshot_ref)
                        .bind(executable_identity)
                        .bind(format_version)
                        .bind(commit.epoch.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                }
                Some(expected) => {
                    sqlx::query(SQL.replace.sql())
                        .bind(&key)
                        .bind(i64::try_from(expected.0).unwrap_or(i64::MAX))
                        .bind(snapshot_ref)
                        .bind(executable_identity)
                        .bind(format_version)
                        .bind(commit.epoch.0)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                }
            }
            Ok(())
        }
        SnapshotWrite::Delete { exec } => {
            sqlx::query(SQL.delete.sql())
                .bind(exec.stored())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

pub(super) async fn read(
    tx: &mut PgConnection,
    exec: &ExecKey,
) -> Result<Option<SnapshotRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.read.sql())
        .bind(exec.stored())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let format_version: i32 = row.try_get(3).map_err(sqlx_failure)?;
    Ok(Some(SnapshotRow {
        exec: exec.clone(),
        rev: revision(row.try_get(0).map_err(sqlx_failure)?),
        snapshot_ref: row.try_get(1).map_err(sqlx_failure)?,
        executable_identity: row.try_get(2).map_err(sqlx_failure)?,
        format_version: u32::try_from(format_version).unwrap_or(0),
        written_epoch: Epoch(row.try_get(4).map_err(sqlx_failure)?),
    }))
}
