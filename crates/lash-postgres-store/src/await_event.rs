//! PostgreSQL storage atoms for durable AwaitEvent promises.
//!
//! The promise state machine lives in [`AwaitEventCoordinator`]; this module is
//! only the PostgreSQL half of its backend port. Every atom runs in a server
//! transaction that first takes a per-session advisory lock, so the tombstone
//! check, the identity comparison, and the write they guard cannot interleave
//! under `READ COMMITTED`.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::facade_support::await_event_coordinator::{
    AwaitEventBackend, AwaitEventCoordinator, AwaitEventRowIdentity, AwaitEventVocabulary,
    PersistedPromise, RegisteredAwaitEvent, TerminalCas,
};
use lash_core::{RuntimeError, RuntimeErrorCode};
use lash_store_sql::Dialect;
use lash_store_sql::wait::revoked_sessions::RevokedSessionStatements;
use lash_store_sql::wait::waits::{WaitRow, WaitStatements};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Executor, Row as _};
use std::sync::LazyLock;

const SESSION_LOCK_NAMESPACE: i64 = 562;
/// Advisory-lock namespace for session-free scopes, disjoint from the session
/// namespace so a process or runtime-operation scope never contends with a
/// session whose id happens to hash alike.
pub(crate) const SCOPE_LOCK_NAMESPACE: i64 = 563;

const VOCABULARY: AwaitEventVocabulary = AwaitEventVocabulary {
    sign: RuntimeErrorCode::PostgresAwaitEventSign,
    encode: RuntimeErrorCode::PostgresAwaitEventEncode,
    decode: RuntimeErrorCode::PostgresAwaitEventDecode,
    notify: RuntimeErrorCode::PostgresAwaitEventNotify,
    display_name: "PostgreSQL",
};

lash_store_sql::statements! {
    /// `await_event_waits` statements only PostgreSQL issues.
    pub(crate) struct WaitPostgresStatements @ "await_event_wait" {
        /// Register a pending promise, keeping any row already under the key.
        ///
        /// `ON CONFLICT DO NOTHING` is the fork: `READ COMMITTED` cannot make
        /// "read the absence, then insert" atomic, so the conflict is what
        /// detects a concurrent registrar. SQLite reads the absence under its
        /// write lock and needs no clause.
        insert_pending = "INSERT INTO await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)
             ON CONFLICT (key_id) DO NOTHING";

        /// Register a promise that is already resolved, reporting the key when
        /// this caller is the registrar. Forks for the same reason
        /// [`WaitPostgresStatements::insert_pending`] does.
        insert_resolved = "INSERT INTO await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT (key_id) DO NOTHING
             RETURNING key_id";

        /// Settle the pending promise under key `?1` with terminal `?6` at
        /// `?7`, if the stored row is still that exact promise.
        ///
        /// FENCING (FIG-3381): the identity comparison rides in the predicate
        /// because `READ COMMITTED` cannot hold a read of it across
        /// statements. SQLite compares the row it read under the write lock
        /// and carries no identity columns here. The two semantics are
        /// deliberately left as they stand.
        resolve_pending = "UPDATE await_event_waits
                 SET terminal_json = ?6, resolved_at_ms = ?7
                 WHERE key_id = ?1
                   AND scope_json = ?2
                   AND wait_json = ?3
                   AND session_id IS NOT DISTINCT FROM ?4
                   AND turn_control = ?5
                   AND terminal_json IS NULL
                 RETURNING terminal_json";

        /// Cancel session `?1`'s unresolved non-control promises. PostgreSQL
        /// stores `turn_control` as a boolean, SQLite as an integer.
        cancel_session_promises = "UPDATE await_event_waits
             SET terminal_json = ?2, resolved_at_ms = ?3
             WHERE session_id = ?1
               AND terminal_json IS NULL
               AND turn_control = FALSE";
    }
}

lash_store_sql::statements! {
    /// `await_event_meta` statements only PostgreSQL issues.
    pub(crate) struct MetaPostgresStatements @ "await_event_meta" {
        /// The promise signing secret. PostgreSQL stores the singleton flag
        /// as a boolean, SQLite as an integer.
        select_signing_secret = "SELECT signing_secret FROM await_event_meta WHERE singleton = TRUE";
    }
}

