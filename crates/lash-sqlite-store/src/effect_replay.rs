//! SQLite-backed runtime effect replay host.
//!
//! The claim/execute/renew/finalize state machine lives in
//! [`StoreEffectReplayDriver`]; this module is the SQLite half of its
//! [`EffectReplayRowStore`] plug-in plus the host and controller types that
//! expose it. Row storage is all this module owns: the driver decides every
//! claim, replay, and drain. Every atom runs
//! inside `SqliteConnection::write` (`BEGIN IMMEDIATE`), so the read, the
//! transition decision, and the write it guards take the cross-process write
//! lock up front and cannot interleave with a competing claimant.
//!
//! SQLite's authoritative lease clock is the host's injected
//! [`Clock`](lash_core::Clock): this store runs in the same clock domain as its
//! host, and every other durable stamp in the crate already comes from there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core::facade_support::effect_replay_driver;
use lash_core::facade_support::effect_replay_driver::{
    CompletionKeys, EffectClaimDecision, EffectClaimObservation, EffectClaimRequest,
    EffectFinalizeOutcome, EffectGroupColumn, EffectGroupRecord, EffectLeaseFence,
    EffectLeaseStamp, EffectReplayCapabilities, EffectReplayRowStore, EffectReplayVocabulary,
    EffectRowStatus, EffectTerminal, StoreEffectReplayDriver, StoredEffectRow,
    StoredGroupSettlement, ToolBatchRedrive, UnsettledGroupChild, decide_effect_claim,
};
use lash_core::{
    EffectJournalRetirement, EffectRetirementGate, ExecutionScope, GroupExecutors,
    RuntimeEffectControllerError, RuntimeError, StoreEffectGroupDrain,
    facade_support::LeaseTimings,
};

use super::*;
use crate::await_event::{SqliteAwaitEventBackend, sqlite_await_events};
use crate::scope_fence::{FenceLocations, JOURNAL_SCHEMA, RegistryAttachment};

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary::sqlite();

/// The SQLite effect-replay driver: one shared state machine over
/// [`SqliteEffectReplayRowStore`].
type SqliteEffectReplay =
    StoreEffectReplayDriver<SqliteEffectReplayRowStore, SqliteAwaitEventBackend>;

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
    /// The journal file, when the host is file-backed: a session-store
    /// factory attaches it for the retention sweep.
    fence_database: Option<PathBuf>,
    /// The bound process registry's file, attached to the journal connection
    /// so process-scope fences live beside the process rows (ADR 0049).
    registry: Arc<RegistryAttachment>,
}

/// Scoped SQLite-backed runtime effect controller.
#[derive(Clone)]
pub struct SqliteRuntimeEffectController {
    inner: Arc<SqliteEffectReplay>,
    scope: ExecutionScope,
}

// The `AwaitEventResolver` / `EffectHost` / `RuntimeEffectController` surface of
// both types is the shared adapter in `effect_replay_driver::adapter`; this
// store only says which driver each handle forwards to.
impl effect_replay_driver::StoreReplayAdapter for SqliteEffectHost {
    type Persistence = SqliteEffectReplayRowStore;
    type AwaitEvents = SqliteAwaitEventBackend;
    fn replay_driver(&self) -> &Arc<SqliteEffectReplay> {
        &self.inner
    }
}

impl effect_replay_driver::StoreReplayHost for SqliteEffectHost {
    fn effect_scope_fence_database(&self) -> Option<PathBuf> {
        self.fence_database.clone()
    }

    fn bind_process_registry(&self, binding: lash_core::ProcessRegistryBinding) {
        if let Some(path) = binding.fence_database {
            self.registry.request(path);
        }
    }
}

impl effect_replay_driver::StoreReplayAdapter for SqliteRuntimeEffectController {
    type Persistence = SqliteEffectReplayRowStore;
    type AwaitEvents = SqliteAwaitEventBackend;
    fn replay_driver(&self) -> &Arc<SqliteEffectReplay> {
        &self.inner
    }
}

