//! The scope-close ledger: one row per closed scope (FIG-3607 R9).
//!
//! The ledger is keyed by the scope itself — `(kind, id)` — not by a process
//! row. A turn root or a session has no process row, and a process scope's row
//! may be pruned before its children settle, so a foreign key onto
//! `lash_processes` cannot express the fact this table records.

use std::num::NonZeroUsize;

use lash_core_execution::{EffectOpener, ParentEndPlan, PluginError, ProcessRecord, ScopeId};
use lash_sansio::ProcessId;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::process_sql::process_sql;
use crate::{plugin_sqlx_error, process_decode_error};

/// The storage key for a scope.
fn ledger_key(scope: &ScopeId) -> (&'static str, String) {
    (scope.storage_kind(), scope.storage_id())
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

/// One stored row's plan. A row whose typed payload does not decode is
/// corrupt stored data: the obligation relay stalls it `undecodable` rather
/// than failing its due page (ADR 0109 §1.4).
#[allow(clippy::too_many_arguments)]
fn decode_plan(
    kind: String,
    id: String,
    payload: String,
    ended_at_ms: i64,
    settled_at_ms: Option<i64>,
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
        ended_at_ms: ended_at_ms.max(0) as u64,
        settled_at_ms: settled_at_ms.map(|value| value.max(0) as u64),
        obligation_id: obligation_id.map(lash_core_execution::store::ObligationId::new),
        obligation_state,
    })
}

/// Serialize every decision about one parent scope at a stable advisory-lock
/// key, held for the caller's transaction.
///
/// PostgreSQL is the only tier where registration and the ledger write are
/// concurrent: SQLite serializes both through one write flow and the in-memory
/// registry through one transaction mutex. Without this lock the fence is a
/// check-then-act under READ COMMITTED — a start reads "no row",
/// the ledger row commits, the sweep pages children without seeing the
/// uncommitted child, settles the row, and the child then commits live with an
/// ended scope that no later pass revisits. Taking the lock in registration,
/// in the ledger write and in settle orders those two writes: the child either
/// commits before the row and is swept, or sees the row and is refused.
pub(crate) async fn lock_parent_scope_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ScopeId,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent);
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

/// The row `parent`'s record writes is also its `ParentEnd` obligation, due
/// immediately (ADR 0109 §3): the record arms it in the same transaction, so
/// no crash window can leave a plan nothing will ever deliver. A replayed
/// record keeps the first row — and the obligation it already owes. The
/// plan's `ended_at_ms` is the registry's database-clock instant, so the
/// obligation is due at once rather than at it: a relay whose host clock is
/// behind the database takes it in its first pass.
pub(crate) async fn record_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ScopeId,
    ended_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    lock_parent_scope_tx(tx, parent).await?;
    let (kind, id) = ledger_key(parent);
    sqlx::query(process_sql().plan.insert_if_absent.sql())
        .bind(kind)
        .bind(id.clone())
        .bind(ledger_payload(parent, fleet_format)?)
        .bind(ended_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)?;
    crate::obligation_ledger::arm_obligation_tx(
        tx,
        &lash_core_execution::store::ObligationKey::ParentEnd {
            parent_kind: kind.to_string(),
            parent_id: id.clone(),
        },
        crate::obligation_ledger::DUE_AT_ONCE_MS,
    )
    .await
    .map_err(|error| PluginError::Session(error.to_string()))?;
    // The close ends every wait the scope's calls still hold (ADR 0116
    // §3.6): an abandoned call leaks no hold, and a late start under the
    // closed scope is refused, so no redrive needs the row pinned.
    sqlx::query(process_sql().process.release_consumer_holds_owned_by.sql())
        .bind(kind)
        .bind(&id)
        .execute(&mut **tx)
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)?;
    // Its abandoned holds' marks go with it: the ledger row now refuses a
    // start under the scope.
    sqlx::query(process_sql().abandoned_hold.forget_owned_by.sql())
        .bind(kind)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)?;
    Ok(())
}

/// Serialize every decision about one consumer hold at a stable
/// advisory-lock key, held for the caller's transaction: the abandonment
/// that marks the hold and reads what it owes, and a registration that
/// checks the mark (ADR 0116 §3.4). Without it the fence is a
/// check-then-act under READ COMMITTED, and a start that read "no mark"
/// could commit after the abandonment's read missed it.
pub(crate) async fn lock_consumer_hold_tx(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
) -> Result<(), PluginError> {
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(format!("lash-consumer-hold:{key}"))
    .execute(&mut **tx)
    .await
    .map(drop)
    .map_err(plugin_sqlx_error)
}

