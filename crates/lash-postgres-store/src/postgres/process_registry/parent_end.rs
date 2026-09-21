//! The parent-end ledger: one row per ended parent scope.
//!
//! The ledger is keyed by the scope itself — `(kind, id)` — not by a process
//! row. A turn-scoped parent has no process row, and a process-scoped parent's
//! row may be pruned before its children settle, so a foreign key onto
//! `lash_processes` cannot express the fact this table records.

use std::num::NonZeroUsize;

use lash_core::{ParentEndPlan, ParentScope, PluginError, ProcessRecord};
use lash_sansio::ProcessId;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::process_sql::process_sql;
use crate::{plugin_sqlx_error, process_decode_error};

/// The storage key for a parent scope, refusing `Host`.
///
/// `Host` never ends within a process's lifetime, so there is no ledger row to
/// write and no sweep to run.
fn ledger_key(parent: &ParentScope) -> Result<(&'static str, String), PluginError> {
    match parent.storage_id() {
        Some(id) => Ok((parent.storage_kind(), id)),
        None => Err(PluginError::Session(
            "the host parent scope never ends and has no parent-end ledger row".to_string(),
        )),
    }
}

fn decode_plan(
    kind: String,
    id: String,
    ended_at_ms: i64,
    settled_at_ms: Option<i64>,
) -> Result<ParentEndPlan, PluginError> {
    let parent = ParentScope::from_storage(&kind, Some(id.as_str())).ok_or_else(|| {
        PluginError::Session(format!("unreadable parent-end ledger key `{kind}`/`{id}`"))
    })?;
    Ok(ParentEndPlan {
        parent,
        ended_at_ms: ended_at_ms.max(0) as u64,
        settled_at_ms: settled_at_ms.map(|value| value.max(0) as u64),
    })
}

/// Serialize every decision about one parent scope at a stable advisory-lock
/// key, held for the caller's transaction.
///
/// PostgreSQL is the only tier where registration and the ledger write are
/// concurrent: SQLite serializes both through one write flow and the in-memory
/// registry through one transaction mutex. Without this lock the fence is a
/// check-then-act under READ COMMITTED — a `Cancel` child reads "no row",
/// the ledger row commits, the sweep pages children without seeing the
/// uncommitted child, settles the row, and the child then commits live with an
/// ended scope that no later pass revisits. Taking the lock in registration,
/// in the ledger write and in settle orders those two writes: the child either
/// commits before the row and is swept, or sees the row and is refused.
pub(crate) async fn lock_parent_scope_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ParentScope,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(format!("lash-parent-end:{kind}:{id}"))
    .execute(&mut **tx)
    .await
    .map(drop)
    .map_err(plugin_sqlx_error)
}

pub(crate) async fn record_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ParentScope,
    ended_at_ms: u64,
) -> Result<(), PluginError> {
    lock_parent_scope_tx(tx, parent).await?;
    let (kind, id) = ledger_key(parent)?;
    sqlx::query(process_sql().plan.insert_if_absent.sql())
        .bind(kind)
        .bind(id)
        .bind(ended_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)
}

/// Whether a ledger row exists for this scope, settled or not.
pub(crate) async fn plan_exists_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ParentScope,
) -> Result<bool, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let row = sqlx::query(process_sql().plan.exists.sql())
        .bind(kind)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(row.is_some())
}

/// The standalone ledger write, which a turn takes after its own commit.
///
/// It runs in its own transaction so it can hold the parent-scope advisory
/// lock: a registration deciding the same scope either commits its child
/// before this row exists, or reads the row and refuses the child.
pub(super) async fn record(
    pool: &PgPool,
    parent: &ParentScope,
    ended_at_ms: u64,
) -> Result<(), PluginError> {
    let mut tx = pool.begin().await.map_err(plugin_sqlx_error)?;
    record_tx(&mut tx, parent, ended_at_ms).await?;
    tx.commit().await.map_err(plugin_sqlx_error)
}

pub(super) async fn list_pending(
    pool: &PgPool,
    limit: NonZeroUsize,
) -> Result<Vec<ParentEndPlan>, PluginError> {
    let rows = sqlx::query(process_sql().plan.list_pending.sql())
        .bind(limit.get() as i64)
        .fetch_all(pool)
        .await
        .map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| decode_plan(row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect()
}

pub(super) async fn get(
    pool: &PgPool,
    parent: &ParentScope,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let row = sqlx::query(process_sql().plan.select_stamps.sql())
        .bind(kind)
        .bind(id.as_str())
        .fetch_optional(pool)
        .await
        .map_err(plugin_sqlx_error)?;
    row.map(|row| decode_plan(kind.to_string(), id, row.get(0), row.get(1)))
        .transpose()
}

/// Turn scopes with live `Cancel` children and no ledger row yet.
///
/// A turn's ledger row is written right after the turn commit rather than
/// inside it, so a crash in between leaves exactly this shape: children that
/// still name a turn scope no row has ended. The recovery sweep confirms the
/// turn actually committed before writing the row, so a turn interrupted
/// mid-flight is reported here and then left alone for its redrive.
///
/// The predicate is the pending-cancel partial index, so a scope whose
/// children are all terminal or already cancelled needs no row and is not
/// reported.
pub(super) async fn list_unrecorded_turn_parents(
    pool: &PgPool,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ParentScope>, PluginError> {
    let rows = sqlx::query(
        process_sql()
            .process_postgres
            .list_unrecorded_turn_parents
            .sql(),
    )
    .bind(after)
    .bind(limit.get() as i64)
    .fetch_all(pool)
    .await
    .map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let id: String = row.get(0);
            ParentScope::from_storage("turn", Some(id.as_str()))
                .ok_or_else(|| PluginError::Session(format!("unreadable turn parent scope `{id}`")))
        })
        .collect()
}

/// Children of one ended parent scope that still owe a cancel.
///
/// The predicate is exactly the pending-cancel partial index: Cancel policy,
/// no cancel request yet, and a live status. `caller_departed` is excluded for
/// the reason it is excluded from every other worklist — lash may never act on
/// such a row nor assert an outcome for it, and a cancel request is both.
pub(super) async fn children(
    pool: &PgPool,
    parent: &ParentScope,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let rows = sqlx::query(
        process_sql()
            .process_postgres
            .list_parent_end_children
            .sql(),
    )
    .bind(kind)
    .bind(id)
    .bind(after.map(|value| value.to_string()))
    .bind(limit.get() as i64)
    .fetch_all(pool)
    .await
    .map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let json: String = row.get(0);
            serde_json::from_str(&json).map_err(process_decode_error)
        })
        .collect()
}

/// Mark one ledger row settled, under the parent-scope advisory lock.
///
/// A registration that read "no row" and has not committed yet still holds the
/// lock, so settle waits for it and the child it commits is already visible to
/// the sweep that follows.
pub(super) async fn settle(
    pool: &PgPool,
    parent: &ParentScope,
    settled_at_ms: u64,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let mut tx = pool.begin().await.map_err(plugin_sqlx_error)?;
    lock_parent_scope_tx(&mut tx, parent).await?;
    sqlx::query(process_sql().plan.settle.sql())
        .bind(kind)
        .bind(id)
        .bind(settled_at_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)
}
