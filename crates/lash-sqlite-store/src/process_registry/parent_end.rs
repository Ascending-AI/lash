//! The scope-close ledger: one row per closed scope (FIG-3607 R9).
//!
//! The ledger is keyed by the scope itself — `(kind, id)` — not by a process
//! row. A turn root or a session has no process row, and a process scope's row
//! may be pruned before its children settle, so a foreign key onto
//! `processes` cannot express the fact this table records.

use std::num::NonZeroUsize;

use lash_core_execution::{ParentEndPlan, PluginError, ProcessRecord, ScopeId};
use lash_sansio::ProcessId;
use rusqlite::{Connection, OptionalExtension, params};

use super::sql::process_sql;
use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};

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
fn ledger_payload(
    parent: &ScopeId,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<String, PluginError> {
    parent
        .storage_payload(fleet_format)
        .map_err(process_decode_error)
}

/// The row `parent`'s record writes is also its `ParentEnd` obligation, due
/// immediately (ADR 0109 §3): the record arms it in the same transaction, so
/// no crash window can leave a plan nothing will ever deliver. A replayed
/// record keeps the first row — and the obligation it already owes.
pub(super) fn record_conn(
    conn: &Connection,
    parent: &ScopeId,
    ended_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent);
    conn.execute(
        process_sql().plan.insert_if_absent.sql(),
        params![
            kind,
            id,
            ledger_payload(parent, fleet_format)?,
            ended_at_ms as i64
        ],
    )
    .map_err(process_sqlite_error)?;
    crate::obligation_ledger::arm_obligation_tx(
        conn,
        &lash_core_execution::store::ObligationKey::ParentEnd {
            parent_kind: kind.to_string(),
            parent_id: id,
        },
        ended_at_ms,
    )
    .map_err(|error| PluginError::Session(error.to_string()))?;
    Ok(())
}

/// Whether a ledger row exists for this scope, settled or not.
///
/// Registration reads this inside its own transaction to fence a late start
/// (FIG-3607 R11).
pub(super) fn plan_exists_conn(conn: &Connection, parent: &ScopeId) -> Result<bool, PluginError> {
    let (kind, id) = ledger_key(parent);
    conn.query_row(process_sql().plan.exists.sql(), params![kind, id], |_| {
        Ok(())
    })
    .optional()
    .map(|row| row.is_some())
    .map_err(process_sqlite_error)
}

pub(super) async fn record(
    registry: &SqliteProcessRegistry,
    parent: &ScopeId,
) -> Result<(), PluginError> {
    let parent = parent.clone();
    let ended_at_ms = registry.clock.timestamp_ms();
    let fleet_format = registry.fleet_format;
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome(record_conn(
                tx,
                &parent,
                ended_at_ms,
                fleet_format,
            )))
        })
        .await
        .map_err(process_sqlite_error)?
}

/// One stored row's plan. A row whose typed payload does not decode is
/// corrupt stored data: the obligation relay stalls it `undecodable` rather
/// than failing its due page (ADR 0109 §1.4).
#[allow(clippy::too_many_arguments)]
fn decode_plan(
    kind: String,
    id: String,
    payload: String,
    ended: i64,
    settled: Option<i64>,
    obligation_id: Option<String>,
    obligation_state: Option<String>,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<ParentEndPlan, PluginError> {
    let parent =
        ScopeId::from_storage_columns(&kind, &id, &payload, fleet_format).map_err(|error| {
            PluginError::StoredDataCorrupt {
                record_kind: "parent_end_plan".to_string(),
                message: error.to_string(),
            }
        })?;
    let obligation_state = obligation_state
        .map(|label| {
            lash_core_execution::store::ObligationState::from_label(&label).map_err(|error| {
                PluginError::StoredDataCorrupt {
                    record_kind: "parent_end_plan".to_string(),
                    message: error.to_string(),
                }
            })
        })
        .transpose()?;
    Ok(ParentEndPlan {
        parent,
        ended_at_ms: ended.max(0) as u64,
        settled_at_ms: settled.map(|value| value.max(0) as u64),
        obligation_id: obligation_id.map(lash_core_execution::store::ObligationId::new),
        obligation_state,
    })
}

/// The row `(kind, id)` names, as the read any caller gets: `get` names it
/// by scope, `get_by_key` by the stored columns a `ParentEnd` obligation's
/// claim carries.
async fn get_by_columns(
    registry: &SqliteProcessRegistry,
    kind: String,
    id: String,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let lookup = (kind, id);
    let inside = lookup.clone();
    let row = registry
        .conn
        .call(move |conn| {
            conn.query_row(
                process_sql().plan.select_stamps.sql(),
                params![inside.0, inside.1],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
        })
        .await
        .map_err(process_sqlite_error)?;
    row.map(
        |(payload, ended, settled, obligation_id, obligation_state)| {
            decode_plan(
                lookup.0.clone(),
                lookup.1.clone(),
                payload,
                ended,
                settled,
                obligation_id,
                obligation_state,
                registry.fleet_format,
            )
        },
    )
    .transpose()
}

pub(super) async fn get(
    registry: &SqliteProcessRegistry,
    parent: &ScopeId,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let (kind, id) = ledger_key(parent);
    get_by_columns(registry, kind.to_string(), id).await
}

/// The row a `ParentEnd` obligation's claim names. Its claim reads only the
/// stored key columns, so this read is what decodes the typed payload —
/// and a payload that does not decode is corrupt, not a page failure.
pub(super) async fn get_by_key(
    registry: &SqliteProcessRegistry,
    parent_kind: &str,
    parent_id: &str,
) -> Result<Option<ParentEndPlan>, PluginError> {
    get_by_columns(registry, parent_kind.to_string(), parent_id.to_string()).await
}

/// Turn and queue-drain scopes with live `Until` children and no ledger row
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
) -> Result<Vec<ScopeId>, PluginError> {
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
            let parent = record.lifetime.scope().cloned();
            parent
                .filter(|parent| {
                    matches!(parent.storage_kind(), "turn" | "queue_drain")
                        && parent.storage_kind() == kind
                        && parent.storage_id() == id
                })
                .ok_or_else(|| {
                    PluginError::Session(format!(
                        "opener parent-scope candidate `{id}` names a different scope in its record"
                    ))
                })
        })
        .collect()
}

/// Processes living `Until` one closed scope that still owe a cancel.
///
/// The predicate is exactly the pending-cancel partial index: `Until` lifetime,
/// no cancel request yet, and a live status. `caller_departed` is excluded for
/// the reason it is excluded from every other worklist — lash may never act on
/// such a row nor assert an outcome for it, and a cancel request is both.
pub(super) fn children_conn(
    conn: &Connection,
    parent: &ScopeId,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (kind, id) = ledger_key(parent);
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
    parent: &ScopeId,
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

/// Settle the plan — and deliver the `due` obligation the same row owes:
/// the apply that ends here is the delivery that obligation carries
/// (ADR 0109). A `claimed` row's claim owns its own settle, and a `stalled`
/// row keeps its stall for the operator.
pub(super) async fn settle(
    registry: &SqliteProcessRegistry,
    parent: &ScopeId,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent);
    let kind = kind.to_string();
    let settled_at_ms = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                tx.execute(
                    process_sql().plan.settle.sql(),
                    params![kind, id.clone(), settled_at_ms as i64],
                )
                .map_err(process_sqlite_error)?;
                tx.execute(
                    process_sql().plan.obligation_apply_delivered.sql(),
                    params![kind, id, settled_at_ms as i64],
                )
                .map_err(process_sqlite_error)?;
                Ok(())
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
