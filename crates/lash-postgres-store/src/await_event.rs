//! PostgreSQL storage atoms for durable AwaitEvent promises.
//!
//! The promise state machine lives in [`AwaitEventCoordinator`]; this module is
//! only the PostgreSQL half of its backend port. Every atom runs in a server
//! transaction that first takes a per-session advisory lock, so the tombstone
//! check, the identity comparison, and the write they guard cannot interleave
//! under `READ COMMITTED`.

use std::sync::Arc;

use lash_core::facade_support::await_event_coordinator::{
    AwaitEventBackend, AwaitEventCoordinator, AwaitEventRowIdentity, AwaitEventVocabulary,
    PersistedPromise, TerminalCas,
};
use lash_core::{RuntimeError, RuntimeErrorCode};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Executor, Row as _};

const SESSION_LOCK_NAMESPACE: i64 = 562;

const VOCABULARY: AwaitEventVocabulary = AwaitEventVocabulary {
    sign: RuntimeErrorCode::PostgresAwaitEventSign,
    encode: RuntimeErrorCode::PostgresAwaitEventEncode,
    decode: RuntimeErrorCode::PostgresAwaitEventDecode,
    notify: RuntimeErrorCode::PostgresAwaitEventNotify,
    display_name: "PostgreSQL",
};

/// The PostgreSQL promise coordinator: one shared state machine over
/// [`PostgresAwaitEventBackend`].
pub(crate) type PostgresAwaitEvents = AwaitEventCoordinator<PostgresAwaitEventBackend>;

/// Build the PostgreSQL await-event coordinator over `pool`.
///
/// PostgreSQL await-event rows are stamped from `clock`, which the sole call
/// site hardwires to the wall clock because `PostgresStorage` carries no
/// injectable time source. These stamps are records, not decision inputs: lease
/// and claim decisions that must survive host clock skew read the server clock
/// instead — the database-authoritative lease boundary the `Clock` contract
/// states, pinned by `postgres_clock_contract`.
pub(crate) fn postgres_await_events(
    pool: PgPool,
    signing_secret: Arc<[u8]>,
    clock: Arc<dyn lash_core::Clock>,
) -> PostgresAwaitEvents {
    AwaitEventCoordinator::new(PostgresAwaitEventBackend { pool }, signing_secret, clock)
}

#[derive(Clone)]
pub(crate) struct PostgresAwaitEventBackend {
    pool: PgPool,
}

#[async_trait::async_trait]
impl AwaitEventBackend for PostgresAwaitEventBackend {
    fn vocabulary(&self) -> AwaitEventVocabulary {
        VOCABULARY.clone()
    }

    async fn session_is_revoked(&self, session_id: &str) -> Result<bool, RuntimeError> {
        session_is_revoked(&self.pool, session_id).await
    }

    async fn ensure_pending(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        now_ms: u64,
    ) -> Result<bool, RuntimeError> {
        let now = now_ms as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        if let Some(session_id) = identity.session_id.as_deref()
            && session_is_revoked(&mut *tx, session_id).await?
        {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO lash_await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES ($1, $2, $3, $4, $5, NULL, $6, NULL)
             ON CONFLICT (key_id) DO NOTHING",
        )
        .bind(key_id)
        .bind(&identity.scope_json)
        .bind(&identity.wait_json)
        .bind(&identity.session_id)
        .bind(identity.turn_control)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        let accepted = select_wait_row(&mut *tx, key_id)
            .await?
            .is_some_and(|row| row.matches(identity));
        tx.commit().await.map_err(store_error)?;
        Ok(accepted)
    }

    async fn store_terminal(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<TerminalCas, RuntimeError> {
        let now = now_ms as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        if let Some(session_id) = identity.session_id.as_deref()
            && session_is_revoked(&mut *tx, session_id).await?
        {
            return Ok(TerminalCas::UnknownOrRevoked);
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
        .bind(key_id)
        .bind(&identity.scope_json)
        .bind(&identity.wait_json)
        .bind(&identity.session_id)
        .bind(identity.turn_control)
        .bind(terminal_json)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?;
        let cas = if inserted.is_some() {
            TerminalCas::Stored
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
            .bind(key_id)
            .bind(&identity.scope_json)
            .bind(&identity.wait_json)
            .bind(&identity.session_id)
            .bind(identity.turn_control)
            .bind(terminal_json)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_error)?;
            if updated.is_some() {
                TerminalCas::Stored
            } else {
                match select_wait_row(&mut *tx, key_id).await? {
                    Some(row) if row.matches(identity) => match row.terminal_json {
                        Some(terminal_json) => TerminalCas::AlreadyResolved { terminal_json },
                        None => {
                            return Err(RuntimeError::new(
                                lash_core::RuntimeErrorCode::PostgresAwaitEventStore,
                                "await-event CAS lost without a winning terminal",
                            ));
                        }
                    },
                    _ => TerminalCas::UnknownOrRevoked,
                }
            }
        };
        tx.commit().await.map_err(store_error)?;
        Ok(cas)
    }

    async fn inspect(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
    ) -> Result<PersistedPromise, RuntimeError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, identity.session_id.as_deref()).await?;
        let revoked = match identity.session_id.as_deref() {
            Some(session_id) => session_is_revoked(&mut *tx, session_id).await?,
            None => false,
        };
        let stored = select_wait_row(&mut *tx, key_id).await?;
        tx.commit().await.map_err(store_error)?;
        if revoked {
            return Ok(PersistedPromise::UnknownOrRevoked);
        }
        let Some(stored) = stored else {
            return Ok(PersistedPromise::Missing);
        };
        if !stored.matches(identity) {
            return Ok(PersistedPromise::UnknownOrRevoked);
        }
        Ok(stored
            .terminal_json
            .map_or(PersistedPromise::Pending, |terminal_json| {
                PersistedPromise::Resolved { terminal_json }
            }))
    }

    async fn revoke_session(&self, session_id: &str, now_ms: u64) -> Result<(), RuntimeError> {
        let now = now_ms as i64;
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
        tx.commit().await.map_err(store_error)
    }

    async fn cancel_session_promises(
        &self,
        session_id: &str,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let now = now_ms as i64;
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
        tx.commit().await.map_err(store_error)
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

/// Serialize every promise atom for one session against its peers.
///
/// `READ COMMITTED` cannot make "check the tombstone, then write the row"
/// atomic on its own: a concurrent revocation would commit between the two
/// statements and the write would survive its own session's deletion. Session-free
/// scopes have no tombstone to race with, so they take no lock.
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

fn store_error(err: sqlx::Error) -> RuntimeError {
    RuntimeError::new(
        lash_core::RuntimeErrorCode::PostgresAwaitEventStore,
        err.to_string(),
    )
}