/// Every wait-family statement, rendered once.
pub(crate) struct WaitSql {
    /// `await_event_waits` statements both backends issue verbatim.
    pub(crate) shared: WaitStatements,
    /// `await_event_waits` statements only PostgreSQL issues.
    pub(crate) postgres: WaitPostgresStatements,
    /// `await_event_revoked_sessions` statements both backends issue verbatim.
    pub(crate) revoked: RevokedSessionStatements,
    /// `await_event_meta` statements only PostgreSQL issues.
    pub(crate) meta_postgres: MetaPostgresStatements,
}

static WAIT_SQL: LazyLock<WaitSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    WaitSql {
        shared: WaitStatements::render(dialect),
        postgres: WaitPostgresStatements::render(dialect),
        revoked: RevokedSessionStatements::render(dialect),
        meta_postgres: MetaPostgresStatements::render(dialect),
    }
});

/// The wait-family statements, rendered once at first use and never again.
pub(crate) fn wait_sql() -> &'static WaitSql {
    &WAIT_SQL
}

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

/// `pub` only because it names an associated type of the shared replay
/// adapter; the module is private, so nothing outside this crate can reach it.
#[derive(Clone)]
pub struct PostgresAwaitEventBackend {
    pool: PgPool,
}

#[async_trait::async_trait]
impl AwaitEventBackend for PostgresAwaitEventBackend {
    fn vocabulary(&self) -> AwaitEventVocabulary {
        VOCABULARY.clone()
    }

    async fn session_is_revoked(&self, session_id: &SessionId) -> Result<bool, RuntimeError> {
        session_is_revoked(&self.pool, session_id).await
    }

    async fn scope_is_retired(&self, scope_id: &str) -> Result<bool, RuntimeError> {
        scope_is_retired(&self.pool, scope_id).await
    }

    async fn ensure_pending(
        &self,
        key_id: &str,
        identity: &AwaitEventRowIdentity,
        now_ms: u64,
    ) -> Result<bool, RuntimeError> {
        let now = now_ms as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_identity(&mut tx, identity).await?;
        if identity_is_fenced(&mut tx, identity).await? {
            return Ok(false);
        }
        sqlx::query(wait_sql().postgres.insert_pending.sql())
            .bind(key_id)
            .bind(&identity.scope_json)
            .bind(&identity.wait_json)
            .bind(identity.session_id.as_deref())
            .bind(identity.turn_control)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        let accepted = select_wait_row(&mut *tx, key_id)
            .await?
            .is_some_and(|row| matches_identity(&row, identity));
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
        lock_identity(&mut tx, identity).await?;
        if identity_is_fenced(&mut tx, identity).await? {
            return Ok(TerminalCas::UnknownOrRevoked);
        }

        let inserted: Option<String> =
            sqlx::query_scalar(wait_sql().postgres.insert_resolved.sql())
                .bind(key_id)
                .bind(&identity.scope_json)
                .bind(&identity.wait_json)
                .bind(identity.session_id.as_deref())
                .bind(identity.turn_control)
                .bind(terminal_json)
                .bind(now)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_error)?;
        let cas = if inserted.is_some() {
            TerminalCas::Stored
        } else {
            let updated: Option<String> =
                sqlx::query_scalar(wait_sql().postgres.resolve_pending.sql())
                    .bind(key_id)
                    .bind(&identity.scope_json)
                    .bind(&identity.wait_json)
                    .bind(identity.session_id.as_deref())
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
                    Some(row) if matches_identity(&row, identity) => match row.terminal_json {
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
        lock_identity(&mut tx, identity).await?;
        let revoked = identity_is_fenced(&mut tx, identity).await?;
        let stored = select_wait_row(&mut *tx, key_id).await?;
        tx.commit().await.map_err(store_error)?;
        if revoked {
            return Ok(PersistedPromise::UnknownOrRevoked);
        }
        let Some(stored) = stored else {
            return Ok(PersistedPromise::Missing);
        };
        if !matches_identity(&stored, identity) {
            return Ok(PersistedPromise::UnknownOrRevoked);
        }
        Ok(stored
            .terminal_json
            .map_or(PersistedPromise::Pending, |terminal_json| {
                PersistedPromise::Resolved { terminal_json }
            }))
    }

    async fn list_pending_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<RegisteredAwaitEvent>, RuntimeError> {
        sqlx::query(wait_sql().shared.list_pending_for_session.sql())
            .bind(session_id.as_str())
            .fetch_all(&self.pool)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| RegisteredAwaitEvent {
                        key_id: row.get("key_id"),
                        scope_json: row.get("scope_json"),
                        wait_json: row.get("wait_json"),
                        turn_control: row.get("turn_control"),
                    })
                    .collect()
            })
            .map_err(store_error)
    }

    async fn revoke_session(
        &self,
        session_id: &SessionId,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let now = now_ms as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, Some(session_id)).await?;
        sqlx::query(wait_sql().revoked.insert_ignore.sql())
            .bind(session_id.as_str())
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        sqlx::query(wait_sql().shared.delete_by_session.sql())
            .bind(session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)
    }

    async fn cancel_session_promises(
        &self,
        session_id: &SessionId,
        terminal_json: &str,
        now_ms: u64,
    ) -> Result<(), RuntimeError> {
        let now = now_ms as i64;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        lock_session(&mut tx, Some(session_id)).await?;
        sqlx::query(wait_sql().postgres.cancel_session_promises.sql())
            .bind(session_id.as_str())
            .bind(terminal_json)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)
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

