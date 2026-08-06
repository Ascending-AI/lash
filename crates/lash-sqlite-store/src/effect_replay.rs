//! SQLite-backed runtime effect replay host.
//!
//! The claim/execute/renew/finalize state machine lives in
//! [`EffectReplayDriver`]; this module is the SQLite half of its persistence
//! port plus the host and controller types that expose it. Every atom runs
//! inside `SqliteConnection::write` (`BEGIN IMMEDIATE`), so the read, the
//! transition decision, and the write it guards take the cross-process write
//! lock up front and cannot interleave with a competing claimant.
//!
//! SQLite's authoritative lease clock is the host's injected
//! [`Clock`](lash_core::Clock): this store runs in the same clock domain as its
//! host, and every other durable stamp in the crate already comes from there.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use lash_core::facade_support::effect_replay_driver;
use lash_core::facade_support::effect_replay_driver::{
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectLeaseFence,
    EffectLeaseStamp, EffectReplayDriver, EffectReplayPersistence, EffectReplayVocabulary,
    EffectRowStatus, EffectTerminal, StoredEffectRow, decide_effect_claim,
};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, EffectJournalRetirement,
    ExecutionScope, Resolution, ResolveOutcome, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, ScopedEffectController, facade_support::LeaseTimings,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::await_event::{SqliteAwaitEventBackend, sqlite_await_events};

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary {
    code_prefix: "sqlite",
};

/// The SQLite effect-replay driver: one shared state machine over
/// [`SqliteEffectReplayPersistence`].
type SqliteEffectReplay =
    EffectReplayDriver<SqliteEffectReplayPersistence, SqliteAwaitEventBackend>;

/// Options for SQLite-backed runtime effect replay.
#[derive(Clone, Debug, Default)]
pub struct SqliteEffectReplayOptions {
    /// Effect-replay lease timing capability. Hosts share the same
    /// [`LeaseTimings`] they configure on the runtime so effect leases expire
    /// on the same failover window as session and process leases.
    pub lease_timings: LeaseTimings,
}

/// Deployment-level SQLite effect host.
///
/// This host persists runtime effect history in a local SQLite database and
/// returns scoped controllers that replay completed outcomes by
/// `(scope_id, replay_key)`.
#[derive(Clone)]
pub struct SqliteEffectHost {
    inner: Arc<SqliteEffectReplay>,
}

/// Scoped SQLite-backed runtime effect controller.
#[derive(Clone)]
pub struct SqliteRuntimeEffectController {
    inner: Arc<SqliteEffectReplay>,
    scope: ExecutionScope,
    allows_process_lifetime_completion_keys: bool,
}

