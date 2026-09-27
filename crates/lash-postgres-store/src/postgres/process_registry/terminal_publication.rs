//! The terminal-publication obligation on a process row (ADR 0109 §3,
//! `ProcessTerminal`).
//!
//! Every transaction that makes a process terminal rewrites its row through
//! [`save_process_tx`](crate::process_helpers::save_process_tx), which arms
//! the row's obligation in that same transaction. The arm only takes a row
//! that owes nothing, so a later save of the terminal record never re-arms a
//! publication already made.

use std::sync::LazyLock;

use lash_core_execution::store::{ObligationId, ObligationKey, ObligationState};
use lash_core_execution::{PluginError, ProcessRecord, ProcessTerminalPublication};
use lash_sansio::ProcessId;
use lash_store_sql::Dialect;
use lash_store_sql::process::processes::ProcessObligationStatements;
use sqlx::{PgPool, Row};

use crate::support::plugin_sqlx_error;

static STATEMENTS: LazyLock<ProcessObligationStatements> =
    LazyLock::new(|| ProcessObligationStatements::render(Dialect::postgres()));

/// Arm `record`'s terminal publication in the transaction that saves it, if
/// the record is terminal and its row owes nothing yet.
pub(crate) async fn arm_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &ProcessRecord,
) -> Result<(), PluginError> {
    if !record.is_terminal() {
        return Ok(());
    }
    crate::obligation_ledger::arm_obligation_tx(
        tx,
        &ObligationKey::ProcessTerminal {
            process_id: record.id.clone(),
        },
        record.updated_at_ms,
    )
    .await
    .map(|_| ())
    .map_err(|error| PluginError::Session(error.to_string()))
}

pub(super) async fn settle(
    pool: &PgPool,
    process_id: &ProcessId,
    settled_at_ms: u64,
) -> Result<bool, PluginError> {
    let changed = sqlx::query(STATEMENTS.obligation_settle_published.sql())
        .bind(process_id.as_str())
        .bind(i64::try_from(settled_at_ms).unwrap_or(i64::MAX))
        .execute(pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected();
    Ok(changed == 1)
}

pub(super) async fn get(
    pool: &PgPool,
    process_id: &ProcessId,
) -> Result<Option<ProcessTerminalPublication>, PluginError> {
    let row = sqlx::query(STATEMENTS.obligation_select_by_process.sql())
        .bind(process_id.as_str())
        .fetch_optional(pool)
        .await
        .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        let id: String = row.try_get(0).map_err(plugin_sqlx_error)?;
        let state: String = row.try_get(1).map_err(plugin_sqlx_error)?;
        Ok(ProcessTerminalPublication {
            id: ObligationId::new(id),
            state: ObligationState::from_label(&state)
                .map_err(|error| PluginError::Session(error.to_string()))?,
        })
    })
    .transpose()
}
