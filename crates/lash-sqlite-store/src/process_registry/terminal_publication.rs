//! The terminal-publication obligation on a process row (ADR 0109 §3,
//! `ProcessTerminal`).
//!
//! Every transaction that makes a process terminal rewrites its row through
//! [`SqliteProcessRegistry::save_process_conn`](super::SqliteProcessRegistry),
//! which arms the row's obligation in that same transaction. The arm only
//! takes a row that owes nothing, so a later save of the terminal record never
//! re-arms a publication already made.

use std::sync::LazyLock;

use lash_core_execution::store::{ObligationId, ObligationKey, ObligationState};
use lash_core_execution::{PluginError, ProcessRecord, ProcessTerminalPublication};
use lash_sansio::ProcessId;
use lash_store_sql::process::processes::ProcessObligationStatements;
use rusqlite::{Connection, OptionalExtension, params};

use super::{SqliteProcessRegistry, process_sqlite_error, tx_outcome};
use crate::schema_layout::Schema;

static STATEMENTS: LazyLock<ProcessObligationStatements> =
    LazyLock::new(|| ProcessObligationStatements::render(Schema::Main.dialect()));

/// Arm `record`'s terminal publication in the transaction that saves it, if
/// the record is terminal and its row owes nothing yet.
pub(super) fn arm_conn(conn: &Connection, record: &ProcessRecord) -> Result<(), PluginError> {
    if !record.is_terminal() {
        return Ok(());
    }
    crate::obligation_ledger::arm_obligation_tx(
        conn,
        &ObligationKey::ProcessTerminal {
            process_id: record.id.clone(),
        },
        record.updated_at_ms,
    )
    .map(|_| ())
    .map_err(|error| PluginError::Session(error.to_string()))
}

pub(super) async fn settle(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
) -> Result<bool, PluginError> {
    let process_id = process_id.as_str().to_owned();
    let settled_at_ms = i64::try_from(registry.clock.timestamp_ms()).unwrap_or(i64::MAX);
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome(
                tx.execute(
                    STATEMENTS.obligation_settle_published.sql(),
                    params![process_id, settled_at_ms],
                )
                .map_err(process_sqlite_error)
                .map(|changed| changed == 1),
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn get(
    registry: &SqliteProcessRegistry,
    process_id: &ProcessId,
) -> Result<Option<ProcessTerminalPublication>, PluginError> {
    let process_id = process_id.as_str().to_owned();
    let row = registry
        .conn
        .call(move |conn| {
            Ok(conn
                .query_row(
                    STATEMENTS.obligation_select_by_process.sql(),
                    params![process_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional())
        })
        .await
        .map_err(process_sqlite_error)?
        .map_err(process_sqlite_error)?;
    row.map(|(id, state)| {
        Ok(ProcessTerminalPublication {
            id: ObligationId::new(id),
            state: ObligationState::from_label(&state)
                .map_err(|error| PluginError::Session(error.to_string()))?,
        })
    })
    .transpose()
}
