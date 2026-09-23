//! SQLite storage atoms for durable AwaitEvent promises.
//!
//! The promise state machine lives in [`AwaitEventCoordinator`]; this module is
//! only the SQLite half of its backend port, and the SQLite owner of the wait
//! family's three tables. Every atom runs inside `SqliteConnection::write`
//! (`BEGIN IMMEDIATE`) or an explicit read transaction, so the tombstone check,
//! the identity comparison, and the write they guard cannot interleave with a
//! competing writer.

use lash_sansio::SessionId;
use std::sync::{Arc, LazyLock};

use lash_core_execution::facade_support::await_event_coordinator::{
    AwaitEventBackend, AwaitEventCoordinator, AwaitEventRowIdentity, AwaitEventVocabulary,
    PersistedPromise, RegisteredAwaitEvent, TerminalCas,
};
use lash_core_execution::{RuntimeError, RuntimeErrorCode};
use lash_store_sql::wait::revoked_sessions::RevokedSessionStatements;
use lash_store_sql::wait::waits::{WaitRow, WaitStatements};
use rusqlite::{OptionalExtension, params};

use crate::conn::SqliteConnection;
use crate::scope_fence::{FenceLocations, RegistryAttachment, Schema};

/// The SQLite promise coordinator: one shared state machine over
/// [`SqliteAwaitEventBackend`].
pub(crate) type SqliteAwaitEvents = AwaitEventCoordinator<SqliteAwaitEventBackend>;

const VOCABULARY: AwaitEventVocabulary = AwaitEventVocabulary {
    sign: RuntimeErrorCode::SqliteAwaitEventSign,
    encode: RuntimeErrorCode::SqliteAwaitEventEncode,
    decode: RuntimeErrorCode::SqliteAwaitEventDecode,
    notify: RuntimeErrorCode::SqliteAwaitEventNotify,
    display_name: "SQLite",
};

lash_store_sql::statements! {
    /// `await_event_waits` statements only SQLite issues.
    pub(crate) struct WaitSqliteStatements @ "await_event_wait" {
        /// No `ON CONFLICT`: the absence of the row was read under the same
        /// `BEGIN IMMEDIATE` lock this insert commits under, so PostgreSQL's
        /// conflict clause has nothing to catch here.
        insert_pending = "INSERT INTO await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)";

        /// Register a promise that is already resolved, `?6` being the
        /// terminal and `?7` the instant it settled. Forks for the same reason
        /// [`WaitSqliteStatements::insert_pending`] does.
        insert_resolved = "INSERT INTO await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)";

        /// Settle the pending promise under key `?1` with terminal `?2` at
        /// `?3`.
        ///
        /// FENCING (FIG-3381): the identity columns are absent from the
        /// predicate because the row was read and compared inside this
        /// transaction under the write lock. PostgreSQL cannot do that under
        /// `READ COMMITTED` and carries the comparison in the statement. The
        /// two semantics are deliberately left as they stand.
        resolve_pending = "UPDATE await_event_waits
             SET terminal_json = ?2, resolved_at_ms = ?3
             WHERE key_id = ?1 AND terminal_json IS NULL";

        /// Cancel session `?1`'s unresolved non-control promises with `?2` at
        /// `?3`. SQLite stores `turn_control` as an integer, PostgreSQL as a
        /// boolean.
        cancel_session_promises = "UPDATE await_event_waits
             SET terminal_json = ?2, resolved_at_ms = ?3
             WHERE session_id = ?1
               AND terminal_json IS NULL
               AND turn_control = 0";
    }
}

lash_store_sql::statements! {
    /// `await_event_meta` statements only SQLite issues.
    pub(crate) struct MetaSqliteStatements @ "await_event_meta" {
        /// The promise signing secret. SQLite stores the singleton flag as an
        /// integer, PostgreSQL as a boolean.
        select_signing_secret = "SELECT signing_secret FROM await_event_meta WHERE singleton = 1";
    }
}

/// Every wait-family statement, rendered for one schema.
pub(crate) struct WaitSql {
    /// `await_event_waits` statements both backends issue verbatim.
    pub(crate) shared: WaitStatements,
    /// `await_event_waits` statements only SQLite issues.
    pub(crate) sqlite: WaitSqliteStatements,
    /// `await_event_revoked_sessions` statements both backends issue verbatim.
    pub(crate) revoked: RevokedSessionStatements,
    /// `await_event_meta` statements only SQLite issues.
    pub(crate) meta_sqlite: MetaSqliteStatements,
}

impl WaitSql {
    fn render(schema: Schema) -> Self {
        let dialect = schema.dialect();
        Self {
            shared: WaitStatements::render(dialect),
            sqlite: WaitSqliteStatements::render(dialect),
            revoked: RevokedSessionStatements::render(dialect),
            meta_sqlite: MetaSqliteStatements::render(dialect),
        }
    }
}