/// Whether a ledger row exists for this scope, settled or not.
pub(crate) async fn plan_exists_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent: &ScopeId,
) -> Result<bool, PluginError> {
    let (kind, id) = ledger_key(parent);
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
    fence: &crate::guarded_tx::WriterFence,
    parent: &ScopeId,
    ended_at_ms: u64,
) -> Result<(), PluginError> {
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    let fleet_format = tx.fleet();
    record_tx(&mut tx, parent, ended_at_ms, fleet_format).await?;
    tx.commit().await.map_err(plugin_sqlx_error)
}

/// The row `(kind, id)` names, as the read any caller gets: `get` names it
/// by scope, `get_by_key` by the stored columns a `ParentEnd` obligation's
/// claim carries.
async fn get_by_columns(
    pool: &PgPool,
    kind: &str,
    id: &str,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let row = sqlx::query(process_sql().plan.select_stamps.sql())
        .bind(kind)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        decode_plan(
            kind.to_string(),
            id.to_string(),
            row.get(0),
            row.get(1),
            row.get(2),
            row.get(3),
            row.get(4),
            fleet_format,
        )
    })
    .transpose()
}

pub(super) async fn get(
    pool: &PgPool,
    parent: &ScopeId,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Option<ParentEndPlan>, PluginError> {
    let (kind, id) = ledger_key(parent);
    get_by_columns(pool, kind, &id, fleet_format).await
}

/// The row a `ParentEnd` obligation's claim names. Its claim reads only the
/// stored key columns, so this read is what decodes the typed payload —
/// and a payload that does not decode is corrupt, not a page failure.
pub(super) async fn get_by_key(
    pool: &PgPool,
    parent_kind: &str,
    parent_id: &str,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Option<ParentEndPlan>, PluginError> {
    get_by_columns(pool, parent_kind, parent_id, fleet_format).await
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
    pool: &PgPool,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ScopeId>, PluginError> {
    let rows = sqlx::query(
        process_sql()
            .process_postgres
            .list_unrecorded_opener_parents
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
            let kind: String = row.get(1);
            let record_json: String = row.get(2);
            let record: ProcessRecord =
                serde_json::from_str(&record_json).map_err(process_decode_error)?;
            record
                .lifetime
                .scope()
                .cloned()
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
/// the reason it is excluded from every non-terminal registry scan — lash may never act on
/// such a row nor assert an outcome for it, and a cancel request is both.
pub(super) async fn children(
    pool: &PgPool,
    parent: &ScopeId,
    after: Option<&ProcessId>,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (kind, id) = ledger_key(parent);
    let after = after.map(|value| value.to_string());
    let limit = limit.get() as i64;
    // A session's plan also owes the children of every scope inside the
    // session that has no row of its own (FIG-3948).
    let query = match parent {
        ScopeId::Session(session_id) => {
            let (turns_from, turns_to) = EffectOpener::session_turn_encoding_range(session_id);
            let (drains_from, drains_to) =
                EffectOpener::session_queue_drain_encoding_range(session_id);
            sqlx::query(
                process_sql()
                    .process_postgres
                    .list_session_end_children
                    .sql(),
            )
            .bind(id)
            .bind(turns_from)
            .bind(turns_to)
            .bind(drains_from)
            .bind(drains_to)
        }
        ScopeId::Opener(_) => sqlx::query(
            process_sql()
                .process_postgres
                .list_parent_end_children
                .sql(),
        )
        .bind(kind)
        .bind(id),
    };
    let rows = query
        .bind(after)
        .bind(limit)
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
///
/// The settle also delivers a `due` obligation the row owes (ADR 0109): the
/// apply that ends here is the delivery that obligation carries. A `claimed`
/// row's claim owns its own settle, and a `stalled` row keeps its stall.
pub(super) async fn settle(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    parent: &ScopeId,
    settled_at_ms: u64,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(parent);
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    lock_parent_scope_tx(&mut tx, parent).await?;
    sqlx::query(process_sql().plan.settle.sql())
        .bind(kind)
        .bind(id.clone())
        .bind(settled_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    sqlx::query(process_sql().plan.obligation_apply_delivered.sql())
        .bind(kind)
        .bind(id)
        .bind(settled_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)
}
