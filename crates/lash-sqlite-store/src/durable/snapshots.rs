//! Execution roots on SQLite: the `snapshots` domain's statements, apply and
//! read (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L7 (FIG-5177). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit, after the fence and the installation of the commit's
//! staged graph nodes; a refusal rolls the whole commit back.

use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_durable::domain::{DomainRefusal, ExecKey, ExecRootRow, ExecRootWrite, SnapshotRev};
use lash_durable::{DurableError, Epoch};
use lash_store_sql::durable::snapshots::SnapshotStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer, node_ref};

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
    -- The execution's ExecRoot node (FIG-5887 §7) and the commit that wrote it.
    root_ref TEXT NOT NULL REFERENCES blobs(hash) ON DELETE RESTRICT,
    commit_id BLOB NOT NULL CONSTRAINT ck_exec_snapshots_commit_id CHECK (length(commit_id) = 32),
    written_epoch INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_snapshots_root_ref ON exec_snapshots (root_ref);
";

fn conflict(
    exec: &ExecKey,
    expected: Option<SnapshotRev>,
    found: Option<SnapshotRev>,
) -> Answer<()> {
    Ok(Err(DurableError::Domain(
        DomainRefusal::SnapshotRevConflict {
            exec: exec.clone(),
            expected,
            found,
        },
    )))
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &ExecRootWrite) -> Answer<()> {
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
            let put = match (expected, expected_root) {
                (None, None) => tx
                    .prepare_cached(SQL.put_first.sql())?
                    .query_row(
                        rusqlite::params![key, root.to_hex(), &commit_id.0[..], commit.epoch.0],
                        |_| Ok(()),
                    )
                    .optional()?,
                (Some(rev), Some(expected_root)) => tx
                    .prepare_cached(SQL.put_next.sql())?
                    .query_row(
                        rusqlite::params![
                            key,
                            integer::<i64>(rev.0)?,
                            root.to_hex(),
                            &commit_id.0[..],
                            commit.epoch.0,
                            expected_root.to_hex(),
                        ],
                        |_| Ok(()),
                    )
                    .optional()?,
                _ => None,
            };
            if put.is_some() {
                if let Some(old) = expected_root.filter(|old| old != root) {
                    commit.retired.closure(&old);
                }
                return Ok(Ok(()));
            }
            // Refused: read the revision it found, for the refusal's account.
            let found = tx
                .prepare_cached(SQL.root.sql())?
                .query_row([&key], |row| row.get::<_, i64>(0))
                .optional()?;
            conflict(exec, *expected, found.map(revision).transpose()?)
        }
        ExecRootWrite::Retire { exec, expected } => {
            let retired = tx
                .prepare_cached(SQL.retire.sql())?
                .query_row([exec.stored(), expected.to_hex()], |_| Ok(()))
                .optional()?;
            if retired.is_none() {
                return conflict(exec, None, None);
            }
            commit.retired.closure(expected);
            // The consumer took the cell's report and state: its own facts
            // and admissions have no reader left.
            if let ExecKey::Cell(..) = exec {
                let facts = super::run_records::delete_owner(tx, &exec.stored())?;
                let admissions = super::graph::clear_admissions(tx, &exec.stored())?;
                for root in facts.into_iter().chain(admissions) {
                    commit.retired.stored(root);
                }
            }
            Ok(Ok(()))
        }
        ExecRootWrite::Clear { exec } => {
            let cleared = tx
                .prepare_cached(SQL.clear.sql())?
                .query_row([exec.stored()], |row| row.get::<_, String>(0))
                .optional()?;
            if let Some(root) = cleared {
                commit.retired.stored(root);
            }
            Ok(Ok(()))
        }
    }
}

fn row_of(
    exec: ExecKey,
    rev: i64,
    root: &str,
    epoch: i64,
    commit_id: &[u8],
) -> rusqlite::Result<Result<ExecRootRow, DurableError>> {
    let root: NodeId = match node_ref(root) {
        Ok(root) => root,
        Err(error) => return Ok(Err(error)),
    };
    let Ok(commit_id) = <[u8; 32]>::try_from(commit_id) else {
        return Ok(Err(corrupt("commit id", &exec.stored())));
    };
    Ok(Ok(ExecRootRow {
        exec,
        rev: revision(rev)?,
        root,
        written_epoch: Epoch(epoch),
        commit_id: CommitId(commit_id),
    }))
}

pub(super) fn read(tx: &Connection, exec: &ExecKey) -> Answer<Option<ExecRootRow>> {
    let row = tx
        .prepare_cached(SQL.read.sql())?
        .query_row([exec.stored()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .optional()?;
    match row {
        None => Ok(Ok(None)),
        Some((rev, root, epoch, commit_id)) => {
            Ok(row_of(exec.clone(), rev, &root, epoch, &commit_id)?.map(Some))
        }
    }
}

/// Delete every execution root whose key starts with `prefix`, with the
/// admissions and run facts of each: the cells one turn stopped in, at its
/// end, or one session's, at its delete. Answers every root they held: the
/// caller's to peel.
pub(super) fn retire_under(tx: &Connection, prefix: &str) -> rusqlite::Result<Vec<String>> {
    let mut roots: Vec<String> = tx
        .prepare_cached(SQL.clear_under.sql())?
        .query_map([prefix], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    roots.extend(super::graph::clear_admissions_under(tx, prefix)?);
    roots.extend(super::run_records::delete_under(tx, prefix)?);
    Ok(roots)
}

/// Every execution root whose key starts with `prefix`, by key.
pub(super) fn under(tx: &Connection, prefix: &str) -> Answer<Vec<ExecRootRow>> {
    under_limited(tx, prefix, None)
}

pub(super) fn under_limited(
    tx: &Connection,
    prefix: &str,
    max_rows: Option<usize>,
) -> Answer<Vec<ExecRootRow>> {
    let sql = match max_rows {
        None => std::borrow::Cow::Borrowed(SQL.under.sql()),
        Some(max_rows) => std::borrow::Cow::Owned(format!(
            "{} LIMIT {}",
            SQL.under.sql(),
            max_rows.saturating_add(1)
        )),
    };
    let rows = tx
        .prepare_cached(&sql)?
        .query_map([prefix], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(max_rows) = max_rows.filter(|limit| rows.len() > *limit) {
        return Ok(Err(DurableError::Graph(
            lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                dimension: lash_core_store::checkpoint_graph::DecodeDimension::Nodes,
                actual: rows.len() as u64,
                limit: max_rows as u64,
            },
        )));
    }
    let mut roots = Vec::with_capacity(rows.len());
    for (key, rev, root, epoch, commit_id) in rows {
        let exec = match ExecKey::parse(&key) {
            Ok(exec) => exec,
            Err(error) => return Ok(Err(corrupt("execution key", &error.0))),
        };
        match row_of(exec, rev, &root, epoch, &commit_id)? {
            Ok(row) => roots.push(row),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(roots))
}
