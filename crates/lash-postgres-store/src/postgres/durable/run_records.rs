//! Run records on PostgreSQL: the `run_records` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L4 (FIG-5174). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, Ordinal, OwnerKey, RunRecordKind, RunRecordRow, RunRecordWrite, RunSeq,
};
use lash_durable::{DurableError, Epoch};
use lash_sansio::ToolCallId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::run_records::RunRecordStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, corrupt, sqlx_failure};

static SQL: LazyLock<RunRecordStatements> =
    LazyLock::new(|| RunRecordStatements::render(Dialect::postgres()));

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &RunRecordWrite,
) -> Result<(), DurableError> {
    match write {
        RunRecordWrite::Append {
            owner,
            run,
            ordinal,
            kind,
            call,
            record_json,
        } => {
            let owner_key = owner.stored();
            let taken: Option<i32> = sqlx::query_scalar(SQL.ordinal_taken.sql())
                .bind(&owner_key)
                .bind(signed(run.0))
                .bind(signed(ordinal.0))
                .fetch_optional(&mut *tx)
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
                let follows: Option<i32> = sqlx::query_scalar(SQL.ordinal_taken.sql())
                    .bind(&owner_key)
                    .bind(signed(run.0))
                    .bind(signed(previous))
                    .fetch_optional(&mut *tx)
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
            if *kind == RunRecordKind::XOutcome
                && let Some(call) = call
            {
                let exists: Option<i32> = sqlx::query_scalar(SQL.outcome_exists.sql())
                    .bind(&owner_key)
                    .bind(signed(run.0))
                    .bind(call.as_str())
                    .fetch_optional(&mut *tx)
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
            sqlx::query(SQL.append.sql())
                .bind(&owner_key)
                .bind(signed(run.0))
                .bind(signed(ordinal.0))
                .bind(kind.as_str())
                .bind(call.as_ref().map(ToolCallId::as_str))
                .bind(record_json)
                .bind(commit.epoch.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        RunRecordWrite::Prune { owner, before } => {
            sqlx::query(SQL.prune.sql())
                .bind(owner.stored())
                .bind(signed(before.0))
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

pub(super) async fn read(
    tx: &mut PgConnection,
    owner: &OwnerKey,
) -> Result<Vec<RunRecordRow>, DurableError> {
    let rows = sqlx::query(SQL.read.sql())
        .bind(owner.stored())
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    rows.iter()
        .map(|row| {
            let run: i64 = row.try_get(0).map_err(sqlx_failure)?;
            let ordinal: i64 = row.try_get(1).map_err(sqlx_failure)?;
            let kind: String = row.try_get(2).map_err(sqlx_failure)?;
            let call: Option<String> = row.try_get(3).map_err(sqlx_failure)?;
            let kind =
                RunRecordKind::parse(&kind).ok_or_else(|| corrupt("run record kind", &kind))?;
            let call = call
                .map(|call| ToolCallId::parse(&call).map_err(|_| corrupt("tool call id", &call)))
                .transpose()?;
            Ok(RunRecordRow {
                owner: owner.clone(),
                run: RunSeq(u64::try_from(run).unwrap_or(0)),
                ordinal: Ordinal(u64::try_from(ordinal).unwrap_or(0)),
                kind,
                call,
                record_json: row.try_get(4).map_err(sqlx_failure)?,
                written_epoch: Epoch(row.try_get(5).map_err(sqlx_failure)?),
            })
        })
        .collect()
}
