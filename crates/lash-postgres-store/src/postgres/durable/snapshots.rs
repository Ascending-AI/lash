//! Execution roots on PostgreSQL: the `snapshots` domain's statements, apply
//! and read (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit, after the fence and the installation of the commit's
//! staged graph nodes; a refusal rolls the whole commit back. The DDL is in
//! `schema.sql`.

use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_durable::domain::{DomainRefusal, ExecKey, ExecRootRow, ExecRootWrite, SnapshotRev};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::Dialect;
use lash_store_sql::durable::snapshots::SnapshotStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, corrupt, integer, node_ref, sqlx_failure};

static SQL: LazyLock<SnapshotStatements> =
    LazyLock::new(|| SnapshotStatements::render(Dialect::postgres()));

fn revision(stored: i64) -> Result<SnapshotRev, DurableError> {
    integer(stored).map(SnapshotRev)
}

fn conflict(
    exec: &ExecKey,
    expected: Option<SnapshotRev>,
    found: Option<SnapshotRev>,
) -> DurableError {
    DurableError::Domain(DomainRefusal::SnapshotRevConflict {
        exec: exec.clone(),
        expected,
        found,
    })
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ExecRootWrite,
) -> Result<(), DurableError> {
    match write {
        ExecRootWrite::Put {
            exec,
            expected,
            expected_root,
            root,
            commit_id,
        } => {
            let key = exec.stored();
            let found = sqlx::query(SQL.root.sql())
                .bind(&key)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            let (found_rev, found_root) = match found {
                None => (None, None),
                Some(row) => {
                    let rev: i64 = row.try_get(0).map_err(sqlx_failure)?;
                    let stored: String = row.try_get(1).map_err(sqlx_failure)?;
                    (Some(revision(rev)?), Some(node_ref(&stored)?))
                }
            };
            if found_rev != *expected || found_root != *expected_root {
                return Err(conflict(exec, *expected, found_rev));
            }
            match expected {
                None => {
                    sqlx::query(SQL.insert.sql())
                        .bind(&key)
                        .bind(root.to_hex())
                        .bind(commit_id.0.to_vec())
                        .bind(commit.epoch.0)
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                }
                Some(rev) => {
                    let replaced = sqlx::query(SQL.replace.sql())
                        .bind(&key)
                        .bind(integer::<i64>(rev.0)?)
                        .bind(root.to_hex())
                        .bind(commit_id.0.to_vec())
                        .bind(commit.epoch.0)
                        .fetch_optional(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                    if replaced.is_none() {
                        return Err(conflict(exec, *expected, found_rev));
                    }
                }
            }
            Ok(())
        }
        ExecRootWrite::Retire { exec, expected } => {
            let retired = sqlx::query(SQL.retire.sql())
                .bind(exec.stored())
                .bind(expected.to_hex())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if retired.is_none() {
                return Err(conflict(exec, None, None));
            }
            Ok(())
        }
        ExecRootWrite::Clear { exec } => {
            sqlx::query(SQL.clear.sql())
                .bind(exec.stored())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

fn row_of(
    exec: ExecKey,
    rev: i64,
    root: &str,
    epoch: i64,
    commit_id: &[u8],
) -> Result<ExecRootRow, DurableError> {
    let root: NodeId = node_ref(root)?;
    let commit_id =
        <[u8; 32]>::try_from(commit_id).map_err(|_| corrupt("commit id", &exec.stored()))?;
    Ok(ExecRootRow {
        exec,
        rev: revision(rev)?,
        root,
        written_epoch: Epoch(epoch),
        commit_id: CommitId(commit_id),
    })
}

pub(super) async fn read(
    tx: &mut PgConnection,
    exec: &ExecKey,
) -> Result<Option<ExecRootRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.read.sql())
        .bind(exec.stored())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let root: String = row.try_get(1).map_err(sqlx_failure)?;
    let commit_id: Vec<u8> = row.try_get(3).map_err(sqlx_failure)?;
    row_of(
        exec.clone(),
        row.try_get(0).map_err(sqlx_failure)?,
        &root,
        row.try_get(2).map_err(sqlx_failure)?,
        &commit_id,
    )
    .map(Some)
}

/// Every execution root whose key starts with `prefix`, by key.
pub(super) async fn under(
    tx: &mut PgConnection,
    prefix: &str,
) -> Result<Vec<ExecRootRow>, DurableError> {
    let rows = sqlx::query(SQL.under.sql())
        .bind(prefix)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    rows.iter()
        .map(|row| {
            let key: String = row.try_get(0).map_err(sqlx_failure)?;
            let root: String = row.try_get(2).map_err(sqlx_failure)?;
            let commit_id: Vec<u8> = row.try_get(4).map_err(sqlx_failure)?;
            row_of(
                ExecKey::parse(&key).map_err(|error| corrupt("execution key", &error.0))?,
                row.try_get(1).map_err(sqlx_failure)?,
                &root,
                row.try_get(3).map_err(sqlx_failure)?,
                &commit_id,
            )
        })
        .collect()
}
