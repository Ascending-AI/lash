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
            // One guarded write: the first root when there is none, else
            // the next over the expected revision and root. A put whose
            // expectation names a revision without a root, or a root without
            // a revision, never matches a row.
            let put: Option<i64> = match (expected, expected_root) {
                (None, None) => crate::observed_sql::query_scalar(SQL.put_first.sql())
                    .bind(&key)
                    .bind(root.to_hex())
                    .bind(commit_id.0.to_vec())
                    .bind(commit.epoch.0)
                    .fetch_optional(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?,
                (Some(rev), Some(expected_root)) => {
                    crate::observed_sql::query_scalar(SQL.put_next.sql())
                        .bind(&key)
                        .bind(integer::<i64>(rev.0)?)
                        .bind(root.to_hex())
                        .bind(commit_id.0.to_vec())
                        .bind(commit.epoch.0)
                        .bind(expected_root.to_hex())
                        .fetch_optional(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?
                }
                _ => None,
            };
            if put.is_some() {
                return Ok(());
            }
            // Refused: read the revision it found, for the refusal's account.
            let found: Option<i64> = crate::observed_sql::query_scalar(SQL.root.sql())
                .bind(&key)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Err(conflict(exec, *expected, found.map(revision).transpose()?))
        }
        ExecRootWrite::Retire { exec, expected } => {
            let retired = crate::observed_sql::query(SQL.retire.sql())
                .bind(exec.stored())
                .bind(expected.to_hex())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if retired.is_none() {
                return Err(conflict(exec, None, None));
            }
            // The consumer took the cell's report and state: its own facts
            // and admissions have no reader left.
            if let ExecKey::Cell(..) = exec {
                super::run_records::delete_owner(&mut *tx, &exec.stored())
                    .await
                    .map_err(sqlx_failure)?;
                super::graph::clear_admissions(&mut *tx, &exec.stored())
                    .await
                    .map_err(sqlx_failure)?;
            }
            Ok(())
        }
        ExecRootWrite::Clear { exec } => {
            crate::observed_sql::query(SQL.clear.sql())
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
    let Some(row) = crate::observed_sql::query(SQL.read.sql())
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
    let rows = crate::observed_sql::query(SQL.under.sql())
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

/// Delete every execution root whose key starts with `prefix`, with the
/// admissions and run facts of each: the cells one turn stopped in, at its
/// end, or one session's, at its delete.
pub(super) async fn retire_under(tx: &mut PgConnection, prefix: &str) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.clear_under.sql())
        .bind(prefix)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    super::graph::clear_admissions_under(&mut *tx, prefix).await?;
    super::run_records::delete_under(tx, prefix).await
}

/// The root of every execution whose key starts with `prefix`.
pub(super) async fn roots_under(
    tx: &mut PgConnection,
    prefix: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar::<_, String>(SQL.under.sql())
        .bind(prefix)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}
