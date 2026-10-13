//! Run facts on PostgreSQL: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, Ordinal, OwnerKey, RunFactKind, RunFactRow, RunFactWrite, RunSeq,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::ToolCallId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::run_records::RunRecordStatements;
use sqlx::{PgConnection, Row};

use super::steps::{self, Entry, Shape, Step, Ty, Value};
use super::{Committing, corrupt, integer, node_ref, sqlx_failure};

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(Dialect::postgres()));

/// The run-fact statements an owner commit runs as steps.
pub(super) static APPEND: Entry = Entry::new(
    || &SQL.append,
    &[
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
    ],
    Shape::Returning,
);
pub(super) static DELETE_OWNER: Entry =
    Entry::new(|| &SQL.delete_owner, &[Ty::Text], Shape::Returning).retiring("record_ref");
/// Every fact of the owners under a key: a terminal's cells.
pub(super) static DELETE_UNDER: Entry =
    Entry::new(|| &SQL.delete_under, &[Ty::Text], Shape::Returning).retiring("record_ref");
pub(super) static PRUNE: Entry =
    Entry::new(|| &SQL.prune, &[Ty::Text, Ty::Int], Shape::Returning).retiring("record_ref");

/// `write`'s step program at `epoch`: an append is one conditional insert
/// that must answer its row, a prune one delete.
pub(super) fn steps(epoch: Epoch, write: &RunFactWrite) -> Result<Option<Vec<Step>>, DurableError> {
    Ok(Some(match write {
        RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        } => vec![Step::one(
            &APPEND,
            vec![
                Value::text(owner.stored()),
                Value::int(integer::<i64>(run.0)?),
                Value::int(integer::<i64>(ordinal.0)?),
                Value::text(kind.as_str()),
                Value::Text(call.as_ref().map(|call| call.as_str().to_owned())),
                Value::text(record.to_hex()),
                Value::int(epoch.0),
            ],
        )],
        RunFactWrite::Prune { owner, before } => vec![Step::any(
            &PRUNE,
            vec![
                Value::text(owner.stored()),
                Value::int(integer::<i64>(before.0)?),
            ],
        )],
    }))
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &RunFactWrite,
) -> Result<(), DurableError> {
    let program = steps(commit.epoch, write)?.unwrap_or_default();
    if steps::run(tx, &program, &commit.steps()).await?.is_none() {
        return Ok(());
    }
    match write {
        RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            ..
        } => {
            let call = call.as_ref().filter(|_| *kind == RunFactKind::XOutcome);
            Err(refused(tx, owner, *run, *ordinal, call).await?)
        }
        RunFactWrite::Prune { .. } => Err(corrupt("run fact prune", "refused a delete")),
    }
}

/// Whether owner `owner_key`'s run `run` has a fact at `ordinal`.
async fn ordinal_taken(
    tx: &mut PgConnection,
    owner_key: &str,
    run: RunSeq,
    ordinal: u64,
) -> Result<bool, DurableError> {
    let found: Option<i32> = crate::observed_sql::query_scalar(SQL.ordinal_taken.sql())
        .bind(owner_key)
        .bind(integer::<i64>(run.0)?)
        .bind(integer::<i64>(ordinal)?)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(found.is_some())
}

/// Why the append of `ordinal` of `owner`'s run `run` (for `call`, when
/// an outcome) was refused, read only once it was, in the refusal's order:
/// its ordinal taken, then the fact before it absent, then its call's
/// outcome recorded.
async fn refused(
    tx: &mut PgConnection,
    owner: &OwnerKey,
    run: RunSeq,
    ordinal: Ordinal,
    call: Option<&ToolCallId>,
) -> Result<DurableError, DurableError> {
    let owner_key = owner.stored();
    if ordinal_taken(tx, &owner_key, run, ordinal.0).await? {
        return Ok(DurableError::Domain(DomainRefusal::RunOrdinalTaken {
            owner: owner.clone(),
            run,
            ordinal,
        }));
    }
    if let Some(previous) = ordinal.0.checked_sub(1)
        && !ordinal_taken(tx, &owner_key, run, previous).await?
    {
        return Ok(DurableError::Domain(DomainRefusal::RunOrdinalGap {
            owner: owner.clone(),
            run,
            ordinal,
        }));
    }
    match call {
        Some(call) => Ok(DurableError::Domain(DomainRefusal::OutcomeExists {
            owner: owner.clone(),
            run,
            call: call.clone(),
        })),
        None => Err(corrupt("run fact append", "refused for no recorded reason")),
    }
}

pub(super) async fn read_limited(
    tx: &mut PgConnection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
    max_rows: Option<usize>,
) -> Result<Vec<RunFactRow>, DurableError> {
    let original = if run.is_some() {
        SQL.read_of.sql()
    } else {
        SQL.read.sql()
    };
    let sql = match max_rows {
        None => std::borrow::Cow::Borrowed(original),
        Some(_) => std::borrow::Cow::Owned(format!(
            "{original} LIMIT ${}",
            if run.is_some() { 3 } else { 2 }
        )),
    };
    let mut query = crate::observed_sql::query(&sql).bind(owner.stored());
    if let Some(run) = run {
        query = query.bind(integer::<i64>(run.0)?);
    }
    if let Some(max_rows) = max_rows {
        query = query.bind(integer::<i64>(max_rows.saturating_add(1))?);
    }
    let rows = query
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
            let run: i64 = row.try_get(0).map_err(sqlx_failure)?;
            let ordinal: i64 = row.try_get(1).map_err(sqlx_failure)?;
            let kind: String = row.try_get(2).map_err(sqlx_failure)?;
            let call: Option<String> = row.try_get(3).map_err(sqlx_failure)?;
            let kind =
                RunFactKind::parse(&kind).ok_or_else(|| corrupt("run record kind", &kind))?;
            let call = call
                .map(|call| ToolCallId::parse(&call).map_err(|_| corrupt("tool call id", &call)))
                .transpose()?;
            let record: String = row.try_get(4).map_err(sqlx_failure)?;
            Ok(RunFactRow {
                owner: owner.clone(),
                run: RunSeq(integer::<u64>(run)?),
                ordinal: Ordinal(integer::<u64>(ordinal)?),
                kind,
                call,
                record: node_ref(&record)?,
                written_epoch: Epoch(row.try_get(5).map_err(sqlx_failure)?),
            })
        })
        .collect()
}

/// Delete every fact of the owner stored as `key`, answering the RoundFact
/// nodes they held: the caller's to peel.
pub(super) async fn delete_owner(
    tx: &mut PgConnection,
    key: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.delete_owner.sql())
        .bind(key)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}

/// Delete every fact whose owner key starts with `key`, answering the
/// RoundFact nodes they held: the caller's to peel.
pub(super) async fn delete_under(
    tx: &mut PgConnection,
    key: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.delete_under.sql())
        .bind(key)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}
