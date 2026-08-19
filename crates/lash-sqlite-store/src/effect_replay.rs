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
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectFinalizeOutcome,
    EffectGroupColumn, EffectGroupRecord, EffectLeaseFence, EffectLeaseStamp, EffectReplayDriver,
    EffectReplayPersistence, EffectReplayVocabulary, EffectRowStatus, EffectTerminal,
    StoredEffectRow, StoredGroupSettlement, UnsettledGroupChild, decide_effect_claim,
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

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary::sqlite();

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

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::KeyAddressed
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

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::KeyAddressed
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
        Box::pin(
            self.inner
                .execute_effect(&self.scope, envelope, local_executor),
        )
        .await
    }

    /// `true`: the group methods below are implemented against the durable
    /// journal, and this store's [`EffectHost::scoped_static`] hands out the
    /// `'static` scopes the flag's other half requires — a child must be able to
    /// outlive its caller to honor
    /// [`LoserDisposition::RunToCompletion`](lash_core::LoserDisposition::RunToCompletion).
    ///
    /// The two capabilities are one question and must not drift apart, which is
    /// why the flag is answered here rather than defaulted: the surface it
    /// admits is exactly the surface below.
    fn supports_effect_groups(&self) -> bool {
        true
    }

    /// Delegated to the shared driver exactly as `execute_effect` is: the group
    /// host is one implementation over
    /// [`EffectReplayPersistence`](lash_core::facade_support::effect_replay_driver::EffectReplayPersistence),
    /// and this store contributes the substrate half of it rather than a second
    /// copy of the state machine.
    async fn open_effect_group(
        &self,
        group: lash_core::CheckedEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, RuntimeEffectControllerError> {
        Box::pin(self.inner.open_effect_group(&self.scope, group)).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<lash_core::GroupSettlement, RuntimeEffectControllerError> {
        Box::pin(self.inner.await_next_group_settlement(handle, cancel)).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        Box::pin(self.inner.close_effect_group(&handle, disposition)).await
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

    /// Writes the terminal and, for a grouped child, allocates its settlement
    /// rank — in the normative order (N1).
    ///
    /// The fenced `UPDATE` runs first and the counter is bumped only on rowcount
    /// 1, so a driver whose lease was taken over allocates nothing. Everything
    /// runs inside one `BEGIN IMMEDIATE` write transaction, which is also why
    /// this backend needs no `RETURNING`: SQLite admits one writer, so the bump
    /// and the read-back of the bumped value cannot interleave with a sibling's.
    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<EffectFinalizeOutcome, RuntimeEffectControllerError> {
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
                if changed != 1 {
                    // No counter bump: the fence moved, so this driver owns
                    // neither the child nor a rank in its group. Committing an
                    // observation is the port's documented shape; burning a
                    // number here would advance a group this driver has lost.
                    return Ok(EffectFinalizeOutcome::FenceMoved);
                }
                let group_key: Option<String> = tx.query_row(
                    "SELECT group_key FROM runtime_effect_replay
                     WHERE scope_id = ?1 AND replay_key = ?2",
                    params![fence.scope_id.as_str(), fence.replay_key.as_str()],
                    |row| row.get(0),
                )?;
                let Some(group_key) = group_key else {
                    return Ok(EffectFinalizeOutcome::Written {
                        settlement_seq: None,
                    });
                };
                let bumped = tx.execute(
                    "UPDATE runtime_effect_group
                     SET next_seq = next_seq + 1
                     WHERE group_key = ?1",
                    params![group_key.as_str()],
                )?;
                if bumped != 1 {
                    return Err(missing_group_row(&group_key));
                }
                let settlement_seq: i64 = tx.query_row(
                    "SELECT next_seq FROM runtime_effect_group WHERE group_key = ?1",
                    params![group_key.as_str()],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "UPDATE runtime_effect_replay
                     SET settlement_seq = ?3
                     WHERE scope_id = ?1 AND replay_key = ?2",
                    params![
                        fence.scope_id.as_str(),
                        fence.replay_key.as_str(),
                        settlement_seq,
                    ],
                )?;
                Ok(EffectFinalizeOutcome::Written {
                    settlement_seq: Some(u64_from_sql(
                        "RuntimeEffectGroup",
                        "next_seq",
                        settlement_seq,
                    )?),
                })
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Records the group and reports the row **as it stands durably**, so a
    /// reopen is fenced against what the journal holds rather than against this
    /// process's memory.
    ///
    /// The insert and the read-back share one `BEGIN IMMEDIATE` transaction,
    /// which touches only the group table and commits before any child of the
    /// group claims (N2).
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
        let record = record.clone();
        self.conn
            .write(move |tx| {
                // `DO NOTHING` rather than an upsert: reopening a group must not
                // reset `next_seq`, which would re-seat recorded children at
                // ranks a caller has already consumed.
                tx.execute(
                    "INSERT INTO runtime_effect_group (
                        group_key, scope_id, session_id, wake, loser_disposition,
                        children, next_seq, created_at_ms
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
                     ON CONFLICT(group_key) DO NOTHING",
                    params![
                        record.group_key.as_str(),
                        record.scope_id.as_str(),
                        record.session_id.as_deref(),
                        record.wake.column(),
                        record.loser_disposition.column(),
                        record.children as i64,
                        record.created_at_ms as i64,
                    ],
                )?;
                select_group_record(tx, &record.group_key)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Reads the group's children that hold no rank: the complement of
    /// [`read_group_settlement`](Self::read_group_settlement)'s
    /// `settlement_seq IS NOT NULL`.
    ///
    /// Served by `idx_runtime_effect_replay_group_unsettled`, whose predicate is
    /// exactly this filter. The rank read's unique backstop indexes the opposite
    /// half, so without a complementary index this read scans the whole effect
    /// journal — once per child completion after a close, and once per drain
    /// pass (FIG-1536).
    async fn read_unsettled_group_children(
        &self,
        group_key: &str,
    ) -> Result<Vec<UnsettledGroupChild>, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT scope_id, replay_key, envelope_json, status, lease_expires_at_ms
                     FROM runtime_effect_replay
                     WHERE group_key = ?1 AND settlement_seq IS NULL
                     ORDER BY replay_key",
                )?;
                let rows = statement
                    .query_map(params![group_key.as_str()], |row| {
                        Ok(UnsettledGroupChild {
                            scope_id: row.get(0)?,
                            replay_key: row.get(1)?,
                            envelope_json: row.get(2)?,
                            status: row.get(3)?,
                            lease_expires_at_ms: u64_from_sql(
                                "RuntimeEffectReplay",
                                "lease_expires_at_ms",
                                row.get(4)?,
                            )?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: usize,
    ) -> Result<Option<StoredGroupSettlement>, RuntimeEffectControllerError> {
        let Some(offset) = rank.checked_sub(1) else {
            return Ok(None);
        };
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT settlement_seq, replay_key, status, outcome_json, error_json
                         FROM runtime_effect_replay
                         WHERE group_key = ?1 AND settlement_seq IS NOT NULL
                         ORDER BY settlement_seq
                         LIMIT 1 OFFSET ?2",
                        params![group_key.as_str(), offset as i64],
                        |row| {
                            Ok(StoredGroupSettlement {
                                sequence: u64_from_sql(
                                    "RuntimeEffectReplay",
                                    "settlement_seq",
                                    row.get(0)?,
                                )?,
                                replay_key: row.get(1)?,
                                status: row.get(2)?,
                                outcome_json: row.get(3)?,
                                error_json: row.get(4)?,
                            })
                        },
                    )
                    .optional()
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

    /// Deletes the named children **and their groups in the same transaction**
    /// (N3), so no partially-retired group is ever visible.
    ///
    /// A settlement rank counts a group's recorded children, and it survives
    /// gaps only because allocation is monotonic and therefore appends above a
    /// consumed rank. A deletion *below* a consumed rank would shift ranks even
    /// though allocation never does, which is why the group row and its children
    /// go together or not at all. Both predicates select the same set: a group
    /// and its children are opened under one journal identity.
    ///
    /// The reported count stays the children, which is what this method has
    /// always reported and what a caller prunes against.
    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retirement = retirement.clone();
        let deleted = self
            .conn
            .write(move |tx| match retirement {
                EffectJournalRetirement::Session { session_id } => {
                    let deleted = tx.execute(
                        "DELETE FROM runtime_effect_replay WHERE session_id = ?1",
                        params![session_id],
                    )?;
                    tx.execute(
                        "DELETE FROM runtime_effect_group WHERE session_id = ?1",
                        params![session_id],
                    )?;
                    Ok(deleted)
                }
                EffectJournalRetirement::Process { process_id } => {
                    let identity = ExecutionScope::process(process_id)
                        .journal_identity()
                        .expect("process scopes always form durable journal identities");
                    let deleted = tx.execute(
                        "DELETE FROM runtime_effect_replay WHERE scope_id = ?1",
                        params![identity.key()],
                    )?;
                    tx.execute(
                        "DELETE FROM runtime_effect_group WHERE scope_id = ?1",
                        params![identity.key()],
                    )?;
                    Ok(deleted)
                }
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
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
                lease_expires_at_ms: u64_from_sql(
                    "RuntimeEffectReplay",
                    "lease_expires_at_ms",
                    row.get(7)?,
                )?,
                due_at_ms: row
                    .get::<_, Option<i64>>(8)?
                    .map(|value| u64_from_sql("RuntimeEffectReplay", "due_at_ms", value))
                    .transpose()?,
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
            lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
            created_at_ms, updated_at_ms
         )
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8, ?9, ?10, ?13, NULL, ?11, ?12)",
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
            request.group_key.as_deref(),
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

/// Reads back the durably recorded group row.
///
/// A group is written before this reads it, in the same transaction, so a
/// missing row is a substrate fault rather than a race — reported as corrupt
/// rather than papered over with the record the caller passed in, which would
/// make the reopen fence compare a row against itself.
fn select_group_record(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
) -> rusqlite::Result<EffectGroupRecord> {
    tx.query_row(
        "SELECT group_key, scope_id, session_id, wake, loser_disposition, children,
                created_at_ms
         FROM runtime_effect_group
         WHERE group_key = ?1",
        params![group_key],
        |row| {
            Ok(EffectGroupRecord {
                group_key: row.get(0)?,
                scope_id: row.get(1)?,
                session_id: row.get(2)?,
                wake: group_column_from_sql("wake rule", &row.get::<_, String>(3)?)?,
                loser_disposition: group_column_from_sql(
                    "loser disposition",
                    &row.get::<_, String>(4)?,
                )?,
                children: usize_from_sql("RuntimeEffectGroup", "children", row.get(5)?)?,
                created_at_ms: u64_from_sql("RuntimeEffectGroup", "created_at_ms", row.get(6)?)?,
            })
        },
    )
}

/// A persisted group column read back through the same mapping that wrote it,
/// refusing a value no version of this runtime writes.
fn group_column_from_sql<T: EffectGroupColumn>(
    column: &'static str,
    value: &str,
) -> rusqlite::Result<T> {
    EffectGroupColumn::from_column(value).ok_or_else(|| {
        sqlite_conversion_error(stored_data_corrupt(
            "RuntimeEffectGroup",
            format!("unknown effect group {column} `{value}`"),
        ))
    })
}

fn usize_from_sql(
    record_kind: &'static str,
    column: &'static str,
    value: i64,
) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| {
        sqlite_conversion_error(stored_data_corrupt(
            record_kind,
            format!("{column} must be non-negative, got {value}"),
        ))
    })
}

/// A grouped child whose group row is gone is a corrupt journal, not a
/// silently ungrouped settlement: the rank it should have taken can never be
/// served, so reporting success would hide a group no caller can finish
/// consuming.
fn missing_group_row(group_key: &str) -> rusqlite::Error {
    sqlite_conversion_error(stored_data_corrupt(
        "RuntimeEffectGroup",
        format!(
            "grouped effect child finalized against missing group row `{group_key}`; \
             its settlement rank can never be served"
        ),
    ))
}

fn effect_sqlite_error(err: rusqlite::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.store_code(), err.to_string())
}

#[cfg(test)]
mod tests;