static WAIT_SQL: LazyLock<[WaitSql; 3]> = LazyLock::new(|| Schema::ALL.map(WaitSql::render));

/// The wait-family statements addressed through `schema`, rendered once at
/// first use and never again.
pub(crate) fn wait_sql(schema: Schema) -> &'static WaitSql {
    &WAIT_SQL[schema.index()]
}

/// Build the SQLite await-event coordinator over `conn`.
pub(crate) fn sqlite_await_events(
    conn: SqliteConnection,
    registry: Arc<RegistryAttachment>,
    signing_secret: Vec<u8>,
    clock: Arc<dyn lash_core_execution::Clock>,
) -> SqliteAwaitEvents {
    AwaitEventCoordinator::new(
        SqliteAwaitEventBackend { conn, registry },
        signing_secret.into(),
        clock,
    )
}

/// `pub` only because it names an associated type of the shared replay
/// adapter; the module is private, so nothing outside this crate can reach it.
#[derive(Clone)]
pub struct SqliteAwaitEventBackend {
    conn: SqliteConnection,
    /// The bound process registry whose file holds process-scope fences.
    registry: Arc<RegistryAttachment>,
}

impl SqliteAwaitEventBackend {
    async fn fence_locations(&self) -> Result<FenceLocations, RuntimeError> {
        self.registry
            .ensure_attached(&self.conn)
            .await
            .map_err(store_error)
    }
}

#[async_trait::async_trait]
impl AwaitEventBackend for SqliteAwaitEventBackend {
    fn vocabulary(&self) -> AwaitEventVocabulary {
        VOCABULARY.clone()
    }

