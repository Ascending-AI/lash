//! SQLite storage atoms for durable AwaitEvent promises.
//!
//! The promise state machine lives in [`AwaitEventCoordinator`]; this module is
//! only the SQLite half of its backend port. Every atom runs inside
//! `SqliteConnection::write` (`BEGIN IMMEDIATE`) or an explicit read
//! transaction, so the tombstone check, the identity comparison, and the write
//! they guard cannot interleave with a competing writer.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::facade_support::await_event_coordinator::{
    AwaitEventBackend, AwaitEventCoordinator, AwaitEventRowIdentity, AwaitEventVocabulary,
    PersistedPromise, RegisteredAwaitEvent, TerminalCas,
};
use lash_core::{RuntimeError, RuntimeErrorCode};
use rusqlite::params;

use crate::conn::SqliteConnection;
use crate::scope_fence::{FenceLocations, RegistryAttachment};

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

/// Build the SQLite await-event coordinator over `conn`.
pub(crate) fn sqlite_await_events(
    conn: SqliteConnection,
    registry: Arc<RegistryAttachment>,
    signing_secret: Vec<u8>,
    clock: Arc<dyn lash_core::Clock>,
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
                let (fenced, stored) = select_fence_and_wait_row(tx, fences, &identity, &key_id)?;
                if fenced {
                    return Ok(false);
                }
                match stored {
                    Some(row) => Ok(row.matches(&identity)),
                    None => {
                        tx.execute(
                            "INSERT INTO await_event_waits (
                                key_id, scope_json, wait_json, session_id, turn_control,
                                terminal_json, created_at_ms, resolved_at_ms
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
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
                let (fenced, stored) = select_fence_and_wait_row(tx, fences, &identity, &key_id)?;
                if fenced {
                    return Ok(TerminalCas::UnknownOrRevoked);
                }
                match stored {
                    None => {
                        tx.execute(
                            "INSERT INTO await_event_waits (
                                key_id, scope_json, wait_json, session_id, turn_control,
                                terminal_json, created_at_ms, resolved_at_ms
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                key_id.as_str(),
                                identity.scope_json,
                                identity.wait_json,
                                identity.session_id.as_deref(),
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
        let fences = self.fence_locations().await?;
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let (revoked, stored) = select_fence_and_wait_row(&tx, fences, &identity, &key_id)?;
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

    async fn list_pending_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RegisteredAwaitEvent>, RuntimeError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT key_id, scope_json, wait_json, turn_control
                     FROM await_event_waits
                     WHERE session_id = ?1
                       AND terminal_json IS NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM await_event_revoked_sessions
                           WHERE session_id = ?1
                       )
                     ORDER BY key_id",
                )?;
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
                tx.execute(
                    "INSERT INTO await_event_revoked_sessions (session_id, revoked_at_ms)
                     VALUES (?1, ?2)
                     ON CONFLICT(session_id) DO NOTHING",
                    params![session_id.as_str(), now],
                )?;
                tx.execute(
                    "DELETE FROM await_event_waits WHERE session_id = ?1",
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
                    "UPDATE await_event_waits
                     SET terminal_json = ?2, resolved_at_ms = ?3
                     WHERE session_id = ?1
                       AND terminal_json IS NULL
                       AND turn_control = 0",
                    params![session_id.as_str(), terminal_json, now],
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
    session_id: Option<SessionId>,
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

/// Which durable fence refuses `identity`: the owning session's revocation
/// tombstone, or the scope's retirement tombstone. Session-free scopes have only
/// the latter; session scopes are never scope-retired, so one predicate answers
/// for either kind.
fn fence_predicate(fences: FenceLocations, identity: &AwaitEventRowIdentity) -> String {
    if identity.session_id.is_some() {
        "EXISTS(SELECT 1 FROM await_event_revoked_sessions WHERE session_id = ?2)".to_string()
    } else {
        fences.fenced_predicate("?2")
    }
}

/// The fence verdict for `identity` and the wait row stored under `key_id`, read
/// as one statement.
///
/// Every caller needs both before it decides anything, and asking them
/// separately cost a round trip per call for no extra isolation — both reads
/// already happened inside one transaction. The fence is a scalar subquery over
/// a one-row probe and the wait row is `LEFT JOIN`ed onto it, so a fenced key
/// with no stored row and an unfenced key with one are the same single read.
/// `scope_json` is `NOT NULL` in the table, so a NULL there is the join missing,
/// not a row with empty columns.
fn select_fence_and_wait_row(
    connection: &rusqlite::Connection,
    fences: FenceLocations,
    identity: &AwaitEventRowIdentity,
    key_id: &str,
) -> rusqlite::Result<(bool, Option<WaitRow>)> {
    let fence_subject = identity
        .session_id
        .as_deref()
        .unwrap_or(identity.scope_id.as_str());
    connection.query_row(
        &format!(
            "SELECT {},
                    stored.scope_json, stored.wait_json, stored.session_id,
                    stored.turn_control, stored.terminal_json
             FROM (SELECT 1) AS probe
             LEFT JOIN await_event_waits AS stored ON stored.key_id = ?1",
            fence_predicate(fences, identity)
        ),
        params![key_id, fence_subject],
        |row| {
            let fenced: bool = row.get(0)?;
            let Some(scope_json) = row.get::<_, Option<String>>(1)? else {
                return Ok((fenced, None));
            };
            Ok((
                fenced,
                Some(WaitRow {
                    scope_json,
                    wait_json: row.get(2)?,
                    session_id: row.get::<_, Option<String>>(3)?.map(SessionId::from),
                    turn_control: row.get(4)?,
                    terminal_json: row.get(5)?,
                }),
            ))
        },
    )
}

fn session_is_revoked(
    connection: &rusqlite::Connection,
    session_id: &SessionId,
) -> rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM await_event_revoked_sessions WHERE session_id = ?1
         )",
        params![session_id.as_str()],
        |row| row.get(0),
    )
}

fn store_error(err: rusqlite::Error) -> RuntimeError {
    RuntimeError::new(
        lash_core::RuntimeErrorCode::SqliteAwaitEventStore,
        err.to_string(),
    )
}
