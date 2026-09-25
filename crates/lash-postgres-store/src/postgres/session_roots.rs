//! The PostgreSQL half of the logical-root family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_roots` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the queued-run settlement, the claim
//! step, session deletion and the factory's catalog reads share, plus
//! [`RootStore`] for the session store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    EnginePark, ParkCancelCause, RootStore, RootTerminal, RootTerminalCause, RootTerminalKind,
    RootTerminalWriteDecision, TurnParkEventKind, close_admission, decide_root_terminal_write,
    root_binding_conflict, stored_intent_kind, stored_intent_state,
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

/// Intent `id`, read on `conn`.
pub(crate) async fn load_intent_conn(
    conn: &mut PgConnection,
    id: ControlIntentId,
) -> Result<Option<ControlIntent>, StoreError> {
    sqlx::query(session_roots_sql().intents.select_by_id.sql())
        .bind(sql_i64("control intent id", id.sequence())?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(decode_intent)
        .transpose()
}

/// Session `session_id`'s open verbs (pending, or failed and retryable), in
/// id order, read on `conn`.
pub(crate) async fn open_verbs_by_session_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Vec<ControlIntent>, StoreError> {
    let rows = sqlx::query(
        session_roots_sql()
            .intents
            .select_open_verbs_by_session
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_all(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    rows.iter().map(decode_intent).collect()
}

/// Record a new intent of `session_id` in the caller's transaction: `kind`,
/// `Pending`, no attempt yet. Answers it with its allocated id.
pub(crate) async fn insert_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    kind: ControlIntentKind,
    engine: Option<&EnginePark>,
    at_ms: u64,
) -> Result<ControlIntent, StoreError> {
    let state = ControlIntentState::Pending;
    let (state_code, state_json) = stored_intent_state(&state)?;
    let id: i64 = sqlx::query_scalar(session_roots_sql().intents.insert.sql())
        .bind(session_id.as_str())
        .bind(i64::from(CONTROL_INTENT_FORMAT))
        .bind(kind.code())
        .bind(stored_intent_kind(&kind)?)
        .bind(state_code)
        .bind(state_json)
        .bind(sql_i64("control intent instant", at_ms)?)
        .bind(engine.map(EnginePark::as_str))
        .fetch_one(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    Ok(ControlIntent {
        id: ControlIntentId::from_sequence(u64_from_sql("ControlIntent", "intent_id", id)?),
        session_id: session_id.clone(),
        format: CONTROL_INTENT_FORMAT,
        kind,
        state,
        attempts: 0,
        created_at_ms: at_ms,
        engine: engine.cloned(),
    })
}

/// Move intent `prior` to `next`'s state and attempt count in the caller's
/// transaction, if it is still as `prior` read it. `false` when another
/// writer moved it first.
pub(crate) async fn write_intent_state_conn(
    conn: &mut PgConnection,
    prior: &ControlIntent,
    next: &ControlIntent,
) -> Result<bool, StoreError> {
    let (state_code, state_json) = stored_intent_state(&next.state)?;
    let (_, prior_json) = stored_intent_state(&prior.state)?;
    let changed = sqlx::query(session_roots_sql().intents.update_state.sql())
        .bind(sql_i64("control intent id", next.id.sequence())?)
        .bind(state_code)
        .bind(state_json)
        .bind(i64::from(next.attempts))
        .bind(prior_json)
        .bind(i64::from(prior.attempts))
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    Ok(changed == 1)
}

/// The store half of session `session_id`'s close, in the caller's
/// transaction ([`ControlIntentStore::begin_session_close`]).
///
/// It takes the session's history-mutation lock first, the lock acceptance
/// takes, so no input is accepted into a session once its close committed.
/// The close names the roots it releases: every root without terminal
/// evidence (its logical-root rows, its parked root, its pending queued
/// run), each ended `Cancelled` by `SessionDeleted`, plus the roots of the
/// open verbs it supersedes, whose engine half then never runs.
///
/// [`ControlIntentStore::begin_session_close`]: lash_core_execution::store::ControlIntentStore::begin_session_close
pub(crate) async fn begin_session_close_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    at_ms: u64,
) -> Result<Option<ControlIntent>, StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    if let Some(intent) = close_session_intent_conn(tx, session_id).await? {
        return Ok(Some(intent));
    }
    let meta: Option<Option<i64>> = sqlx::query_scalar(
        crate::session_sql::session_sql()
            .meta
            .select_closing_intent
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if meta.is_none() {
        return Ok(None);
    }
    let sql = session_roots_sql();
    let mut roots = std::collections::BTreeSet::new();
    let open: Vec<String> = sqlx::query_scalar(sql.roots.select_open_roots.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    roots.extend(open.into_iter().map(TurnId::from));
    // The parked root is released with the park, whose feed event outlives
    // the session (FIG-3659).
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks
            .delete_by_session_returning
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if let Some(released) = released {
        let parked_root: String = released.try_get(0).map_err(store_sqlx_error)?;
        let park_id: i64 = released.try_get(1).map_err(store_sqlx_error)?;
        crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
            tx,
            session_id,
            &parked_root,
            park_id,
            &TurnParkEventKind::Cancelled {
                cause: ParkCancelCause::SessionDeleted,
            },
            at_ms,
        )
        .await?;
        roots.insert(TurnId::from(parked_root));
    }
    roots.extend(crate::runtime_persistence::pending_queued_root_tx(tx, session_id).await?);
    let verbs = open_verbs_by_session_conn(tx, session_id).await?;
    for verb in &verbs {
        match &verb.kind {
            ControlIntentKind::Redrive { root, .. }
            | ControlIntentKind::Cancel { root, .. }
            | ControlIntentKind::Fork { root, .. } => {
                roots.insert(root.clone());
            }
            ControlIntentKind::CloseSession { .. } => {}
        }
    }
    let roots: Vec<TurnId> = roots.into_iter().collect();
    let intent = insert_intent_conn(
        tx,
        session_id,
        ControlIntentKind::CloseSession {
            roots: roots.clone(),
        },
        None,
        at_ms,
    )
    .await?;
    for root in &roots {
        if root_terminal_conn(tx, session_id, root).await?.is_none() {
            write_root_terminal_conn(
                tx,
                &RootTerminal {
                    session_id: session_id.clone(),
                    root: root.clone(),
                    kind: RootTerminalKind::Cancelled,
                    cause: RootTerminalCause::SessionDeleted { intent: intent.id },
                    head_revision: None,
                    at_ms,
                },
            )
            .await?;
        }
    }
    for verb in verbs {
        let mut superseded = verb.clone();
        superseded.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &verb, &superseded).await? {
            return Err(StoreError::Contended);
        }
    }
    let closed = sqlx::query(crate::session_sql::session_sql().meta.begin_close.sql())
        .bind(session_id.as_str())
        .bind(sql_i64("control intent id", intent.id.sequence())?)
        .bind(close_admission(intent.id).as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if closed != 1 {
        return Err(StoreError::Contended);
    }
    Ok(Some(intent))
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
