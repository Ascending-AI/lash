//! Durable SQLite AwaitEvent promises.
//!
//! SQLite rows are the source of truth. The local notifier map is only a
//! latency hint; every waiter also polls persisted state with bounded backoff.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use lash_core::facade_support::promise_semantics::{self, PromiseState, PromiseTransition};
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome, RuntimeError,
};
use rusqlite::{OptionalExtension, params};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::conn::SqliteConnection;

type HmacSha256 = Hmac<sha2::Sha256>;

const INITIAL_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct SqliteAwaitEvents {
    conn: SqliteConnection,
    signing_secret: Arc<[u8]>,
    clock: Arc<dyn lash_core::Clock>,
    notifiers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

enum StoredPromise {
    Missing,
    Pending,
    Resolved(Resolution),
    UnknownOrRevoked,
}

#[derive(Clone)]
struct StoredIdentity {
    scope_json: String,
    wait_json: String,
    session_id: Option<String>,
    turn_control: bool,
}

impl SqliteAwaitEvents {
    pub(crate) fn new(
        conn: SqliteConnection,
        signing_secret: Vec<u8>,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Self {
        Self {
            conn,
            signing_secret: signing_secret.into(),
            clock,
            notifiers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn key_for(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        let key_id = promise_semantics::derive_key_id(scope, &wait)?;
        if let Some(session_id) = scope.session_id()
            && self.session_is_revoked(session_id).await?
        {
            return Err(unknown_or_revoked());
        }
        let signature = self.signature(scope, &wait, &key_id)?;
        Ok(AwaitEventKey {
            scope: scope.clone(),
            wait,
            key_id,
            signature,
        })
    }

    pub(crate) async fn resolve(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !self.authenticates(key)? {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        let identity = stored_identity(key)?;
        let key_id = key.key_id.clone();
        let proposed_json = encode_resolution(&resolution)?;
        let now = self.clock.timestamp_ms() as i64;
        let proposed = resolution.clone();
        let outcome = self
            .conn
            .write(move |tx| {
                if let Some(session_id) = identity.session_id.as_deref()
                    && session_is_revoked(tx, session_id)?
                {
                    return Ok(ResolveOutcome::UnknownOrRevoked);
                }

                let stored = select_wait_row(tx, &key_id)?;
                let is_missing = stored.is_none();
                let state = match stored {
                    None => PromiseState::Missing,
                    Some(ref row) if !row.matches(&identity) => {
                        return Ok(ResolveOutcome::UnknownOrRevoked);
                    }
                    Some(row) => match row.terminal_json {
                        Some(terminal_json) => {
                            PromiseState::Resolved(decode_resolution_sql(&terminal_json)?)
                        }
                        None => PromiseState::Pending,
                    },
                };

                match promise_semantics::resolve(state, proposed) {
                    PromiseTransition::Store(_) if is_missing => {
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
                        Ok(ResolveOutcome::Accepted)
                    }
                    PromiseTransition::Store(_) => {
                        let changed = tx.execute(
                            "UPDATE await_event_waits
                             SET terminal_json = ?2, resolved_at_ms = ?3
                             WHERE key_id = ?1 AND terminal_json IS NULL",
                            params![key_id, proposed_json, now],
                        )?;
                        if changed != 1 {
                            return Err(rusqlite::Error::InvalidQuery);
                        }
                        Ok(ResolveOutcome::Accepted)
                    }
                    transition => Ok(transition
                        .resolve_outcome()
                        .expect("resolve never returns the unchanged transition")),
                }
            })
            .await
            .map_err(store_error)?;
        if outcome == ResolveOutcome::Accepted {
            self.notify_key(&key.key_id)?;
        }
        Ok(outcome)
    }

    pub(crate) async fn peek(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        match self.inspect(key).await? {
            StoredPromise::Missing | StoredPromise::Pending => Ok(None),
            StoredPromise::Resolved(terminal) => Ok(Some(terminal)),
            StoredPromise::UnknownOrRevoked => Err(unknown_or_revoked()),
        }
    }

    pub(crate) async fn await_resolution(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        let clock = Arc::clone(&self.clock);
        self.await_resolution_with_clock(key, cancel, deadline, clock.as_ref())
            .await
    }

    pub(crate) async fn await_resolution_with_clock(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        clock: &dyn lash_core::Clock,
    ) -> Result<Resolution, RuntimeError> {
        let notify = self.notifier_for(&key.key_id)?;
        self.ensure_pending(key).await?;
        let mut backoff = INITIAL_POLL;
        loop {
            match self.inspect(key).await? {
                StoredPromise::Resolved(terminal) => return Ok(terminal),
                StoredPromise::UnknownOrRevoked => return Err(unknown_or_revoked()),
                StoredPromise::Missing | StoredPromise::Pending => {}
            }

            let poll = clock.sleep(backoff);
            tokio::pin!(poll);
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(
                                "turn_control_wait_cancelled",
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled).await?;
                    }
                    _ = clock.sleep_until(deadline) => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(
                                "turn_control_wait_timeout",
                                "turn-control waiter timed out without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Timeout).await?;
                    }
                    _ = notify.notified() => {
                        backoff = INITIAL_POLL;
                        continue;
                    }
                    _ = &mut poll => {}
                }
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if key.wait.is_turn_control() {
                            return Err(RuntimeError::new(
                                "turn_control_wait_cancelled",
                                "turn-control waiter stopped without resolving its keyed promise",
                            ));
                        }
                        let _ = self.resolve(key, Resolution::Cancelled).await?;
                    }
                    _ = notify.notified() => {
                        backoff = INITIAL_POLL;
                        continue;
                    }
                    _ = &mut poll => {}
                }
            }
            backoff = (backoff * 2).min(MAX_POLL);
        }
    }

    pub(crate) async fn revoke_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        validate_session_id(session_id)?;
        let session_id = session_id.to_string();
        let now = self.clock.timestamp_ms() as i64;
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
            .map_err(store_error)?;
        self.notify_all()?;
        Ok(())
    }

    pub(crate) async fn cancel_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        validate_session_id(session_id)?;
        let session_id = session_id.to_string();
        let terminal_json = encode_resolution(&Resolution::Cancelled)?;
        let now = self.clock.timestamp_ms() as i64;
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
            .map_err(store_error)?;
        self.notify_all()?;
        Ok(())
    }

    async fn ensure_pending(&self, key: &AwaitEventKey) -> Result<(), RuntimeError> {
        if !self.authenticates(key)? {
            return Err(unknown_or_revoked());
        }
        let identity = stored_identity(key)?;
        let key_id = key.key_id.clone();
        let now = self.clock.timestamp_ms() as i64;
        let accepted = self
            .conn
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
            .map_err(store_error)?;
        if accepted {
            Ok(())
        } else {
            Err(unknown_or_revoked())
        }
    }

    async fn inspect(&self, key: &AwaitEventKey) -> Result<StoredPromise, RuntimeError> {
        if !self.authenticates(key)? {
            return Ok(StoredPromise::UnknownOrRevoked);
        }
        let identity = stored_identity(key)?;
        let query_identity = identity.clone();
        let key_id = key.key_id.clone();
        let (revoked, stored) = self
            .conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let revoked = match query_identity.session_id.as_deref() {
                    Some(session_id) => session_is_revoked(&tx, session_id)?,
                    None => false,
                };
                let stored = select_wait_row(&tx, &key_id)?;
                tx.commit()?;
                Ok((revoked, stored))
            })
            .await
            .map_err(store_error)?;
        if revoked {
            return Ok(StoredPromise::UnknownOrRevoked);
        }
        let Some(stored) = stored else {
            return Ok(StoredPromise::Missing);
        };
        if !stored.matches(&identity) {
            return Ok(StoredPromise::UnknownOrRevoked);
        }
        stored.terminal_json.map_or_else(
            || Ok(StoredPromise::Pending),
            |json| decode_resolution(&json).map(StoredPromise::Resolved),
        )
    }

    async fn session_is_revoked(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |connection| session_is_revoked(connection, &session_id))
            .await
            .map_err(store_error)
    }

    fn signature(
        &self,
        scope: &ExecutionScope,
        wait: &AwaitEventWaitIdentity,
        key_id: &str,
    ) -> Result<String, RuntimeError> {
        let mut mac = HmacSha256::new_from_slice(&self.signing_secret).map_err(|err| {
            RuntimeError::new(
                "sqlite_await_event_sign",
                format!("failed to initialize SQLite await-event signer: {err}"),
            )
        })?;
        mac.update(&promise_semantics::sign_material(scope, wait, key_id));
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    fn authenticates(&self, key: &AwaitEventKey) -> Result<bool, RuntimeError> {
        let Ok(derived_key_id) = promise_semantics::derive_key_id(&key.scope, &key.wait) else {
            return Ok(false);
        };
        let expected_signature = self.signature(&key.scope, &key.wait, &key.key_id)?;
        let key_id_matches =
            promise_semantics::constant_time_eq(derived_key_id.as_bytes(), key.key_id.as_bytes());
        let signature_matches = promise_semantics::constant_time_eq(
            expected_signature.as_bytes(),
            key.signature.as_bytes(),
        );
        Ok(key_id_matches & signature_matches)
    }

    fn notifier_for(&self, key_id: &str) -> Result<Arc<Notify>, RuntimeError> {
        let mut notifiers = self.notifiers.lock().map_err(|_| notifier_error())?;
        Ok(Arc::clone(
            notifiers
                .entry(key_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new())),
        ))
    }

    fn notify_key(&self, key_id: &str) -> Result<(), RuntimeError> {
        if let Some(notify) = self
            .notifiers
            .lock()
            .map_err(|_| notifier_error())?
            .get(key_id)
        {
            notify.notify_waiters();
        }
        Ok(())
    }

    fn notify_all(&self) -> Result<(), RuntimeError> {
        for notify in self
            .notifiers
            .lock()
            .map_err(|_| notifier_error())?
            .values()
        {
            notify.notify_waiters();
        }
        Ok(())
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
    fn matches(&self, identity: &StoredIdentity) -> bool {
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

fn stored_identity(key: &AwaitEventKey) -> Result<StoredIdentity, RuntimeError> {
    Ok(StoredIdentity {
        scope_json: serde_json::to_string(&key.scope).map_err(encode_error)?,
        wait_json: serde_json::to_string(&key.wait).map_err(encode_error)?,
        session_id: key.scope.session_id().map(ToOwned::to_owned),
        turn_control: key.wait.is_turn_control(),
    })
}

fn encode_resolution(resolution: &Resolution) -> Result<String, RuntimeError> {
    serde_json::to_string(resolution).map_err(encode_error)
}

fn decode_resolution(encoded: &str) -> Result<Resolution, RuntimeError> {
    serde_json::from_str(encoded).map_err(|err| {
        RuntimeError::new(
            "sqlite_await_event_decode",
            format!("failed to decode SQLite await-event terminal: {err}"),
        )
    })
}

fn decode_resolution_sql(encoded: &str) -> rusqlite::Result<Resolution> {
    serde_json::from_str(encoded).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn validate_session_id(session_id: &str) -> Result<(), RuntimeError> {
    if session_id.trim().is_empty() {
        return Err(RuntimeError::new(
            "invalid_await_event_session_id",
            "await-event session id must be non-empty",
        ));
    }
    Ok(())
}

fn unknown_or_revoked() -> RuntimeError {
    RuntimeError::new(
        "await_event_unknown_or_revoked",
        "await-event key is invalid or revoked",
    )
}

fn store_error(err: rusqlite::Error) -> RuntimeError {
    RuntimeError::new("sqlite_await_event_store", err.to_string())
}

fn encode_error(err: serde_json::Error) -> RuntimeError {
    RuntimeError::new(
        "sqlite_await_event_encode",
        format!("failed to encode SQLite await-event state: {err}"),
    )
}

fn notifier_error() -> RuntimeError {
    RuntimeError::new(
        "sqlite_await_event_notify",
        "SQLite await-event notifier lock poisoned",
    )
}