impl SqliteEffectHost {
    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options(path, SqliteEffectReplayOptions::default()).await
    }

    pub async fn open_with_clock(
        path: &Path,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(path, SqliteEffectReplayOptions::default(), clock).await
    }

    pub async fn open_with_options(
        path: &Path,
        options: SqliteEffectReplayOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(
            path,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_effect_host_path(path)?;
        Ok(Self {
            inner: open_effect_replay_driver(path, StoreBacking::File, options, clock).await?,
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for SqliteEffectHost {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for SqliteEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate()?;
        let controller = SqliteRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
            allows_process_lifetime_completion_keys: true,
        };
        ScopedEffectController::shared(Arc::new(controller), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scope.validate()?;
        let controller = SqliteRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
            allows_process_lifetime_completion_keys: true,
        };
        Ok(Some(ScopedEffectController::shared(
            Arc::new(controller),
            scope,
        )?))
    }

    async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.inner.retire_effect_journal(retirement).await
    }
}

impl SqliteRuntimeEffectController {
    pub async fn open(path: &Path, scope: ExecutionScope) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options(path, scope, SqliteEffectReplayOptions::default()).await
    }

    pub async fn open_with_clock(
        path: &Path,
        scope: ExecutionScope,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(path, scope, SqliteEffectReplayOptions::default(), clock)
            .await
    }

    pub async fn open_with_options(
        path: &Path,
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(
            path,
            scope,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_effect_host_path(path)?;
        Ok(Self {
            inner: open_effect_replay_driver(path, StoreBacking::File, options, clock).await?,
            scope,
            allows_process_lifetime_completion_keys: true,
        })
    }

    #[cfg(feature = "testing")]
    pub async fn memory(scope: ExecutionScope) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options(scope, SqliteEffectReplayOptions::default()).await
    }

    #[cfg(feature = "testing")]
    pub async fn memory_with_clock(
        scope: ExecutionScope,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(scope, SqliteEffectReplayOptions::default(), clock)
            .await
    }

    #[cfg(feature = "testing")]
    pub async fn memory_with_options(
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(
            scope,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub async fn memory_with_options_and_clock(
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Ok(Self {
            inner: open_effect_replay_memory_driver(options, clock).await?,
            scope,
            allows_process_lifetime_completion_keys: false,
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for SqliteRuntimeEffectController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.allows_process_lifetime_completion_keys
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for SqliteRuntimeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.inner
            .execute_effect(&self.scope, envelope, local_executor)
            .await
    }
}

fn validate_effect_host_path(path: &Path) -> tokio_rusqlite::Result<()> {
    let rendered = path.to_string_lossy();
    if path.as_os_str().is_empty() || rendered == ":memory:" || rendered.starts_with("file:") {
        return Err(tokio_rusqlite::Error::Error(
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!(
                    "SqliteEffectHost requires a file-backed database path, got `{rendered}`"
                )),
            ),
        ));
    }
    Ok(())
}

async fn open_effect_replay_driver(
    path: &Path,
    backing: StoreBacking,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open(path).await?;
    let signing_secret = ensure_effect_schema(&conn).await?;
    apply_pragmas(&conn, backing).await?;
    Ok(Arc::new(build_effect_replay_driver(
        conn,
        options,
        clock,
        signing_secret,
    )))
}

#[cfg(feature = "testing")]
async fn open_effect_replay_memory_driver(
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open_in_memory().await?;
    let signing_secret = ensure_effect_schema(&conn).await?;
    apply_pragmas(&conn, StoreBacking::Memory).await?;
    Ok(Arc::new(build_effect_replay_driver(
        conn,
        options,
        clock,
        signing_secret,
    )))
}

fn build_effect_replay_driver(
    conn: SqliteConnection,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
    signing_secret: Vec<u8>,
) -> SqliteEffectReplay {
    let await_events = sqlite_await_events(conn.clone(), signing_secret, Arc::clone(&clock));
    EffectReplayDriver::new(
        SqliteEffectReplayPersistence {
            conn,
            clock: Arc::clone(&clock),
        },
        await_events,
        clock,
        options.lease_timings,
    )
}

/// SQLite storage atoms for the durable effect journal.
struct SqliteEffectReplayPersistence {
    conn: SqliteConnection,
    /// SQLite's authoritative lease clock, shared with the driver's sleep clock
    /// because the store and its host share one clock domain.
    clock: Arc<dyn lash_core::Clock>,
}

impl effect_replay_driver::sealed::EffectReplayBackend for SqliteEffectReplayPersistence {}

#[async_trait::async_trait]
impl EffectReplayPersistence for SqliteEffectReplayPersistence {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let request = request.clone();
        // Read the lease clock before entering the connection thread: the
        // closure is synchronous and the injected clock is the store's
        // authoritative instant either way.
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                let row = select_effect_row(tx, &request.scope_id, &request.replay_key)?;
                Ok(match decide_effect_claim(row.as_ref(), &request, now_ms) {
                    EffectClaimDecision::Insert(stamp) => {
                        insert_claimed_row(tx, &request, &stamp)?;
                        EffectClaimObservation::Claimed {
                            due_at_ms: stamp.due_at_ms,
                        }
                    }
                    EffectClaimDecision::TakeOver(stamp) => {
                        take_over_expired_lease(tx, &request, &stamp)?;
                        EffectClaimObservation::Claimed {
                            due_at_ms: stamp.due_at_ms,
                        }
                    }
                    EffectClaimDecision::Report(observation) => observation,
                })
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let fence = fence.clone();
        let status = terminal.status().column();
        let outcome_json = terminal.outcome_json().map(str::to_string);
        let error_json = terminal.error_json().map(str::to_string);
        let now = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                let changed = tx.execute(
                    "UPDATE runtime_effect_replay
                     SET status = ?6,
                         outcome_json = ?7,
                         error_json = ?8,
                         lease_owner_id = NULL,
                         lease_token = NULL,
                         lease_expires_at_ms = 0,
                         updated_at_ms = ?9
                     WHERE scope_id = ?1
                       AND replay_key = ?2
                       AND envelope_hash = ?3
                       AND lease_owner_id = ?4
                       AND lease_token = ?5
                       AND status = 'in_progress'
                       AND lease_expires_at_ms > ?10",
                    params![
                        fence.scope_id.as_str(),
                        fence.replay_key.as_str(),
                        fence.envelope_hash.as_str(),
                        fence.owner_id.as_str(),
                        fence.lease_token.as_str(),
                        status,
                        outcome_json,
                        error_json,
                        now as i64,
                        now as i64,
                    ],
                )?;
                Ok(changed == 1)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn renew(
        &self,
        fence: &EffectLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let fence = fence.clone();
        let now = self.clock.timestamp_ms();
        let renewed_expires_at = now.saturating_add(lease_ttl_ms);
        self.conn
            .write(move |tx| {
                let changed = tx.execute(
                    "UPDATE runtime_effect_replay
                     SET lease_expires_at_ms = ?6,
                         updated_at_ms = ?7
                     WHERE scope_id = ?1
                       AND replay_key = ?2
                       AND envelope_hash = ?3
                       AND lease_owner_id = ?4
                       AND lease_token = ?5
                       AND status = 'in_progress'
                       AND lease_expires_at_ms > ?8",
                    params![
                        fence.scope_id.as_str(),
                        fence.replay_key.as_str(),
                        fence.envelope_hash.as_str(),
                        fence.owner_id.as_str(),
                        fence.lease_token.as_str(),
                        renewed_expires_at as i64,
                        now as i64,
                        now as i64,
                    ],
                )?;
                Ok(changed == 1)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retirement = retirement.clone();
        let deleted = self
            .conn
            .write(move |tx| match retirement {
                EffectJournalRetirement::Session { session_id } => tx.execute(
                    "DELETE FROM runtime_effect_replay WHERE session_id = ?1",
                    params![session_id],
                ),
                EffectJournalRetirement::Process { process_id } => {
                    let identity = ExecutionScope::process(process_id)
                        .journal_identity()
                        .expect("process scopes always form durable journal identities");
                    tx.execute(
                        "DELETE FROM runtime_effect_replay WHERE scope_id = ?1",
                        params![identity.key()],
                    )
                }
            })
            .await
            .map_err(|error| {
                RuntimeError::new("sqlite_effect_journal_retirement", error.to_string())
            })?;
        Ok(deleted)
    }
}

fn select_effect_row(
    tx: &rusqlite::Transaction<'_>,
    scope_id: &str,
    replay_key: &str,
) -> rusqlite::Result<Option<StoredEffectRow>> {
    tx.query_row(
        "SELECT envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_owner_id, lease_token, lease_expires_at_ms, due_at_ms
         FROM runtime_effect_replay
         WHERE scope_id = ?1 AND replay_key = ?2",
        params![scope_id, replay_key],
        |row| {
            Ok(StoredEffectRow {
                envelope_hash: row.get(0)?,
                envelope_json: row.get(1)?,
                status: row.get(2)?,
                outcome_json: row.get(3)?,
                error_json: row.get(4)?,
                lease_expires_at_ms: row.get::<_, i64>(7)? as u64,
                due_at_ms: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
            })
        },
    )
    .optional()
}

fn insert_claimed_row(
    tx: &rusqlite::Transaction<'_>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO runtime_effect_replay (
            scope_id, session_id, replay_key, envelope_hash,
            envelope_json, status, outcome_json, error_json, lease_owner_id,
            lease_token, lease_expires_at_ms, due_at_ms, created_at_ms, updated_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            request.scope_id.as_str(),
            request.session_id.as_deref(),
            request.replay_key.as_str(),
            request.envelope_hash.as_str(),
            request.envelope_json.as_str(),
            EffectRowStatus::InProgress.column(),
            request.owner_id.as_str(),
            request.lease_token.as_str(),
            stamp.lease_expires_at_ms as i64,
            stamp.due_at_ms.map(|value| value as i64),
            stamp.now_ms as i64,
            stamp.now_ms as i64,
        ],
    )?;
    Ok(())
}

fn take_over_expired_lease(
    tx: &rusqlite::Transaction<'_>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE runtime_effect_replay
         SET lease_owner_id = ?3,
             lease_token = ?4,
             lease_expires_at_ms = ?5,
             due_at_ms = ?6,
             updated_at_ms = ?7
         WHERE scope_id = ?1 AND replay_key = ?2",
        params![
            request.scope_id.as_str(),
            request.replay_key.as_str(),
            request.owner_id.as_str(),
            request.lease_token.as_str(),
            stamp.lease_expires_at_ms as i64,
            stamp.due_at_ms.map(|value| value as i64),
            stamp.now_ms as i64,
        ],
    )?;
    Ok(())
}

fn effect_sqlite_error(err: rusqlite::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.code("store"), err.to_string())
}
