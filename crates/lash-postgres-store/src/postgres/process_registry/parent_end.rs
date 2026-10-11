//! Turn scopes derive their close from the retained run terminal. Other
//! scopes retain their typed close ledger, including after a process is pruned.

use lash_core_execution::{EffectOpener, ParentEndPlan, PluginError, ScopeId};
use sqlx::{PgPool, Row};

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

/// One stored row's plan. A typed payload that does not decode is corrupt
/// stored data.
fn decode_plan(
    kind: &str,
    id: &str,
    payload: &str,
    ended_at_ms: i64,
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
        ended_at_ms: ended_at_ms.max(0) as u64,
    })
}

/// Serialize registration with the scope's close for this transaction. A
/// turn locks its run row, the same row its terminal writes: taking a turn
/// advisory lock before updating the hold marker would reverse the terminal
/// writer's row-then-close order. Other scopes retain their advisory lock.
pub(crate) async fn lock_parent_scope_tx(
    tx: &mut sqlx::PgConnection,
    parent: &ScopeId,
) -> Result<(), PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = parent.opener()
    {
        crate::observed_sql::query(process_sql().plan_postgres.lock_turn.sql())
            .bind(session_id.as_str())
            .bind(turn_id.as_str())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
        return Ok(());
    }
    let (kind, id) = ledger_key(parent);
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(format!("lash-parent-end:{kind}:{id}"))
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map(drop)
    .map_err(plugin_sqlx_error)
}

/// Record that `parent` ended at `ended_at_ms`, in the caller's
/// transaction, under the scope's lock: a process's terminal append, or the
/// durable commit that closed a turn or a session
/// (`ProcessWrite::ScopeClosed`). A repeated record keeps the first row.
pub(crate) async fn record_tx(
    tx: &mut sqlx::PgConnection,
    parent: &ScopeId,
    ended_at_ms: u64,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(), PluginError> {
    lock_parent_scope_tx(tx, parent).await?;
    if matches!(parent.opener(), Some(EffectOpener::Turn { .. })) {
        return Ok(());
    }
    let (kind, id) = ledger_key(parent);
    crate::observed_sql::query(process_sql().plan.insert_if_absent.sql())
        .bind(kind)
        .bind(id.clone())
        .bind(ledger_payload(parent, fleet_format)?)
        .bind(ended_at_ms as i64)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)?;
    // The close ends every wait the scope's calls still hold (ADR 0116
    // §3.6): an abandoned call leaks no hold, and a late start under the
    // closed scope is refused, so no redrive needs the row pinned.
    crate::observed_sql::query(process_sql().process.release_consumer_holds_owned_by.sql())
        .bind(kind)
        .bind(&id)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)?;
    // Its abandoned holds' marks go with it: the ledger row now refuses a
    // start under the scope.
    crate::observed_sql::query(process_sql().abandoned_hold.forget_owned_by.sql())
        .bind(kind)
        .bind(id)
        .execute(crate::observed_sql::executor(&mut *tx))
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
    tx: &mut sqlx::PgConnection,
    key: &str,
) -> Result<(), PluginError> {
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(format!("lash-consumer-hold:{key}"))
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map(drop)
    .map_err(plugin_sqlx_error)
}

/// Whether a ledger row exists for this scope, settled or not.
pub(crate) async fn plan_exists_tx(
    tx: &mut sqlx::PgConnection,
    parent: &ScopeId,
) -> Result<bool, PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = parent.opener()
    {
        return crate::observed_sql::query(process_sql().plan.turn_terminal.sql())
            .bind(session_id.as_str())
            .bind(turn_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map(|row| row.is_some())
            .map_err(plugin_sqlx_error);
    }
    let (kind, id) = ledger_key(parent);
    let row = crate::observed_sql::query(process_sql().plan.exists.sql())
        .bind(kind)
        .bind(id)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(row.is_some())
}

pub(super) async fn get(
    pool: &PgPool,
    parent: &ScopeId,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<Option<ParentEndPlan>, PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = parent.opener()
    {
        let ended: Option<i64> =
            crate::observed_sql::query_scalar(process_sql().plan.turn_terminal.sql())
                .bind(session_id.as_str())
                .bind(turn_id.as_str())
                .fetch_optional(crate::observed_sql::executor(pool))
                .await
                .map_err(plugin_sqlx_error)?;
        return Ok(ended.map(|ended| ParentEndPlan {
            parent: parent.clone(),
            ended_at_ms: ended.max(0) as u64,
        }));
    }
    let (kind, id) = ledger_key(parent);
    let row = crate::observed_sql::query(process_sql().plan.select_stamps.sql())
        .bind(kind)
        .bind(&id)
        .fetch_optional(crate::observed_sql::executor(pool))
        .await
        .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        let payload: String = row.get(0);
        decode_plan(kind, &id, &payload, row.get(1), fleet_format)
    })
    .transpose()
}

/// Registration/abandonment marks only turn owners; other scopes keep their ledger.
pub(crate) async fn mark_holds_tx(
    tx: &mut sqlx::PgConnection,
    owner: &ScopeId,
) -> Result<(), PluginError> {
    if let Some(EffectOpener::Turn {
        session_id,
        turn_id,
    }) = owner.opener()
    {
        crate::observed_sql::query(process_sql().plan.mark_turn_holds.sql())
            .bind(session_id.as_str())
            .bind(turn_id.as_str())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(plugin_sqlx_error)?;
    }
    Ok(())
}

/// The close releases only holds the scope registered.
pub(crate) async fn release_holds_tx(
    tx: &mut sqlx::PgConnection,
    owner: &ScopeId,
) -> Result<(), PluginError> {
    let (kind, id) = ledger_key(owner);
    crate::observed_sql::query(process_sql().process.release_consumer_holds_owned_by.sql())
        .bind(kind)
        .bind(&id)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    crate::observed_sql::query(process_sql().abandoned_hold.forget_owned_by.sql())
        .bind(kind)
        .bind(&id)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(plugin_sqlx_error)?;
    Ok(())
}
