//! The SQLite half of the logical-root family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_roots` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the queued-run settlement, the claim
//! step, session deletion and the factory's catalog reads share, plus
//! [`RootStore`] for the bound store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    ControlIntent, ControlIntentId, RootStore, RootTerminal, RootTerminalWriteDecision,
    decide_root_terminal_write, root_binding_conflict,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::session_roots::{
    control_intents::ControlIntentStatements, root_inputs::SessionRootInputStatements,
    roots::SessionRootStatements,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::conn::TxOutcome;
use crate::scope_fence::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

/// Every logical-root statement the session catalog issues.
pub(crate) struct SessionRootsSql {
    pub(crate) roots: SessionRootStatements,
    pub(crate) inputs: SessionRootInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_ROOTS_SQL: LazyLock<SessionRootsSql> = LazyLock::new(|| {
    let dialect = Schema::Main.dialect();
    SessionRootsSql {
        roots: SessionRootStatements::render(dialect),
        inputs: SessionRootInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// The session catalog's logical-root statements, rendered once at first use.
pub(crate) fn session_roots_sql() -> &'static SessionRootsSql {
    &SESSION_ROOTS_SQL
}

fn stored_u64(record: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt(record, "a negative counter"))
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The terminal evidence of `root` in `session_id`, read on `conn`.
pub(crate) fn root_terminal_conn(
    conn: &Connection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootTerminal>, StoreError> {
    let row: Option<(Option<String>, Option<String>, Option<i64>, Option<i64>)> = conn
        .query_row(
            session_roots_sql().roots.select_terminal.sql(),
            params![session_id.as_str(), root.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((Some(kind), Some(cause_json), head_revision, Some(at_ms))) = row else {
        return Ok(None);
    };
    RootTerminal::from_stored(
        session_id.clone(),
        root.clone(),
        &kind,
        &cause_json,
        head_revision
            .map(|revision| stored_u64("RootTerminal", revision))
            .transpose()?,
        stored_u64("RootTerminal", at_ms)?,
    )
    .map(Some)
}

/// Write `terminal` in the caller's transaction, deciding it against the
/// stored evidence first: the same terminal is a no-op, another one is
/// [`StoreError::RootAlreadyTerminal`].
pub(crate) fn write_root_terminal_conn(
    tx: &Connection,
    terminal: &RootTerminal,
) -> Result<(), StoreError> {
    let stored = root_terminal_conn(tx, &terminal.session_id, &terminal.root)?;
    if decide_root_terminal_write(stored.as_ref(), terminal)?
        == RootTerminalWriteDecision::AlreadyWritten
    {
        return Ok(());
    }
    let sql = session_roots_sql();
    tx.execute(
        sql.roots.insert_open.sql(),
        params![terminal.session_id.as_str(), terminal.root.as_str()],
    )
    .map_err(sqlite_error)?;
    let columns = terminal.to_stored()?;
    let written = tx
        .execute(
            sql.roots.write_terminal.sql(),
            params![
                terminal.session_id.as_str(),
                terminal.root.as_str(),
                columns.kind,
                columns.cause_json,
                columns
                    .head_revision
                    .map(|revision| sql_i64("terminal head revision", revision))
                    .transpose()?,
                sql_i64("terminal instant", columns.at_ms)?,
            ],
        )
        .map_err(sqlite_error)?;
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "root `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.root, terminal.session_id
        )));
    }
    Ok(())
}

/// The root input `input` of `session_id` is bound to, read on `conn`.
pub(crate) fn root_binding_conn(
    conn: &Connection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    conn.query_row(
        session_roots_sql().inputs.select_root.sql(),
        params![session_id.as_str(), input.as_str()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(sqlite_error)
    .map(|root| root.map(TurnId::from))
}

/// Bind each of `inputs` to `root`, set-if-absent, and open `root`'s row, in
/// the caller's transaction. A binding to another root refuses the whole
/// write.
pub(crate) fn bind_root_inputs_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for input in inputs {
        if let Some(bound) = root_binding_conn(tx, session_id, input)?
            && bound != *root
        {
            return Err(root_binding_conflict(session_id, input, &bound, root));
        }
    }
    tx.execute(
        sql.roots.insert_open.sql(),
        params![session_id.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    for input in inputs {
        tx.execute(
            sql.inputs.insert.sql(),
            params![session_id.as_str(), input.as_str(), root.as_str()],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

fn intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredIntentRow> {
    Ok(StoredIntentRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        format: row.get(2)?,
        kind_json: row.get(3)?,
        state_json: row.get(4)?,
        attempts: row.get(5)?,
        created_at_ms: row.get(6)?,
        engine_ref: row.get(7)?,
    })
}

struct StoredIntentRow {
    id: i64,
    session_id: String,
    format: i64,
    kind_json: String,
    state_json: String,
    attempts: i64,
    created_at_ms: i64,
    engine_ref: Option<String>,
}

impl StoredIntentRow {
    fn decode(self) -> Result<ControlIntent, StoreError> {
        ControlIntent::from_stored(
            stored_u64("ControlIntent", self.id)?,
            SessionId::from(self.session_id),
            u32::try_from(self.format)
                .map_err(|_| stored_data_corrupt("ControlIntent", "format out of range"))?,
            &self.kind_json,
            &self.state_json,
            u32::try_from(self.attempts)
                .map_err(|_| stored_data_corrupt("ControlIntent", "attempts out of range"))?,
            stored_u64("ControlIntent", self.created_at_ms)?,
            self.engine_ref,
        )
    }
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) fn close_session_intent_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    conn.query_row(
        session_roots_sql().intents.select_close_session.sql(),
        params![session_id.as_str()],
        intent_row,
    )
    .optional()
    .map_err(sqlite_error)?
    .map(StoredIntentRow::decode)
    .transpose()
}

/// Open control intents after `after`, at most `limit`, read on `conn`.
pub(crate) fn open_control_intents_conn(
    conn: &Connection,
    after: Option<ControlIntentId>,
    limit: usize,
) -> Result<Vec<ControlIntent>, StoreError> {
    let after = sql_i64(
        "control intent cursor",
        after.map_or(0, ControlIntentId::sequence),
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = conn
        .prepare(session_roots_sql().intents.select_open_after.sql())
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![after, limit], intent_row)
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    rows.into_iter().map(StoredIntentRow::decode).collect()
}

/// Forget what session `session_id`'s roots hold, in its deletion's
/// transaction: its roots, its bindings and its verbs. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's roots from.
pub(crate) fn delete_session_roots_conn(
    tx: &Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for statement in [
        sql.roots.delete_by_session.sql(),
        sql.inputs.delete_by_session.sql(),
        sql.intents.delete_verbs_by_session.sql(),
    ] {
        tx.execute(statement, params![session_id.as_str()])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

#[async_trait::async_trait]
impl RootStore for crate::Store {
    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError> {
        let session_id = session_id.clone();
        let root = root.clone();
        self.conn
            .call(move |conn| Ok(root_terminal_conn(conn, &session_id, &root)))
            .await
            .map_err(sqlite_error)?
    }

    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        // Queued-run members are bound when S8 folds queued runs into the
        // logical-root record; until then a claim's binding is the answer.
        self.root_binding(session_id, input).await
    }

    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        let session_id = session_id.clone();
        let input = input.clone();
        self.conn
            .call(move |conn| Ok(root_binding_conn(conn, &session_id, &input)))
            .await
            .map_err(sqlite_error)?
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        self.bind_session(session_id)?;
        let session_id = session_id.clone();
        let root = root.clone();
        let inputs = inputs.to_vec();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                    bind_root_inputs_conn(tx, &session_id, &root, &inputs)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
