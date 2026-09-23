//! The parent-end ledger: one row per ended parent scope.
//!
//! The ledger is keyed by the scope itself — `(kind, id)` — not by a process
//! row. A turn-scoped parent has no process row, and a process-scoped parent's
//! row may be pruned before its children settle, so a foreign key onto
//! `processes` cannot express the fact this table records.

use std::num::NonZeroUsize;

use lash_core_execution::{ParentEndPlan, ParentScope, PluginError, ProcessRecord};
use lash_sansio::ProcessId;
use rusqlite::{Connection, OptionalExtension, params};

use super::sql::process_sql;
use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};

/// The storage key for a parent scope, refusing `Host`.
///
/// `Host` never ends within a process's lifetime, so there is no ledger row to
/// write and no sweep to run; a caller asking for one is asking a question the
/// ledger cannot answer.
fn ledger_key(parent: &ParentScope) -> Result<(&'static str, String), PluginError> {
    match parent.storage_id() {
        Some(id) => Ok((parent.storage_kind(), id)),
        None => Err(PluginError::Session(
            "the host parent scope never ends and has no parent-end ledger row".to_string(),
        )),
    }
}

/// Reclaim settled ledger rows the retention horizon has passed and no live
/// child still names.
///
/// The row has to outlive its scope — it is what refuses a late `Cancel`
/// child — so it is reclaimed by retention rather than by the sweep that
/// settles it. Past the same cutoff the process rows themselves are pruned
/// under, a settled scope with no live child can no longer parent anything
/// lash will act on, and without this the table grows by one row per committed
/// turn forever. A `caller_departed` child is not live by construction: lash
/// may never act on such a row, so it can never need a parent-end cancel.
pub(crate) fn reclaim_settled_plans_conn(
    conn: &Connection,
    cutoff: i64,
) -> Result<usize, PluginError> {
    conn.execute(
        process_sql().plan_sqlite.delete_reclaimable.sql(),
        params![cutoff],
    )
    .map_err(process_sqlite_error)
}

/// The typed payload a ledger row persists beside the projection key.
fn ledger_payload(parent: &ParentScope) -> Result<String, PluginError> {
    parent.storage_payload().map_err(process_decode_error)
}

pub(super) fn record_conn(
    conn: &Connection,
    parent: &ParentScope,
    ended_at_ms: u64,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent)?;
    conn.execute(
        process_sql().plan.insert_if_absent.sql(),
        params![kind, id, ledger_payload(parent)?, ended_at_ms as i64],
    )
    .map_err(process_sqlite_error)?;
    Ok(())
}

/// Whether a ledger row exists for this scope, settled or not.
///
/// Registration reads this inside its own transaction to fence a late
/// `Cancel` child.
pub(super) fn plan_exists_conn(
    conn: &Connection,
    parent: &ParentScope,
) -> Result<bool, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    conn.query_row(process_sql().plan.exists.sql(), params![kind, id], |_| {
        Ok(())
    })
    .optional()
    .map(|row| row.is_some())
    .map_err(process_sqlite_error)
}

pub(super) async fn record(
    registry: &SqliteProcessRegistry,
    parent: &ParentScope,
) -> Result<(), PluginError> {
    let parent = parent.clone();
    let ended_at_ms = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| Ok(tx_outcome(record_conn(tx, &parent, ended_at_ms))))
        .await
        .map_err(process_sqlite_error)?
}

fn decode_plan(
    kind: String,
    id: String,
    payload: String,
    ended: i64,
    settled: Option<i64>,
) -> Result<ParentEndPlan, PluginError> {
    let parent = ParentScope::from_storage_columns(&kind, Some(id.as_str()), &payload)
        .map_err(|error| PluginError::Session(error.to_string()))?;
    Ok(ParentEndPlan {
        parent,
        ended_at_ms: ended.max(0) as u64,
        settled_at_ms: settled.map(|value| value.max(0) as u64),
    })
}

