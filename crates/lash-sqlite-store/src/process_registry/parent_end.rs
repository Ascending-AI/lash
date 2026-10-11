//! Turn scopes derive their close from the retained run terminal. Other
//! scopes retain their typed close ledger, including after a process is pruned.

use lash_core_execution::{EffectOpener, ParentEndPlan, PluginError, ScopeId};
use rusqlite::{Connection, OptionalExtension, params};

use super::sql::process_sql;
use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error};

/// The storage key for a scope.
fn ledger_key(scope: &ScopeId) -> (&'static str, String) {
    (scope.storage_kind(), scope.storage_id())
}

/// Reclaim settled ledger rows the retention horizon has passed and no live
/// child still names.
///
/// The row has to outlive its scope — it is what refuses a late `Cancel`
/// child — so it is reclaimed by retention rather than by the sweep that
/// settles it. Past the same cutoff the process rows themselves are pruned
/// under, a settled scope with no live child can no longer parent anything
/// lash will act on, and without this the table grows by one row per committed
/// turn forever.
pub(crate) fn reclaim_settled_plans_conn(
    conn: &Connection,
    cutoff: i64,
) -> Result<usize, PluginError> {
    crate::conn::cached_execute(
        conn,
        process_sql().plan_sqlite.delete_reclaimable.sql(),
        params![cutoff],
    )
    .map_err(process_sqlite_error)
}

/// The typed payload a ledger row persists beside the projection key.
fn ledger_payload(
    parent: &ScopeId,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<String, PluginError> {
    parent
        .storage_payload(fleet_format)
        .map_err(process_decode_error)
}

/// Record that `parent` ended at `ended_at_ms`, in the caller's
/// transaction: a process's terminal append, or the durable commit that
/// closed a turn or a session (`ProcessWrite::ScopeClosed`). A repeated
/// record keeps the first row.
pub(crate) fn record_conn(
    conn: &Connection,
    parent: &ScopeId,
    ended_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    if matches!(parent.opener(), Some(EffectOpener::Turn { .. })) {
        return Ok(());
    }
    let (kind, id) = ledger_key(parent);
    crate::conn::cached_execute(
        conn,
        process_sql().plan.insert_if_absent.sql(),
        params![
            kind,
            id,
            ledger_payload(parent, fleet_format)?,
            ended_at_ms as i64
        ],
    )
    .map_err(process_sqlite_error)?;
    // The close ends every wait the scope's calls still hold (ADR 0116
    // §3.6): an abandoned call leaks no hold, and a late start under the
    // closed scope is refused above, so no redrive needs the row pinned.
    crate::conn::cached_execute(
        conn,
        process_sql().process.release_consumer_holds_owned_by.sql(),
        params![kind, id],
    )
    .map_err(process_sqlite_error)?;
    // Its abandoned holds' marks go with it: the ledger row now refuses a
    // start under the scope.
    crate::conn::cached_execute(
        conn,
        process_sql().abandoned_hold.forget_owned_by.sql(),
        params![kind, id],
    )
    .map_err(process_sqlite_error)?;
    Ok(())
}

/// Whether a ledger row exists for this scope, settled or not.
///
/// Registration reads this inside its own transaction to fence a late start
/// (FIG-3607 R11).
pub(super) fn plan_exists_conn(conn: &Connection, parent: &ScopeId) -> Result<bool, PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = parent.opener()
    {
        return conn
            .query_row(
                process_sql().plan.turn_terminal.sql(),
                params![session_id.as_str(), turn_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(process_sqlite_error);
    }
    let (kind, id) = ledger_key(parent);
    conn.query_row(process_sql().plan.exists.sql(), params![kind, id], |_| {
        Ok(())
    })
    .optional()
    .map(|row| row.is_some())
    .map_err(process_sqlite_error)
}

/// One stored row's plan. A typed payload that does not decode is corrupt
/// stored data.
fn decode_plan(
    kind: &str,
    id: &str,
    payload: &str,
    ended: i64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<ParentEndPlan, PluginError> {
    let parent =
        ScopeId::from_storage_columns(kind, id, payload, fleet_format).map_err(|error| {
            PluginError::StoredDataCorrupt {
                record_kind: "parent_end_plan".to_string(),
                message: error.to_string(),
            }
        })?;
    Ok(ParentEndPlan {
        parent,
        ended_at_ms: ended.max(0) as u64,
    })
}

pub(super) async fn get(
    registry: &SqliteProcessRegistry,
    parent: &ScopeId,
) -> Result<Option<ParentEndPlan>, PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = parent.opener()
    {
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let ended = registry
            .conn
            .call(move |conn| {
                conn.query_row(
                    process_sql().plan.turn_terminal.sql(),
                    params![session_id.as_str(), turn_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            })
            .await
            .map_err(process_sqlite_error)?;
        return Ok(ended.map(|ended| ParentEndPlan {
            parent: parent.clone(),
            ended_at_ms: ended.max(0) as u64,
        }));
    }
    let (kind, id) = ledger_key(parent);
    let inside = (kind, id.clone());
    let row = registry
        .conn
        .call(move |conn| {
            conn.query_row(
                process_sql().plan.select_stamps.sql(),
                params![inside.0, inside.1],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
        })
        .await
        .map_err(process_sqlite_error)?;
    row.map(|(payload, ended)| decode_plan(kind, &id, &payload, ended, registry.conn.fleet()))
        .transpose()
}

/// Registration/abandonment marks only turn owners; other scopes keep their ledger.
pub(crate) fn mark_holds_conn(conn: &Connection, owner: &ScopeId) -> Result<(), PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = owner.opener()
    {
        conn.prepare_cached(process_sql().plan.mark_turn_holds.sql())
            .map_err(process_sqlite_error)?
            .query_row(params![session_id.as_str(), turn_id.as_str()], |_| Ok(()))
            .optional()
            .map_err(process_sqlite_error)?;
    }
    Ok(())
}

/// The close releases only holds the scope registered.
pub(crate) fn release_holds_conn(conn: &Connection, owner: &ScopeId) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(owner);
    crate::conn::cached_execute(
        conn,
        process_sql().process.release_consumer_holds_owned_by.sql(),
        params![kind, id],
    )
    .map_err(process_sqlite_error)?;
    crate::conn::cached_execute(
        conn,
        process_sql().abandoned_hold.forget_owned_by.sql(),
        params![kind, id],
    )
    .map_err(process_sqlite_error)?;
    Ok(())
}
