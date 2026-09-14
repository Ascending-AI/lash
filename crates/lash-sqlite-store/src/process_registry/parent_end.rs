//! The parent-end ledger: one row per ended parent scope.
//!
//! The ledger is keyed by the scope itself — `(kind, id)` — not by a process
//! row. A turn-scoped parent has no process row, and a process-scoped parent's
//! row may be pruned before its children settle, so a foreign key onto
//! `processes` cannot express the fact this table records.

use std::num::NonZeroUsize;
use std::sync::LazyLock;

use lash_core::{ParentEndPlan, ParentScope, PluginError, ProcessRecord};
use lash_sansio::ProcessId;
use rusqlite::{Connection, OptionalExtension, params};

use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};
use crate::process_lifecycle_sql::live_process_status;

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

/// Settled ledger rows the retention horizon has passed and no live child
/// still names.
///
/// The row has to outlive its scope — it is what refuses a late `Cancel`
/// child — so it is reclaimed by retention rather than by the sweep that
/// settles it. Past the same cutoff the process rows themselves are pruned
/// under, a settled scope with no live child can no longer parent anything
/// lash will act on, and without this the table grows by one row per committed
/// turn forever. A `caller_departed` child is not live by construction: lash
/// may never act on such a row, so it can never need a parent-end cancel.
pub(crate) static RECLAIMABLE_PLANS_DELETE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "DELETE FROM parent_end_plans
         WHERE settled_at_ms IS NOT NULL
           AND settled_at_ms < ?1
           AND NOT EXISTS (
               SELECT 1 FROM processes AS child
               WHERE child.parent_scope_kind = parent_end_plans.parent_kind
                 AND child.parent_scope_id = parent_end_plans.parent_id
                 AND {live}
           )",
        live = live_process_status("child.status")
    )
});

pub(crate) fn reclaim_settled_plans_conn(
    conn: &Connection,
    cutoff: i64,
) -> Result<usize, PluginError> {
    conn.execute(&RECLAIMABLE_PLANS_DELETE, params![cutoff])
        .map_err(process_sqlite_error)
}

pub(super) fn record_conn(
    conn: &Connection,
    parent: &ParentScope,
    ended_at_ms: u64,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent)?;
    conn.execute(
        "INSERT INTO parent_end_plans (parent_kind, parent_id, ended_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (parent_kind, parent_id) DO NOTHING",
        params![kind, id, ended_at_ms as i64],
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
    conn.query_row(
        "SELECT 1 FROM parent_end_plans WHERE parent_kind = ?1 AND parent_id = ?2",
        params![kind, id],
        |_| Ok(()),
    )
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
    ended: i64,
    settled: Option<i64>,
) -> Result<ParentEndPlan, PluginError> {
    let parent = ParentScope::from_storage(&kind, Some(id.as_str())).ok_or_else(|| {
        PluginError::Session(format!("unreadable parent-end ledger key `{kind}`/`{id}`"))
    })?;
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
            let mut statement = conn.prepare(
                "SELECT parent_kind, parent_id, ended_at_ms, settled_at_ms
                 FROM parent_end_plans
                 WHERE settled_at_ms IS NULL
                 ORDER BY ended_at_ms, parent_kind, parent_id
                 LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit.get() as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|(kind, id, ended, settled)| decode_plan(kind, id, ended, settled))
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
                "SELECT ended_at_ms, settled_at_ms FROM parent_end_plans
                 WHERE parent_kind = ?1 AND parent_id = ?2",
                params![lookup.0, lookup.1],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()
        })
        .await
        .map_err(process_sqlite_error)?;
    row.map(|(ended, settled)| decode_plan(kind.to_string(), id, ended, settled))
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
pub(crate) static UNRECORDED_TURN_PARENTS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT DISTINCT child.parent_scope_id FROM processes AS child
                 WHERE child.parent_scope_kind = 'turn'
                   AND child.on_parent_end = 'cancel'
                   AND child.cancel_requested_at_ms IS NULL
                   AND {live}
                   AND NOT EXISTS (
                       SELECT 1 FROM parent_end_plans AS plan
                       WHERE plan.parent_kind = 'turn'
                         AND plan.parent_id = child.parent_scope_id
                   )
                   AND (?1 IS NULL OR child.parent_scope_id > ?1)
                 ORDER BY child.parent_scope_id
                 LIMIT ?2",
        live = live_process_status("child.status")
    )
});

pub(super) async fn list_unrecorded_turn_parents(
    registry: &SqliteProcessRegistry,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ParentScope>, PluginError> {
    let after = after.map(str::to_string);
    let ids = registry
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(&UNRECORDED_TURN_PARENTS_SQL)?;
            let rows = statement.query_map(params![after, limit.get() as i64], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    ids.into_iter()
        .map(|id| {
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
pub(crate) static PARENT_END_CHILDREN_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT record_json FROM processes
     WHERE parent_scope_kind = ?1
       AND parent_scope_id = ?2
       AND on_parent_end = 'cancel'
       AND cancel_requested_at_ms IS NULL
       AND {live}
       AND (?3 IS NULL OR process_id > ?3)
     ORDER BY process_id ASC
     LIMIT ?4",
        live = live_process_status("status")
    )
});

pub(super) fn children_conn(
    conn: &Connection,
    parent: &ParentScope,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (kind, id) = ledger_key(parent)?;
    let after = after.map(|value| value.to_string());
    let mut statement = conn
        .prepare(&PARENT_END_CHILDREN_SQL)
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
                    "UPDATE parent_end_plans SET settled_at_ms = ?3
                     WHERE parent_kind = ?1 AND parent_id = ?2
                       AND settled_at_ms IS NULL",
                    params![kind, id, settled_at_ms as i64],
                )
                .map_err(process_sqlite_error)
                .map(|_| ()),
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}
