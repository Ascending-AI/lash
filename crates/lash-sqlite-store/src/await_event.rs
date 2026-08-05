//! SQLite storage atoms for durable AwaitEvent promises.
//!
//! The promise state machine lives in [`AwaitEventCoordinator`]; this module is
//! only the SQLite half of its backend port. Every atom runs inside
//! `SqliteConnection::write` (`BEGIN IMMEDIATE`) or an explicit read
//! transaction, so the tombstone check, the identity comparison, and the write
//! they guard cannot interleave with a competing writer.

use std::sync::Arc;

use lash_core::RuntimeError;
use lash_core::facade_support::await_event_coordinator::{
    AwaitEventBackend, AwaitEventCoordinator, AwaitEventRowIdentity, AwaitEventVocabulary,
    PersistedPromise, TerminalCas,
};
use rusqlite::{OptionalExtension, params};

use crate::conn::SqliteConnection;

/// The SQLite promise coordinator: one shared state machine over
/// [`SqliteAwaitEventBackend`].
pub(crate) type SqliteAwaitEvents = AwaitEventCoordinator<SqliteAwaitEventBackend>;

const VOCABULARY: AwaitEventVocabulary = AwaitEventVocabulary {
    code_prefix: "sqlite",
    display_name: "SQLite",
};

/// Build the SQLite await-event coordinator over `conn`.
pub(crate) fn sqlite_await_events(
    conn: SqliteConnection,
    signing_secret: Vec<u8>,
    clock: Arc<dyn lash_core::Clock>,
) -> SqliteAwaitEvents {
    AwaitEventCoordinator::new(
        SqliteAwaitEventBackend { conn },
        signing_secret.into(),
        clock,
    )
}

#[derive(Clone)]
pub(crate) struct SqliteAwaitEventBackend {
    conn: SqliteConnection,
}

#[async_trait::async_trait]
impl AwaitEventBackend for SqliteAwaitEventBackend {
    fn vocabulary(&self) -> AwaitEventVocabulary {
        VOCABULARY
    }

    async fn session_is_revoked(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |connection| session_is_revoked(connection, &session_id))
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
        self.conn
            .write(move |tx| {
                if let Some(session_id) = identity.session_id.as_deref()
                    && session_is_revoked(tx, session_id)?
                {
                    return Ok(false);
                }
                match select_wait_row(tx, &key_id)? {
                    Some(row) => Ok(row.matches(&identity)),
                    None => {
                        tx.execute(
                            "INSERT INTO await_event_waits (
                                key_id, scope_json, wait_json, session_id, turn_control,
                                terminal_json, created_at_ms, resolved_at_ms
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
                            params![
                                key_id,
                                identity.scope_json,
                                identity.wait_json,
                                identity.session_id,
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
        self.conn
            .write(move |tx| {
                if let Some(session_id) = identity.session_id.as_deref()
                    && session_is_revoked(tx, session_id)?
                {
                    return Ok(TerminalCas::UnknownOrRevoked);
                }
                match select_wait_row(tx, &key_id)? {
                    None => {
                        tx.execute(
                            "INSERT INTO await_event_waits (
                                key_id, scope_json, wait_json, session_id, turn_control,
                                terminal_json, created_at_ms, resolved_at_ms
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                key_id,
                                identity.scope_json,
                                identity.wait_json,
                                identity.session_id,
                                identity.turn_control,
                                proposed_json,
                                now,
                                now,
                            ],
                        )?;
                        Ok(TerminalCas::Stored)
                    }
                    Some(row) if !row.matches(&identity) => Ok(TerminalCas::UnknownOrRevoked),
                    Some(row) => match row.terminal_json {
                        Some(terminal_json) => Ok(TerminalCas::AlreadyResolved { terminal_json }),
                        None => {
                            let changed = tx.execute(
                                "UPDATE await_event_waits
                                 SET terminal_json = ?2, resolved_at_ms = ?3
                                 WHERE key_id = ?1 AND terminal_json IS NULL",
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
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let revoked = match identity.session_id.as_deref() {
                    Some(session_id) => session_is_revoked(&tx, session_id)?,
                    None => false,
                };
                let stored = select_wait_row(&tx, &key_id)?;
                tx.commit()?;
                if revoked {
                    return Ok(PersistedPromise::UnknownOrRevoked);
                }
                let Some(stored) = stored else {
                    return Ok(PersistedPromise::Missing);
                };
                if !stored.matches(&identity) {
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

    async fn revoke_session(&self, session_id: &str, now_ms: u64) -> Result<(), RuntimeError> {
        let session_id = session_id.to_string();
        let now = now_ms as i64;
        self.conn
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO await_event_revoked_sessions (session_id, revoked_at_ms)
                     VALUES (?1, ?2)
                     ON CONFLICT(session_id) DO NOTHING",
                    params![session_id, now],
                )?;
                tx.execute(
                    "DELETE FROM await_event_waits WHERE session_id = ?1",
                    params![session_id],
                )?;
                Ok(())
            })
            .await
            .map_err(store_error)
    }

    async fn cancel_session_promises(
        &self,
        session_id: &str,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let session_id = session_id.to_string();
        let terminal_json = terminal_json.to_string();
        let now = now_ms as i64;
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE await_event_waits
                     SET terminal_json = ?2, resolved_at_ms = ?3
                     WHERE session_id = ?1
                       AND terminal_json IS NULL
                       AND turn_control = 0",
                    params![session_id, terminal_json, now],
                )?;
                Ok(())
            })
            .await
            .map_err(store_error)
    }
}

struct WaitRow {
    scope_json: String,
    wait_json: String,
    session_id: Option<String>,
    turn_control: bool,
    terminal_json: Option<String>,
}

impl WaitRow {
    fn matches(&self, identity: &AwaitEventRowIdentity) -> bool {
        self.scope_json == identity.scope_json
            && self.wait_json == identity.wait_json
            && self.session_id == identity.session_id
            && self.turn_control == identity.turn_control
    }
}

fn select_wait_row(
    connection: &rusqlite::Connection,
    key_id: &str,
) -> rusqlite::Result<Option<WaitRow>> {
    connection
        .query_row(
            "SELECT scope_json, wait_json, session_id, turn_control, terminal_json
             FROM await_event_waits
             WHERE key_id = ?1",
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
    session_id: &str,
) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM await_event_revoked_sessions WHERE session_id = ?1
         )",
        params![session_id],
        |row| row.get(0),
    )
}

fn store_error(err: rusqlite::Error) -> RuntimeError {
    RuntimeError::new("sqlite_await_event_store", err.to_string())
}