async fn select_wait_row<'e, E>(executor: E, key_id: &str) -> Result<Option<WaitRow>, RuntimeError>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(wait_sql().shared.select_by_key.sql())
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

async fn session_is_revoked<'e, E>(
    executor: E,
    session_id: &SessionId,
) -> Result<bool, RuntimeError>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(wait_sql().revoked.exists.sql())
        .bind(session_id.as_str())
        .fetch_one(executor)
        .await
        .map_err(store_error)
}

/// Whether either durable fence refuses `identity`: the owning session's
/// revocation tombstone, or the scope's retirement tombstone. Read inside the
/// transaction that holds the identity's advisory lock.
async fn identity_is_fenced(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    identity: &AwaitEventRowIdentity,
) -> Result<bool, RuntimeError> {
    if let Some(session_id) = identity.session_id.as_deref()
        && session_is_revoked(&mut **tx, &SessionId::from(session_id)).await?
    {
        return Ok(true);
    }
    scope_is_retired(&mut **tx, &identity.scope_id).await
}

pub(crate) async fn scope_is_retired<'e, E>(
    executor: E,
    scope_id: &str,
) -> Result<bool, RuntimeError>
where
    E: Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query_scalar(crate::effect_replay::effect_sql().fence.exists.sql())
        .bind(scope_id)
        .fetch_one(executor)
        .await
        .map_err(store_error)
}

/// Serialize a promise atom against every peer that can fence it: the session
/// lock for session scopes, the scope lock for session-free scopes (whose
/// fence is the scope-retirement tombstone that retirement writes under the
/// same lock).
async fn lock_identity(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    identity: &AwaitEventRowIdentity,
) -> Result<(), RuntimeError> {
    match identity.session_id.as_deref() {
        Some(_) => lock_session(tx, identity.session_id.as_ref()).await,
        None => lock_scope(tx, &identity.scope_id)
            .await
            .map_err(|err| store_error_message(err.to_string())),
    }
}

/// Serialize every fence-sensitive atom for one session-free scope against
/// its peers, retirement included. Shared with the effect-replay row store,
/// which takes the same lock before reading the tombstone on claim, group
/// open, and retirement.
pub(crate) async fn lock_scope(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
        .bind(scope_id)
        .bind(SCOPE_LOCK_NAMESPACE)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Serialize every promise atom for one session against its peers.
///
/// `READ COMMITTED` cannot make "check the tombstone, then write the row"
/// atomic on its own: a concurrent revocation would commit between the two
/// statements and the write would survive its own session's deletion. Session-free
/// scopes take the scope lock instead (see [`lock_identity`]).
async fn lock_session(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Option<&SessionId>,
) -> Result<(), RuntimeError> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, $2))")
        .bind(session_id.as_str())
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

fn store_error_message(message: String) -> RuntimeError {
    RuntimeError::new(
        lash_core::RuntimeErrorCode::PostgresAwaitEventStore,
        message,
    )
}
