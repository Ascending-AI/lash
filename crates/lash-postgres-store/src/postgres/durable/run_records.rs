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

use super::{Committing, corrupt, integer, node_ref, sqlx_failure};

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(Dialect::postgres()));

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &RunFactWrite,
) -> Result<(), DurableError> {
    match write {
        RunFactWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record,
        } => {
            let owner_key = owner.stored();
            let appended: Option<i64> = crate::observed_sql::query_scalar(SQL.append.sql())
                .bind(&owner_key)
                .bind(integer::<i64>(run.0)?)
                .bind(integer::<i64>(ordinal.0)?)
                .bind(kind.as_str())
                .bind(call.as_ref().map(ToolCallId::as_str))
                .bind(record.to_hex())
                .bind(commit.epoch.0)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if appended.is_some() {
                return Ok(());
            }
            let call = call.as_ref().filter(|_| *kind == RunFactKind::XOutcome);
            Err(refused(tx, owner, *run, *ordinal, call).await?)
        }
        RunFactWrite::Prune { owner, before } => {
            crate::observed_sql::query(SQL.prune.sql())
                .bind(owner.stored())
                .bind(integer::<i64>(before.0)?)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
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

pub(super) async fn read(
    tx: &mut PgConnection,
    owner: &OwnerKey,
    run: Option<RunSeq>,
) -> Result<Vec<RunFactRow>, DurableError> {
    let mut query = crate::observed_sql::query(if run.is_some() {
        SQL.read_of.sql()
    } else {
        SQL.read.sql()
    })
    .bind(owner.stored());
    if let Some(run) = run {
        query = query.bind(integer::<i64>(run.0)?);
    }
    let rows = query
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
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

/// Delete every fact of the owner stored as `key`.
pub(super) async fn delete_owner(tx: &mut PgConnection, key: &str) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.delete_owner.sql())
        .bind(key)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

/// Delete every fact whose owner key starts with `key`.
pub(super) async fn delete_under(tx: &mut PgConnection, key: &str) -> Result<(), sqlx::Error> {
    crate::observed_sql::query(SQL.delete_under.sql())
        .bind(key)
        .execute(crate::observed_sql::executor(tx))
        .await
        .map(drop)
}

/// The RoundFact node of every fact whose owner key starts with `prefix`.
pub(super) async fn roots_under(
    tx: &mut PgConnection,
    prefix: &str,
) -> Result<Vec<String>, sqlx::Error> {
    crate::observed_sql::query_scalar(SQL.roots_under.sql())
        .bind(prefix)
        .fetch_all(crate::observed_sql::executor(tx))
        .await
}