    async fn session_is_revoked(&self, session_id: &SessionId) -> Result<bool, RuntimeError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |connection| session_is_revoked(connection, &session_id))
            .await
            .map_err(store_error)
    }

    async fn scope_is_retired(&self, scope_id: &str) -> Result<bool, RuntimeError> {
        let scope_id = scope_id.to_string();
        let fences = self.fence_locations().await?;
        self.conn
            .call(move |connection| fences.is_fenced(connection, &scope_id))
            .await
            .map_err(store_error)
    }

    async fn ensure_pending(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        now_ms: u64,
    ) -> Result<bool, RuntimeError> {
        let key_id = key_id.to_string();
        let identity = identity.clone();
        let now = now_ms as i64;
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                if identity_is_fenced(tx, fences, &identity)? {
                    return Ok(false);
                }
                match select_wait_row(tx, &key_id)? {
                    Some(row) => Ok(matches_identity(&row, &identity)),
                    None => {
                        tx.execute(
                            wait_sql(Schema::Main).sqlite.insert_pending.sql(),
                            params![
                                key_id.as_str(),
                                identity.scope_json,
                                identity.wait_json,
                                identity.session_id.as_deref(),
                                identity.turn_control,
                                now,
                            ],
                        )?;
                        Ok(true)
                    }
                }
            })
            .await
            .map_err(store_error)
    }

    async fn store_terminal(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<TerminalCas, RuntimeError> {
        let key_id = key_id.to_string();
        let identity = identity.clone();
        let proposed_json = terminal_json.to_string();
        let now = now_ms as i64;
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                if identity_is_fenced(tx, fences, &identity)? {
                    return Ok(TerminalCas::UnknownOrRevoked);
                }
                let sql = wait_sql(Schema::Main);
                match select_wait_row(tx, &key_id)? {
                    None => {
                        tx.execute(
                            sql.sqlite.insert_resolved.sql(),
                            params![
                                key_id.as_str(),
                                identity.scope_json,
                                identity.wait_json,
                                identity.session_id.as_deref(),
                                identity.turn_control,
                                proposed_json,
                                now,
                            ],
                        )?;
                        Ok(TerminalCas::Stored)
                    }
                    Some(row) if !matches_identity(&row, &identity) => {
                        Ok(TerminalCas::UnknownOrRevoked)
                    }
                    Some(row) => match row.terminal_json {
                        Some(terminal_json) => Ok(TerminalCas::AlreadyResolved { terminal_json }),
                        None => {
                            let changed = tx.execute(
                                sql.sqlite.resolve_pending.sql(),
                                params![key_id, proposed_json, now],
                            )?;
                            // Unreachable while the `BEGIN IMMEDIATE` write lock
                            // is held: the row was just read as pending inside
                            // this transaction.
                            if changed != 1 {
                                return Err(rusqlite::Error::InvalidQuery);
                            }
                            Ok(TerminalCas::Stored)
                        }
                    },
                }
            })
            .await
            .map_err(store_error)
    }

    async fn inspect(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
    ) -> Result<PersistedPromise, RuntimeError> {
        let key_id = key_id.to_string();
        let identity = identity.clone();
        let fences = self.fence_locations().await?;
        self.conn
            .call(move |connection| {
                // One journal snapshot for the journal's own fence and the
                // row; the attached registry's fence is read after it, in a
                // statement of its own. A read never holds the journal while
                // it waits for the registry: a writer takes them the other
                // way round (journal, then registry, then commits the
                // journal), and a `memdb` reader waits on any writer, so the
                // two would wait out the busy timeout on each other. A fence
                // that lands between the two reads is still seen, and a
                // fence always wins over the row.
                let tx = connection.transaction()?;
                let revoked_in_journal = identity_is_fenced(&tx, fences.journal_only(), &identity)?;
                let stored = select_wait_row(&tx, &key_id)?;
                tx.commit()?;
                let revoked = revoked_in_journal
                    || (identity.session_id.is_none()
                        && fences.is_fenced_in_registry(connection, &identity.scope_id)?);
                if revoked {
                    return Ok(PersistedPromise::UnknownOrRevoked);
                }
                let Some(stored) = stored else {
                    return Ok(PersistedPromise::Missing);
                };
                if !matches_identity(&stored, &identity) {
                    return Ok(PersistedPromise::UnknownOrRevoked);
                }
                Ok(stored
                    .terminal_json
                    .map_or(PersistedPromise::Pending, |terminal_json| {
                        PersistedPromise::Resolved { terminal_json }
                    }))
            })
            .await
            .map_err(store_error)
    }

    async fn list_pending_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RegisteredAwaitEvent>, RuntimeError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |connection| {
                let mut statement = connection
                    .prepare(wait_sql(Schema::Main).shared.list_pending_for_session.sql())?;
                statement
                    .query_map(params![session_id.as_str()], |row| {
                        Ok(RegisteredAwaitEvent {
                            key_id: row.get(0)?,
                            scope_json: row.get(1)?,
                            wait_json: row.get(2)?,
                            turn_control: row.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(store_error)
    }

    async fn revoke_session(
        &self,
        session_id: &SessionId,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let session_id = SessionId::from(session_id.to_string());
        let now = now_ms as i64;
        self.conn
            .write(move |tx| {
                let sql = wait_sql(Schema::Main);
                tx.execute(
                    sql.revoked.insert_ignore.sql(),
                    params![session_id.as_str(), now],
                )?;
                tx.execute(
                    sql.shared.delete_by_session.sql(),
                    params![session_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(store_error)
    }

    async fn cancel_session_promises(
        &self,
        session_id: &SessionId,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let session_id = SessionId::from(session_id.to_string());
        let terminal_json = terminal_json.to_string();
        let now = now_ms as i64;
        self.conn
            .write(move |tx| {
                tx.execute(
                    wait_sql(Schema::Main).sqlite.cancel_session_promises.sql(),
                    params![session_id.as_str(), terminal_json, now],
                )?;
                Ok(())
            })
            .await
            .map_err(store_error)
    }
}

fn matches_identity(row: &WaitRow, identity: &AwaitEventRowIdentity) -> bool {
    row.matches_identity(
        &identity.scope_json,
        &identity.wait_json,
        identity.session_id.as_deref(),
        identity.turn_control,
    )
}

/// Which durable fence refuses `identity`: the owning session's revocation
/// tombstone, or the scope's retirement tombstone. Session-free scopes have
/// only the latter; session scopes are never scope-retired, so one of the two
/// answers for either kind. Read inside the caller's transaction, which is the
/// `BEGIN IMMEDIATE` write the fence's own writer takes.
fn identity_is_fenced(
    connection: &rusqlite::Connection,
    fences: FenceLocations,
    identity: &AwaitEventRowIdentity,
) -> rusqlite::Result<bool> {
    match identity.session_id.as_deref() {
        Some(session_id) => connection.query_row(
            wait_sql(Schema::Main).revoked.exists.sql(),
            params![session_id],
            |row| row.get(0),
        ),
        None => fences.is_fenced(connection, &identity.scope_id),
    }
}

/// The row stored under `key_id`, if any.
fn select_wait_row(
    connection: &rusqlite::Connection,
    key_id: &str,
) -> rusqlite::Result<Option<WaitRow>> {
    connection
        .query_row(
            wait_sql(Schema::Main).shared.select_by_key.sql(),
            params![key_id],
            |row| {
                Ok(WaitRow {
                    scope_json: row.get(0)?,
                    wait_json: row.get(1)?,
                    session_id: row.get(2)?,
                    turn_control: row.get(3)?,
                    terminal_json: row.get(4)?,
                })
            },
        )
        .optional()
}

fn session_is_revoked(
    connection: &rusqlite::Connection,
    session_id: &SessionId,
) -> rusqlite::Result<bool> {
    connection.query_row(
        wait_sql(Schema::Main).revoked.exists.sql(),
        params![session_id.as_str()],
        |row| row.get(0),
    )
}

fn store_error(err: rusqlite::Error) -> RuntimeError {
    RuntimeError::new(
        lash_core_execution::RuntimeErrorCode::SqliteAwaitEventStore,
        err.to_string(),
    )
}
