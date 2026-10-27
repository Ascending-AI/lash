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
use lash_protocol_types::ToolCallId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::run_records::RunRecordStatements;
use sqlx::{PgConnection, Row};

use super::steps::{self, Entry, Shape, Step, Ty, Value};
use super::{Committing, corrupt, integer, node_ref, sqlx_failure};

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    struct RunFactBatchStatements @ "durable_run_fact_batch_postgres" {
        /// Validate the batch's ordinal chain and outcome uniqueness against
        /// both stored rows and its earlier rows before inserting the set.
        append = "WITH incoming AS MATERIALIZED (SELECT position AS pos, value->>0 AS owner_key, (value->>1)::bigint AS run_seq, (value->>2)::bigint AS ordinal, value->>3 AS kind, value->>4 AS call_id, decode(value->>5, 'hex') AS record_ref, (value->>6)::bigint AS written_epoch FROM jsonb_array_elements(?1::jsonb) WITH ORDINALITY AS batch(value, position))
             INSERT INTO run_records (owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch)
             SELECT owner_key, run_seq, ordinal, kind, call_id, record_ref, written_epoch FROM incoming
             WHERE NOT EXISTS (
                 SELECT 1 FROM incoming AS item WHERE
                   EXISTS (SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                     AND held.run_seq = item.run_seq AND held.ordinal = item.ordinal)
                   OR EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                     AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                     AND earlier.ordinal = item.ordinal)
                   OR (item.ordinal <> 0 AND NOT EXISTS (
                     SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                       AND held.run_seq = item.run_seq AND held.ordinal = item.ordinal - 1)
                     AND NOT EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                       AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                       AND earlier.ordinal = item.ordinal - 1))
                   OR (item.kind = 'x_outcome' AND item.call_id IS NOT NULL AND (
                     EXISTS (SELECT 1 FROM run_records AS held WHERE held.owner_key = item.owner_key
                       AND held.run_seq = item.run_seq AND held.kind = 'x_outcome' AND held.call_id = item.call_id)
                     OR EXISTS (SELECT 1 FROM incoming AS earlier WHERE earlier.pos < item.pos
                       AND earlier.owner_key = item.owner_key AND earlier.run_seq = item.run_seq
                       AND earlier.kind = 'x_outcome' AND earlier.call_id = item.call_id)))
             ) ORDER BY pos ON CONFLICT DO NOTHING RETURNING ordinal";
    }
}
static BATCH: LazyLock<RunFactBatchStatements> =
    LazyLock::new(|| RunFactBatchStatements::render(Dialect::postgres()));

fn append_rows(epoch: Epoch, writes: &[&RunFactWrite]) -> Result<String, DurableError> {
    let mut rows = Vec::with_capacity(writes.len());
    for write in writes {
        let RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        } = write
        else {
            return Err(corrupt("run fact batch", "contains a prune"));
        };
        rows.push(serde_json::json!([
            owner.stored(),
            integer::<i64>(run.0)?,
            integer::<i64>(ordinal.0)?,
            kind.as_str(),
            call.as_ref().map(ToolCallId::as_str),
            record.to_hex(),
            epoch.0
        ]));
    }
    serde_json::to_string(&rows).map_err(|error| corrupt("run fact batch", &error.to_string()))
}

/// The run-fact statements an owner commit runs as steps.
pub(super) static APPEND: Entry = Entry::new(|| &BATCH.append, &[Ty::Text], Shape::Returning);

pub(super) fn append_step(epoch: Epoch, writes: &[&RunFactWrite]) -> Result<Step, DurableError> {
    Ok(Step::new(
        &APPEND,
        vec![Value::text(append_rows(epoch, writes)?)],
        Some(writes.len() as u64),
    ))
}
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
        RunFactWrite::Append { .. } => vec![append_step(epoch, &[write])?],
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
                dimension: lash_kernel_value::tokens::DecodeDimension::Nodes,
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
            let record: String = crate::keys::get(row, 4).map_err(sqlx_failure)?;
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
    crate::keys::query_scalar(SQL.delete_owner.sql())
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
    crate::keys::query_scalar(SQL.delete_under.sql())
        .bind(key)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}