pub(super) async fn list_pending(
    registry: &SqliteProcessRegistry,
    limit: NonZeroUsize,
) -> Result<Vec<ParentEndPlan>, PluginError> {
    let rows = registry
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(process_sql().plan.list_pending.sql())?;
            let rows = statement.query_map(params![limit.get() as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|(kind, id, payload, ended, settled)| decode_plan(kind, id, payload, ended, settled))
        .collect()
}

pub(super) async fn get(
    registry: &SqliteProcessRegistry,
    parent: &ParentScope,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let lookup = (kind.to_string(), id.clone());
    let row = registry
        .conn
        .call(move |conn| {
            conn.query_row(
                process_sql().plan.select_stamps.sql(),
                params![lookup.0, lookup.1],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()
        })
        .await
        .map_err(process_sqlite_error)?;
    row.map(|(payload, ended, settled)| decode_plan(kind.to_string(), id, payload, ended, settled))
        .transpose()
}

/// Turn and queue-drain scopes with live `Cancel` children and no ledger row
/// yet.
///
/// An opener's ledger row is written right after its end evidence rather than
/// inside it — the turn commit for a turn, the drain-end receipt for a drain —
/// so a crash in between leaves exactly this shape: children that still name
/// an owner scope no row has ended. The recovery sweep confirms the owner
/// actually ended before writing the row, so an interrupted turn or drain is
/// reported here and then left alone for its redrive.
///
/// The predicate is the pending-cancel partial index, so a scope whose
/// children are all terminal or already cancelled needs no row and is not
/// reported.
pub(super) async fn list_unrecorded_opener_parents(
    registry: &SqliteProcessRegistry,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ParentScope>, PluginError> {
    let after = after.map(str::to_string);
    let rows = registry
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(
                process_sql()
                    .process_sqlite
                    .list_unrecorded_opener_parents
                    .sql(),
            )?;
            let rows = statement.query_map(params![after, limit.get() as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|(id, kind, record_json)| {
            let record: ProcessRecord =
                serde_json::from_str(&record_json).map_err(process_decode_error)?;
            let parent = record.lifecycle.parent;
            (matches!(parent.storage_kind(), "turn" | "queue_drain")
                && parent.storage_kind() == kind
                && parent.storage_id().as_deref() == Some(id.as_str()))
            .then_some(parent)
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "opener parent-scope candidate `{id}` names a different scope in its record"
                ))
            })
        })
        .collect()
}

/// Children of one ended parent scope that still owe a cancel.
///
/// The predicate is exactly the pending-cancel partial index: Cancel policy,
/// no cancel request yet, and a live status. `caller_departed` is excluded for
/// the reason it is excluded from every other worklist — lash may never act on
/// such a row nor assert an outcome for it, and a cancel request is both.
pub(super) fn children_conn(
    conn: &Connection,
    parent: &ParentScope,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let after = after.map(|value| value.to_string());
    let mut statement = conn
        .prepare(process_sql().process_sqlite.list_parent_end_children.sql())
        .map_err(process_sqlite_error)?;
    let rows = statement
        .query_map(params![kind, id, after, limit.get() as i64], |row| {
            row.get::<_, String>(0)
        })
        .map_err(process_sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .collect()
}

pub(super) async fn children(
    registry: &SqliteProcessRegistry,
    parent: &ParentScope,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let parent = parent.clone();
    let after = after.cloned();
    registry
        .conn
        .call(move |conn| Ok(children_conn(conn, &parent, after.as_ref(), limit)))
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn settle(
    registry: &SqliteProcessRegistry,
    parent: &ParentScope,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let kind = kind.to_string();
    let settled_at_ms = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome(
                tx.execute(
                    process_sql().plan.settle.sql(),
                    params![kind, id, settled_at_ms as i64],
                )
                .map_err(process_sqlite_error)
                .map(|_| ()),
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}
