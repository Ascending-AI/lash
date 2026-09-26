//! The PostgreSQL half of the logical-root family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_roots` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the queued-run settlement, the claim
//! step, session deletion and the factory's catalog reads share, plus
//! [`RootStore`] for the session store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    ControlIntent, ControlIntentId, RootStore, RootTerminal, RootTerminalWriteDecision,
    decide_root_terminal_write, root_binding_conflict,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::session_roots::{
    control_intents::ControlIntentStatements, root_inputs::SessionRootInputStatements,
    roots::SessionRootStatements,
};
use sqlx::{PgConnection, Row};

use crate::support::{store_sqlx_error, u64_from_sql};
use crate::{PostgresSessionStore, StoreError, acquire_runtime_connection};

/// Every logical-root statement this store issues.
pub(crate) struct SessionRootsSql {
    pub(crate) roots: SessionRootStatements,
    pub(crate) inputs: SessionRootInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_ROOTS_SQL: LazyLock<SessionRootsSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    SessionRootsSql {
        roots: SessionRootStatements::render(dialect),
        inputs: SessionRootInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// This store's logical-root statements, rendered once at first use.
pub(crate) fn session_roots_sql() -> &'static SessionRootsSql {
    &SESSION_ROOTS_SQL
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The terminal evidence of `root` in `session_id`, read on `conn`.
pub(crate) async fn root_terminal_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootTerminal>, StoreError> {
    let Some(row) = sqlx::query(session_roots_sql().roots.select_terminal.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
    else {
        return Ok(None);
    };
    let kind: Option<String> = row.try_get(0).map_err(store_sqlx_error)?;
    let cause_json: Option<String> = row.try_get(1).map_err(store_sqlx_error)?;
    let head_revision: Option<i64> = row.try_get(2).map_err(store_sqlx_error)?;
    let at_ms: Option<i64> = row.try_get(3).map_err(store_sqlx_error)?;
    let (Some(kind), Some(cause_json), Some(at_ms)) = (kind, cause_json, at_ms) else {
        return Ok(None);
    };
    RootTerminal::from_stored(
        session_id.clone(),
        root.clone(),
        &kind,
        &cause_json,
        head_revision
            .map(|revision| u64_from_sql("RootTerminal", "terminal_head_revision", revision))
            .transpose()?,
        u64_from_sql("RootTerminal", "terminal_at_ms", at_ms)?,
    )
    .map(Some)
}

/// Write `terminal` in the caller's transaction, deciding it against the
/// stored evidence first: the same terminal is a no-op, another one is
/// [`StoreError::RootAlreadyTerminal`].
#[expect(
    dead_code,
    reason = "the commit-path writer for RuntimeCommit::root_terminal lands with S7-B"
)]
pub(crate) async fn write_root_terminal_conn(
    conn: &mut PgConnection,
    terminal: &RootTerminal,
) -> Result<(), StoreError> {
    let stored = root_terminal_conn(conn, &terminal.session_id, &terminal.root).await?;
    if decide_root_terminal_write(stored.as_ref(), terminal)?
        == RootTerminalWriteDecision::AlreadyWritten
    {
        return Ok(());
    }
    let sql = session_roots_sql();
    sqlx::query(sql.roots.insert_open.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let columns = terminal.to_stored()?;
    let written = sqlx::query(sql.roots.write_terminal.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.root.as_str())
        .bind(columns.kind)
        .bind(columns.cause_json)
        .bind(
            columns
                .head_revision
                .map(|revision| sql_i64("terminal head revision", revision))
                .transpose()?,
        )
        .bind(sql_i64("terminal instant", columns.at_ms)?)
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "root `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.root, terminal.session_id
        )));
    }
    Ok(())
}

/// The root input `input` of `session_id` is bound to, read on `conn`.
pub(crate) async fn root_binding_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    sqlx::query_scalar::<_, String>(session_roots_sql().inputs.select_root.sql())
        .bind(session_id.as_str())
        .bind(input.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)
        .map(|root| root.map(TurnId::from))
}

/// Bind each of `inputs` to `root`, set-if-absent, and open `root`'s row, in
/// the caller's transaction. A binding to another root refuses the whole
/// write.
pub(crate) async fn bind_root_inputs_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    for input in inputs {
        if let Some(bound) = root_binding_conn(conn, session_id, input).await?
            && bound != *root
        {
            return Err(root_binding_conflict(session_id, input, &bound, root));
        }
    }
    let sql = session_roots_sql();
    sqlx::query(sql.roots.insert_open.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    for input in inputs {
        sqlx::query(sql.inputs.insert.sql())
            .bind(session_id.as_str())
            .bind(input.as_str())
            .bind(root.as_str())
            .execute(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

fn decode_intent(row: &sqlx::postgres::PgRow) -> Result<ControlIntent, StoreError> {
    let id: i64 = row.try_get(0).map_err(store_sqlx_error)?;
    let session_id: String = row.try_get(1).map_err(store_sqlx_error)?;
    let format: i64 = row.try_get(2).map_err(store_sqlx_error)?;
    let kind_json: String = row.try_get(3).map_err(store_sqlx_error)?;
    let state_json: String = row.try_get(4).map_err(store_sqlx_error)?;
    let attempts: i64 = row.try_get(5).map_err(store_sqlx_error)?;
    let created_at_ms: i64 = row.try_get(6).map_err(store_sqlx_error)?;
    let engine_ref: Option<String> = row.try_get(7).map_err(store_sqlx_error)?;
    let corrupt = |field: &str| StoreError::StoredDataCorrupt {
        record_kind: "ControlIntent",
        message: format!("{field} out of range"),
    };
    ControlIntent::from_stored(
        u64_from_sql("ControlIntent", "intent_id", id)?,
        SessionId::from(session_id),
        u32::try_from(format).map_err(|_| corrupt("format"))?,
        &kind_json,
        &state_json,
        u32::try_from(attempts).map_err(|_| corrupt("attempts"))?,
        u64_from_sql("ControlIntent", "created_at_ms", created_at_ms)?,
        engine_ref,
    )
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) async fn close_session_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    sqlx::query(session_roots_sql().intents.select_close_session.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(decode_intent)
        .transpose()
}

/// Open control intents after `after`, at most `limit`, read on `conn`.
pub(crate) async fn open_control_intents_conn(
    conn: &mut PgConnection,
    after: Option<ControlIntentId>,
    limit: usize,
) -> Result<Vec<ControlIntent>, StoreError> {
    let rows = sqlx::query(session_roots_sql().intents.select_open_after.sql())
        .bind(sql_i64(
            "control intent cursor",
            after.map_or(0, ControlIntentId::sequence),
        )?)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    rows.iter().map(decode_intent).collect()
}

/// Forget what session `session_id`'s roots hold, in its deletion's
/// transaction: its roots, its bindings and its verbs. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's roots from.
pub(crate) async fn delete_session_roots_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for statement in [
        sql.roots.delete_by_session.sql(),
        sql.inputs.delete_by_session.sql(),
        sql.intents.delete_verbs_by_session.sql(),
    ] {
        sqlx::query(statement)
            .bind(session_id.as_str())
            .execute(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl RootStore for PostgresSessionStore {
    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        root_terminal_conn(&mut connection, session_id, root).await
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
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        root_binding_conn(&mut connection, session_id, input).await
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        self.bind_session_id(session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = sqlx::Connection::begin(&mut *connection)
            .await
            .map_err(store_sqlx_error)?;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        bind_root_inputs_conn(&mut tx, session_id, root, inputs).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}
