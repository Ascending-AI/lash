//! Durable PostgreSQL AwaitEvent promises.
//!
//! PostgreSQL rows are the source of truth. The local notifier map is only a
//! latency hint; every waiter also polls persisted state with bounded backoff.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use lash_core::facade_support::promise_semantics;
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome, RuntimeError,
};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Executor, Row as _};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type HmacSha256 = Hmac<sha2::Sha256>;

const INITIAL_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_secs(1);
const SESSION_LOCK_NAMESPACE: i64 = 562;

#[derive(Clone)]
pub(crate) struct PostgresAwaitEvents {
    pool: PgPool,
    signing_secret: Arc<[u8]>,
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

impl PostgresAwaitEvents {
    pub(crate) fn new(pool: PgPool, signing_secret: Arc<[u8]>) -> Self {
        Self {
            pool,
            signing_secret,
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
        let proposed_json = encode_resolution(&resolution)?;
        let now = current_epoch_ms() as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        if let Some(session_id) = identity.session_id.as_deref()
            && session_is_revoked(&mut *tx, session_id).await?
        {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }

        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO lash_await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
             ON CONFLICT (key_id) DO NOTHING
             RETURNING key_id",
        )
        .bind(&key.key_id)
        .bind(&identity.scope_json)
        .bind(&identity.wait_json)
        .bind(&identity.session_id)
        .bind(identity.turn_control)
        .bind(&proposed_json)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let outcome = if inserted.is_some() {
            ResolveOutcome::Accepted
        } else {
            let updated: Option<String> = sqlx::query_scalar(
                "UPDATE lash_await_event_waits
                 SET terminal_json = $6, resolved_at_ms = $7
                 WHERE key_id = $1
                   AND scope_json = $2
                   AND wait_json = $3
                   AND session_id IS NOT DISTINCT FROM $4
                   AND turn_control = $5
                   AND terminal_json IS NULL
                 RETURNING terminal_json",
            )
            .bind(&key.key_id)
            .bind(&identity.scope_json)
            .bind(&identity.wait_json)
            .bind(&identity.session_id)
            .bind(identity.turn_control)
            .bind(&proposed_json)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_error)?;
            if updated.is_some() {
                ResolveOutcome::Accepted
            } else {
                let stored = select_wait_row(&mut *tx, &key.key_id).await?;
                match stored {
                    Some(row) if row.matches(&identity) => match row.terminal_json {
                        Some(terminal_json) => ResolveOutcome::AlreadyResolved {
                            terminal: decode_resolution(&terminal_json)?,
                        },
                        None => {
                            return Err(RuntimeError::new(
                                "postgres_await_event_store",
                                "await-event CAS lost without a winning terminal",
                            ));
                        }
                    },
                    _ => ResolveOutcome::UnknownOrRevoked,
                }
            }
        };
        tx.commit().await.map_err(store_error)?;
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
        self.await_resolution_with_clock(
            key,
            cancel,
            deadline,
            &lash_core::facade_support::SystemClock,
        )
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
        let now = current_epoch_ms() as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, Some(session_id)).await?;
        sqlx::query(
            "INSERT INTO lash_await_event_revoked_sessions (session_id, revoked_at_ms)
             VALUES ($1, $2)
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        sqlx::query("DELETE FROM lash_await_event_waits WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        self.notify_all()?;
        Ok(())
    }

    pub(crate) async fn cancel_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        validate_session_id(session_id)?;
        let terminal_json = encode_resolution(&Resolution::Cancelled)?;
        let now = current_epoch_ms() as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, Some(session_id)).await?;
        sqlx::query(
            "UPDATE lash_await_event_waits
             SET terminal_json = $2, resolved_at_ms = $3
             WHERE session_id = $1
               AND terminal_json IS NULL
               AND turn_control = FALSE",
        )
        .bind(session_id)
        .bind(terminal_json)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        self.notify_all()?;
        Ok(())
    }

    async fn ensure_pending(&self, key: &AwaitEventKey) -> Result<(), RuntimeError> {
        if !self.authenticates(key)? {
            return Err(unknown_or_revoked());
        }
        let identity = stored_identity(key)?;
        let now = current_epoch_ms() as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        if let Some(session_id) = identity.session_id.as_deref()
            && session_is_revoked(&mut *tx, session_id).await?
        {
            return Err(unknown_or_revoked());
        }
        sqlx::query(
            "INSERT INTO lash_await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES ($1, $2, $3, $4, $5, NULL, $6, NULL)
             ON CONFLICT (key_id) DO NOTHING",
        )
        .bind(&key.key_id)
        .bind(&identity.scope_json)
        .bind(&identity.wait_json)
        .bind(&identity.session_id)
        .bind(identity.turn_control)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        let accepted = select_wait_row(&mut *tx, &key.key_id)
            .await?
            .is_some_and(|row| row.matches(&identity));
        tx.commit().await.map_err(store_error)?;
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
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        let revoked = match identity.session_id.as_deref() {
            Some(session_id) => session_is_revoked(&mut *tx, session_id).await?,
            None => false,
        };
        let stored = select_wait_row(&mut *tx, &key.key_id).await?;
        tx.commit().await.map_err(store_error)?;
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
        session_is_revoked(&self.pool, session_id).await
    }

    fn signature(
        &self,
        scope: &ExecutionScope,
        wait: &AwaitEventWaitIdentity,
        key_id: &str,
    ) -> Result<String, RuntimeError> {
        let mut mac = HmacSha256::new_from_slice(&self.signing_secret).map_err(|err| {
            RuntimeError::new(
                "postgres_await_event_sign",
                format!("failed to initialize PostgreSQL await-event signer: {err}"),
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

async fn select_wait_row<'e, E>(executor: E, key_id: &str) -> Result<Option<WaitRow>, RuntimeError>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(
        "SELECT scope_json, wait_json, session_id, turn_control, terminal_json
         FROM lash_await_event_waits
         WHERE key_id = $1",
    )
    .bind(key_id)
    .fetch_optional(executor)
    .await
    .map_err(store_error)?;
    Ok(row.map(wait_row))
}

fn wait_row(row: PgRow) -> WaitRow {
    WaitRow {
        scope_json: row.get("scope_json"),
        wait_json: row.get("wait_json"),
        session_id: row.get("session_id"),
        turn_control: row.get("turn_control"),
        terminal_json: row.get("terminal_json"),
    }
}

async fn session_is_revoked<'e, E>(executor: E, session_id: &str) -> Result<bool, RuntimeError>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM lash_await_event_revoked_sessions WHERE session_id = $1
         )",
    )
    .bind(session_id)
    .fetch_one(executor)
    .await
    .map_err(store_error)
}

async fn lock_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
        .bind(session_id)
        .bind(SESSION_LOCK_NAMESPACE)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    Ok(())
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
            "postgres_await_event_decode",
            format!("failed to decode PostgreSQL await-event terminal: {err}"),
        )
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

fn store_error(err: sqlx::Error) -> RuntimeError {
    RuntimeError::new("postgres_await_event_store", err.to_string())
}

fn encode_error(err: serde_json::Error) -> RuntimeError {
    RuntimeError::new(
        "postgres_await_event_encode",
        format!("failed to encode PostgreSQL await-event state: {err}"),
    )
}

fn notifier_error() -> RuntimeError {
    RuntimeError::new(
        "postgres_await_event_notify",
        "PostgreSQL await-event notifier lock poisoned",
    )
}

fn current_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_millis() as u64
}