impl effect_replay_driver::StoreReplayController for SqliteRuntimeEffectController {
    fn execution_scope(&self) -> &ExecutionScope {
        &self.scope
    }
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
        let registry = Arc::new(RegistryAttachment::default());
        Ok(Self {
            inner: open_effect_replay_driver(
                path,
                StoreBacking::File,
                options,
                clock,
                Arc::clone(&registry),
            )
            .await?,
            fence_database: Some(path.to_path_buf()),
            registry,
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
    }

    /// Register the resolver that says how a grouped child is run.
    ///
    /// This is the host's one wiring seam: it is supplied here — by the host
    /// that owns those runners — rather than discovered from whatever session is
    /// in scope, and every path resolves through it, the open of a group, a
    /// retry, and the loser drain alike. Until it is called this host answers
    /// [`RuntimeEffectController::supports_effect_groups`] `false` and refuses
    /// every group method with
    /// [`EffectGroupUnsupported`](lash_core::RuntimeErrorCode::EffectGroupUnsupported)
    /// rather than journaling a group nothing can run.
    ///
    /// Registering the *same* resolver again is a no-op; registering a different
    /// one is refused, and the refusal is decided by the write itself, so two
    /// threads racing here cannot both believe they won.
    pub fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    /// The host-owned drain over this host's effect journal.
    ///
    /// The drain shares this host's driver, so it claims under the same owner
    /// identity, over the same journal, and resolves children through the same
    /// registered resolver.
    pub fn group_drain(&self) -> Arc<dyn StoreEffectGroupDrain> {
        Arc::clone(&self.inner).into_group_drain()
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
            inner: open_effect_replay_driver(
                path,
                StoreBacking::File,
                options,
                clock,
                Arc::new(RegistryAttachment::default()),
            )
            .await?,
            scope,
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
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
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
    registry: Arc<RegistryAttachment>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open(path).await?;
    ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay).await?;
    let signing_secret = conn
        .call(|connection| {
            connection.query_row(
                "SELECT signing_secret FROM await_event_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
        })
        .await?;
    apply_pragmas(&conn, backing).await?;
    Ok(Arc::new(build_effect_replay_driver(
        conn,
        options,
        clock,
        signing_secret,
        CompletionKeys::Issued,
        registry,
    )))
}

#[cfg(feature = "testing")]
async fn open_effect_replay_memory_driver(
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open_in_memory().await?;
    ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay).await?;
    let signing_secret = conn
        .call(|connection| {
            connection.query_row(
                "SELECT signing_secret FROM await_event_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
        })
        .await?;
    apply_pragmas(&conn, StoreBacking::Memory).await?;
    Ok(Arc::new(build_effect_replay_driver(
        conn,
        options,
        clock,
        signing_secret,
        CompletionKeys::Unsupported,
        Arc::new(RegistryAttachment::default()),
    )))
}

fn build_effect_replay_driver(
    conn: SqliteConnection,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
    signing_secret: Vec<u8>,
    completion_keys: CompletionKeys,
    registry: Arc<RegistryAttachment>,
) -> SqliteEffectReplay {
    let await_events = sqlite_await_events(
        conn.clone(),
        Arc::clone(&registry),
        signing_secret,
        Arc::clone(&clock),
    );
    StoreEffectReplayDriver::new(
        SqliteEffectReplayRowStore {
            completion_keys,
            conn,
            clock: Arc::clone(&clock),
            registry,
        },
        await_events,
        clock,
        options.lease_timings,
    )
}

/// SQLite storage atoms for the durable effect journal.
///
/// `pub` only because it names an associated type of the shared adapter; the
/// module is private, so nothing outside this crate can reach it.
pub struct SqliteEffectReplayRowStore {
    /// File-backed rows outlive the process and back routable completion keys;
    /// the testing-only memory backing's do not.
    completion_keys: CompletionKeys,
    conn: SqliteConnection,
    /// SQLite's authoritative lease clock, shared with the driver's sleep clock
    /// because the store and its host share one clock domain.
    clock: Arc<dyn lash_core::Clock>,
    /// The bound process registry whose file holds process-scope fences.
    registry: Arc<RegistryAttachment>,
}

impl SqliteEffectReplayRowStore {
    async fn fence_locations(&self) -> Result<FenceLocations, RuntimeEffectControllerError> {
        self.registry
            .ensure_attached(&self.conn)
            .await
            .map_err(effect_sqlite_error)
    }
}

impl effect_replay_driver::sealed::EffectReplayBackend for SqliteEffectReplayRowStore {}

#[async_trait::async_trait]
impl EffectReplayRowStore for SqliteEffectReplayRowStore {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    fn capabilities(&self) -> EffectReplayCapabilities {
        EffectReplayCapabilities {
            completion_keys: self.completion_keys,
            tool_batch_redrive: ToolBatchRedrive::AggregateClaim,
        }
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let request = request.clone();
        let clock = Arc::clone(&self.clock);
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                // The retirement fence is read under the same `BEGIN IMMEDIATE`
                // lock retirement writes it under (on every attached file), so
                // a claim can never slip between a scope's tombstone and its
                // row deletions.
                if fences.is_fenced(tx, &request.scope_id)? {
                    return Ok(EffectClaimObservation::ScopeRetired);
                }
                let row = select_effect_row(tx, &request.scope_id, &request.replay_key)?;
                // Queueing and writer admission must not consume the new lease.
                let now_ms = clock.timestamp_ms();
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

    async fn replay_row_exists(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let scope_id = scope_id.to_string();
        let replay_key = replay_key.to_string();
        self.conn
            .call(move |connection| {
                connection.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM runtime_effect_replay
                         WHERE scope_id = ?1 AND replay_key = ?2
                     )",
                    params![scope_id, replay_key],
                    |row| row.get(0),
                )
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
        let clock = Arc::clone(&self.clock);
        self.conn
            .write(move |tx| {
                let now = clock.timestamp_ms();
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
        let scope_id = record.scope_id.clone();
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                if fences.is_fenced(tx, &record.scope_id)? {
                    return Ok(None);
                }
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
                select_group_record(tx, &record.group_key).map(Some)
            })
            .await
            .map_err(effect_sqlite_error)?
            .ok_or_else(|| effect_replay_driver::scope_retired(&scope_id))
    }

    /// Reads the group row without writing one, so a drain reads the declared
    /// disposition instead of inserting a group it was only asking about.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let record = select_group_record(&tx, &group_key).optional()?;
                tx.commit()?;
                Ok(record)
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
                    "SELECT scope_id, replay_key, envelope_json, status, outcome_json, error_json, lease_expires_at_ms
                     FROM runtime_effect_replay
                     WHERE group_key = ?1 AND settlement_seq IS NULL
                     ORDER BY replay_key",
                )?;
                let rows = statement
                    .query_map(params![group_key.as_str()], |row| {
                        let state = effect_replay_driver::EffectRowState::from_columns(
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        );
                        Ok(UnsettledGroupChild {
                            scope_id: row.get(0)?,
                            replay_key: row.get(1)?,
                            envelope_json: row.get(2)?,
                            state,
                            lease_expires_at_ms: u64_from_sql(
                                "RuntimeEffectReplay",
                                "lease_expires_at_ms",
                                row.get(6)?,
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
                            let state = effect_replay_driver::EffectRowState::from_columns(
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            );
                            Ok(StoredGroupSettlement {
                                sequence: u64_from_sql(
                                    "RuntimeEffectReplay",
                                    "settlement_seq",
                                    row.get(0)?,
                                )?,
                                replay_key: row.get(1)?,
                                state,
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
        let clock = Arc::clone(&self.clock);
        self.conn
            .write(move |tx| {
                let now = clock.timestamp_ms();
                let renewed_expires_at = now.saturating_add(lease_ttl_ms);
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
    ///
    /// A scope-exact retirement (N4) additionally deletes the scope's
    /// await-event promise rows and writes its permanent retirement tombstone.
    /// A fence kept in this file is written in the same `BEGIN IMMEDIATE`
    /// transaction as the deletions, so both become visible together. A
    /// process-scope fence kept in the bound registry's file is the one
    /// commit point of the retirement: its insert commits first, under the
    /// quiescence proof read on the same locks, and the journal purge that
    /// follows is an idempotent cleanup a crash may lose — the fenced scope
    /// then admits nothing and the next bind or retention sweep purges its
    /// rows (ADR 0049).
    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retired_scope_key = retirement
            .retired_scope()
            .and_then(|scope| scope.journal_identity().ok())
            .map(|identity| identity.key().to_string());
        let retirement = retirement.clone();
        let now_ms = self.clock.timestamp_ms();
        let retirement_error = |error: rusqlite::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                error.to_string(),
            )
        };
        let scope = match &retirement {
            EffectJournalRetirement::Session { session_id } => {
                let session_id = session_id.clone();
                return self
                    .conn
                    .write(move |tx| {
                        // Session deletion is the authoritative lifecycle
                        // decision for every exact execution scope it owns.
                        // Preserve those scopes as the same retirement
                        // evidence used by scope-exact retirement so artifact
                        // cleanup remains retryable after journal deletion.
                        tx.execute(
                            "INSERT INTO effect_scope_retirements (
                                 scope_id, retired_at_ms, artifact_cleanup_completed
                             )
                             SELECT scope_id, ?2, 0 FROM (
                                 SELECT DISTINCT scope_id FROM runtime_effect_replay
                                 WHERE session_id = ?1
                                 UNION
                                 SELECT DISTINCT scope_id FROM runtime_effect_group
                                 WHERE session_id = ?1
                             ) AS retired_session_scopes
                             WHERE 1
                             ON CONFLICT(scope_id) DO NOTHING",
                            params![session_id.as_str(), now_ms as i64],
                        )?;
                        let deleted = tx.execute(
                            "DELETE FROM runtime_effect_replay WHERE session_id = ?1",
                            params![session_id.as_str()],
                        )?;
                        tx.execute(
                            "DELETE FROM runtime_effect_group WHERE session_id = ?1",
                            params![session_id.as_str()],
                        )?;
                        Ok(deleted)
                    })
                    .await
                    .map_err(retirement_error);
            }
            EffectJournalRetirement::Process { .. }
            | EffectJournalRetirement::RuntimeOperation { .. } => retirement
                .retired_scope()
                .expect("scope-exact retirements name their scope"),
        };
        let identity = scope
            .journal_identity()
            .expect("process and runtime-operation scopes always form durable journal identities");
        let scope_id = identity.key().to_string();
        let scope_json =
            serde_json::to_string(&scope).expect("execution scopes serialize infallibly");
        let when_quiescent = retirement.gate() == Some(EffectRetirementGate::WhenQuiescent);
        let fences = self
            .registry
            .ensure_attached(&self.conn)
            .await
            .map_err(retirement_error)?;
        let fence_schema = fences.fence_schema_for(&scope);
        let two_commits = fences.fence_is_in_registry_file(&scope);
        // Commit point: the quiescence proof and the fence insert, under the
        // `BEGIN IMMEDIATE` lock every claim reads the fence under. When the
        // fence shares the journal file, the purge rides the same commit.
        let fenced = {
            let scope_id = scope_id.clone();
            let scope_json = scope_json.clone();
            self.conn
                .write(move |tx| {
                    if when_quiescent
                        && !scope_is_quiescent(tx, JOURNAL_SCHEMA, &scope_id, &scope_json)?
                    {
                        return Ok(None);
                    }
                    insert_scope_fence(tx, fence_schema, &scope_id, now_ms)?;
                    if two_commits {
                        return Ok(Some(None));
                    }
                    Ok(Some(Some(delete_scope_rows(
                        tx,
                        JOURNAL_SCHEMA,
                        &scope_id,
                        &scope_json,
                    )?)))
                })
                .await
                .map_err(retirement_error)?
        };
        let Some(purged) = fenced else {
            return Err(effect_replay_driver::scope_not_quiescent(
                retired_scope_key.as_deref().unwrap_or_default(),
            ));
        };
        if let Some(deleted) = purged {
            return Ok(deleted);
        }
        // Post-commit cleanup: idempotent, and repeated by the next bind or
        // sweep if this process dies before it lands.
        self.conn
            .write(move |tx| delete_scope_rows(tx, JOURNAL_SCHEMA, &scope_id, &scope_json))
            .await
            .map_err(retirement_error)
    }

    async fn reinstate_scope(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let scope_id = scope_id.to_string();
        let fences = self
            .registry
            .ensure_attached(&self.conn)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })?;
        self.conn
            .write(move |tx| fences.lift(tx, &scope_id))
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }

    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<ExecutionScope>, RuntimeError> {
        self.conn
            .call(|conn| {
                let mut statement = conn.prepare(
                    "SELECT scope_id FROM effect_scope_retirements
                     WHERE artifact_cleanup_completed = 0
                     ORDER BY scope_id",
                )?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                let mut scopes = Vec::new();
                for row in rows {
                    let key = row?;
                    let scope = ExecutionScope::from_journal_key(&key).ok_or_else(|| {
                        rusqlite::Error::InvalidParameterName(format!(
                            "invalid retired effect scope key `{key}`"
                        ))
                    })?;
                    scopes.push(scope);
                }
                Ok(scopes)
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }

    async fn complete_artifact_owner_retirement(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let scope_id = scope_id.to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE effect_scope_retirements
                     SET artifact_cleanup_completed = 1
                     WHERE scope_id = ?1",
                    params![scope_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }
}

/// Whether nothing under `scope_id` is still live: no `in_progress` effect
/// row, no group row still waiting for a child that has not been journaled
/// (an open group, or a run-to-completion close whose drain has not yet
/// claimed every loser), and no unresolved promise under the scope (a wait
/// row is a continuation's durable wait: it stays unresolved until the
/// promise settles or the wait is cancelled).
///
/// `schema` names the database the journal tables live in: `main` on the
/// journal's own connection, the attached name on a connection that reaches
/// the journal file from another database (the retention sweep).
pub(crate) fn scope_is_quiescent(
    tx: &rusqlite::Transaction<'_>,
    schema: &str,
    scope_id: &str,
    scope_json: &str,
) -> rusqlite::Result<bool> {
    let live: bool = tx.query_row(
        &format!(
            "SELECT EXISTS(
                SELECT 1 FROM {schema}.runtime_effect_replay
                WHERE scope_id = ?1 AND status = 'in_progress'
             ) OR EXISTS(
                SELECT 1 FROM {schema}.runtime_effect_group AS grp
                WHERE grp.scope_id = ?1
                  AND grp.children > (
                      SELECT COUNT(*) FROM {schema}.runtime_effect_replay AS child
                      WHERE child.scope_id = ?1 AND child.group_key = grp.group_key
                  )
             ) OR EXISTS(
                SELECT 1 FROM {schema}.await_event_waits
                WHERE scope_json = ?2 AND terminal_json IS NULL
             )"
        ),
        params![scope_id, scope_json],
        |row| row.get(0),
    )?;
    Ok(!live)
}

/// Scope-exact retirement (N4) of one non-session scope whose fence shares
/// the journal's file: the permanent fence first, then the scope's effect
/// rows, group rows, and promise rows, all in the caller's transaction.
/// Returns the effect rows deleted.
pub(crate) fn retire_scope_rows(
    tx: &rusqlite::Transaction<'_>,
    schema: &str,
    scope_id: &str,
    scope_json: &str,
    now_ms: u64,
) -> rusqlite::Result<usize> {
    insert_scope_fence(tx, schema, scope_id, now_ms)?;
    delete_scope_rows(tx, schema, scope_id, scope_json)
}

/// Write the permanent fence of `scope_id` into `schema`'s fence table.
pub(crate) fn insert_scope_fence(
    tx: &rusqlite::Transaction<'_>,
    schema: &str,
    scope_id: &str,
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO {schema}.effect_scope_retirements (
                 scope_id, retired_at_ms, artifact_cleanup_completed
             )
             VALUES (?1, ?2, 0)
             ON CONFLICT(scope_id) DO NOTHING"
        ),
        params![scope_id, now_ms as i64],
    )?;
    Ok(())
}

/// Delete the effect rows, group rows, and promise rows of one scope from
/// `schema`'s journal tables. Returns the effect rows deleted.
pub(crate) fn delete_scope_rows(
    tx: &rusqlite::Transaction<'_>,
    schema: &str,
    scope_id: &str,
    scope_json: &str,
) -> rusqlite::Result<usize> {
    let deleted = tx.execute(
        &format!("DELETE FROM {schema}.runtime_effect_replay WHERE scope_id = ?1"),
        params![scope_id],
    )?;
    tx.execute(
        &format!("DELETE FROM {schema}.runtime_effect_group WHERE scope_id = ?1"),
        params![scope_id],
    )?;
    tx.execute(
        &format!("DELETE FROM {schema}.await_event_waits WHERE scope_json = ?1"),
        params![scope_json],
    )?;
    Ok(deleted)
}

/// Delete every journal row under a scope that any fence location has
/// fenced: the cleanup a retirement whose fence committed in the registry
/// file but whose purge was lost still owes (ADR 0049). Idempotent; returns
/// the scopes purged.
pub(crate) fn purge_rows_under_fenced_scopes(
    tx: &rusqlite::Transaction<'_>,
    journal_schema: &str,
    fences: FenceLocations,
) -> rusqlite::Result<usize> {
    let mut scopes: Vec<(String, String)> = Vec::new();
    {
        let mut keyed = tx.prepare(&format!(
            "SELECT scope_id FROM {journal_schema}.runtime_effect_replay
             WHERE session_id IS NULL
             UNION
             SELECT scope_id FROM {journal_schema}.runtime_effect_group
             WHERE session_id IS NULL"
        ))?;
        for key in keyed.query_map([], |row| row.get::<_, String>(0))? {
            let key = key?;
            if let Some(scope) = lash_core::ExecutionScope::from_journal_key(&key) {
                let scope_json = serde_json::to_string(&scope).expect("execution scopes serialize");
                scopes.push((key, scope_json));
            }
        }
        let mut waited = tx.prepare(&format!(
            "SELECT DISTINCT scope_json FROM {journal_schema}.await_event_waits
             WHERE session_id IS NULL"
        ))?;
        for scope_json in waited.query_map([], |row| row.get::<_, String>(0))? {
            let scope_json = scope_json?;
            if let Ok(scope) = serde_json::from_str::<lash_core::ExecutionScope>(&scope_json)
                && let Ok(identity) = scope.journal_identity()
            {
                scopes.push((identity.key().to_string(), scope_json));
            }
        }
    }
    scopes.sort();
    scopes.dedup();
    let mut purged = 0;
    for (scope_id, scope_json) in scopes {
        if fences.is_fenced(tx, &scope_id)? {
            delete_scope_rows(tx, journal_schema, &scope_id, &scope_json)?;
            purged += 1;
        }
    }
    Ok(purged)
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
            let state = effect_replay_driver::EffectRowState::from_columns(
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            );
            Ok(StoredEffectRow {
                envelope_hash: row.get(0)?,
                envelope_json: row.get(1)?,
                state,
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
                session_id: row.get::<_, Option<String>>(2)?.map(SessionId::from),
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
