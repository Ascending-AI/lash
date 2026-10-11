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
            let taken: Option<i32> = crate::observed_sql::query_scalar(SQL.ordinal_taken.sql())
                .bind(&owner_key)
                .bind(integer::<i64>(run.0)?)
                .bind(integer::<i64>(ordinal.0)?)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if taken.is_some() {
                return Err(DurableError::Domain(DomainRefusal::RunOrdinalTaken {
                    owner: owner.clone(),
                    run: *run,
                    ordinal: *ordinal,
                }));
            }
            if let Some(previous) = ordinal.0.checked_sub(1) {
                let follows: Option<i32> =
                    crate::observed_sql::query_scalar(SQL.ordinal_taken.sql())
                        .bind(&owner_key)
                        .bind(integer::<i64>(run.0)?)
                        .bind(integer::<i64>(previous)?)
                        .fetch_optional(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                if follows.is_none() {
                    return Err(DurableError::Domain(DomainRefusal::RunOrdinalGap {
                        owner: owner.clone(),
                        run: *run,
                        ordinal: *ordinal,
                    }));
                }
            }
            if *kind == RunFactKind::XOutcome
                && let Some(call) = call
            {
                let exists: Option<i32> =
                    crate::observed_sql::query_scalar(SQL.outcome_exists.sql())
                        .bind(&owner_key)
                        .bind(integer::<i64>(run.0)?)
                        .bind(call.as_str())
                        .fetch_optional(crate::observed_sql::executor(&mut *tx))
                        .await
                        .map_err(sqlx_failure)?;
                if exists.is_some() {
                    return Err(DurableError::Domain(DomainRefusal::OutcomeExists {
                        owner: owner.clone(),
                        run: *run,
                        call: call.clone(),
                    }));
                }
            }
            crate::observed_sql::query(SQL.append.sql())
                .bind(&owner_key)
                .bind(integer::<i64>(run.0)?)
                .bind(integer::<i64>(ordinal.0)?)
                .bind(kind.as_str())
                .bind(call.as_ref().map(ToolCallId::as_str))
                .bind(record.to_hex())
                .bind(commit.epoch.0)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
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

pub(super) async fn read(
    tx: &mut PgConnection,
    owner: &OwnerKey,
) -> Result<Vec<RunFactRow>, DurableError> {
    let rows = crate::observed_sql::query(SQL.read.sql())
        .bind(owner.stored())
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
