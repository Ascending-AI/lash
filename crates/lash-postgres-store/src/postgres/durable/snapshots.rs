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

use super::steps::{self, Entry, Shape, Step, Ty, Value};
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

/// The exec-root statements an owner commit runs as steps.
pub(super) static PUT_FIRST: Entry = Entry::new(
    || &SQL.put_first,
    &[Ty::Text, Ty::Key, Ty::Bytes, Ty::Int],
    Shape::Returning,
);
pub(super) static PUT_NEXT: Entry = Entry::new(
    || &SQL.put_next,
    &[Ty::Text, Ty::Int, Ty::Key, Ty::Bytes, Ty::Int, Ty::Key],
    Shape::Returning,
);
pub(super) static RETIRE: Entry =
    Entry::new(|| &SQL.retire, &[Ty::Text, Ty::Key], Shape::Returning);
/// Every root of the executions under a key: a terminal's cells.
pub(super) static CLEAR_UNDER: Entry =
    Entry::new(|| &SQL.clear_under, &[Ty::Text], Shape::Returning).retiring("root_ref");
pub(super) static CLEAR: Entry =
    Entry::new(|| &SQL.clear, &[Ty::Text], Shape::Returning).retiring("root_ref");

/// `write`'s step program at `epoch`: a put is the first root's insert or
/// the next root's guarded replace, a retire its guarded delete, each
/// answering its row, and a clear one delete. A put whose expectation names
/// a revision without a root, or a root without a revision, has none: it
/// matches no row, and its refusal is read.
pub(super) fn steps(
    epoch: Epoch,
    write: &ExecRootWrite,
) -> Result<Option<Vec<Step>>, DurableError> {
    Ok(match write {
        ExecRootWrite::Put {
            exec,
            expected,
            expected_root,
            root,
            commit_id,
        } => match (expected, expected_root) {
            (None, None) => Some(vec![Step::one(
                &PUT_FIRST,
                vec![
                    Value::text(exec.stored()),
                    Value::key(root.to_hex()),
                    Value::Bytes(Some(commit_id.0.to_vec())),
                    Value::int(epoch.0),
                ],
            )]),
            (Some(rev), Some(expected_root)) => Some(vec![
                Step::one(
                    &PUT_NEXT,
                    vec![
                        Value::text(exec.stored()),
                        Value::int(integer::<i64>(rev.0)?),
                        Value::key(root.to_hex()),
                        Value::Bytes(Some(commit_id.0.to_vec())),
                        Value::int(epoch.0),
                        Value::key(expected_root.to_hex()),
                    ],
                )
                .retiring([expected_root.to_hex()]),
            ]),
            _ => None,
        },
        ExecRootWrite::Retire { exec, expected } => {
            let mut program = vec![
                Step::one(
                    &RETIRE,
                    vec![Value::text(exec.stored()), Value::key(expected.to_hex())],
                )
                .retiring([expected.to_hex()]),
            ];
            // The consumer took the cell's report and state: its own facts
            // and admissions have no reader left.
            if let ExecKey::Cell(..) = exec {
                program.extend([
                    Step::any(
                        &super::run_records::DELETE_OWNER,
                        vec![Value::text(exec.stored())],
                    ),
                    Step::any(
                        &super::graph::CLEAR_ADMISSIONS,
                        vec![Value::text(exec.stored())],
                    ),
                ]);
            }
            Some(program)
        }
        ExecRootWrite::Clear { exec } => {
            Some(vec![Step::any(&CLEAR, vec![Value::text(exec.stored())])])
        }
    })
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &ExecRootWrite,
) -> Result<(), DurableError> {
    if let Some(program) = steps(commit.epoch, write)?
        && steps::run(tx, &program, &commit.steps()).await?.is_none()
    {
        return Ok(());
    }
    match write {
        ExecRootWrite::Put { exec, expected, .. } => {
            // Refused: read the revision it found, for the refusal's account.
            let found: Option<i64> = crate::observed_sql::query_scalar(SQL.root.sql())
                .bind(exec.stored())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Err(conflict(exec, *expected, found.map(revision).transpose()?))
        }
        ExecRootWrite::Retire { exec, .. } => Err(conflict(exec, None, None)),
        ExecRootWrite::Clear { .. } => Err(corrupt("exec root clear", "refused a delete")),
    }
}

pub(super) fn row_of(
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
    let root: String = crate::keys::get(&row, 1).map_err(sqlx_failure)?;
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
    under_limited(tx, prefix, None).await
}

pub(super) async fn under_limited(
    tx: &mut PgConnection,
    prefix: &str,
    max_rows: Option<usize>,
) -> Result<Vec<ExecRootRow>, DurableError> {
    let sql = match max_rows {
        None => std::borrow::Cow::Borrowed(SQL.under.sql()),
        Some(max_rows) => std::borrow::Cow::Owned(format!(
            "{} LIMIT {}",
            SQL.under.sql(),
            max_rows.saturating_add(1)
        )),
    };
    let rows = crate::observed_sql::query(&sql)
        .bind(prefix)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if let Some(max_rows) = max_rows.filter(|limit| rows.len() > *limit) {
        return Err(DurableError::Graph(
            lash_core_store::checkpoint_graph::GraphRefusal::DecodeLimit {
                dimension: lash_core_store::checkpoint_graph::DecodeDimension::Nodes,
                actual: rows.len() as u64,
                limit: max_rows as u64,
            },
        ));
    }
    rows.iter()
        .map(|row| {
            let key: String = row.try_get(0).map_err(sqlx_failure)?;
            let root: String = crate::keys::get(row, 2).map_err(sqlx_failure)?;
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
/// end, or one session's, at its delete. Answers every root they held: the
/// caller's to peel.
pub(super) async fn retire_under(
    tx: &mut PgConnection,
    prefix: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let mut roots: Vec<String> = crate::keys::query_scalar(SQL.clear_under.sql())
        .bind(prefix)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    roots.extend(super::graph::clear_admissions_under(&mut *tx, prefix).await?);
    roots.extend(super::run_records::delete_under(tx, prefix).await?);
    Ok(roots)
}
