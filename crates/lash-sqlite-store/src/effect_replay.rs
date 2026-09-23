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
//! [`Clock`](lash_core_execution::Clock): this store runs in the same clock domain as its
//! host, and every other durable stamp in the crate already comes from there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_core_execution::facade_support::effect_replay_driver;
use lash_core_execution::facade_support::effect_replay_driver::{
    AcceptedGroupChild, CompletionKeys, EffectCancelOutcome, EffectCancelRequest,
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectCommitState,
    EffectDischargeOutcome, EffectDischargeRequest, EffectFinalizeOutcome,
    EffectGroupChildCommitOutcome, EffectGroupChildCommitRequest, EffectGroupColumn,
    EffectGroupLifecycle, EffectGroupLifecyclePhase, EffectGroupRecord, EffectLeaseFence,
    EffectLeaseStamp, EffectReplayCapabilities, EffectReplayRowStore, EffectReplayVocabulary,
    EffectRowStatus, EffectTerminal, StoreEffectReplayDriver, StoredChildArbitration,
    StoredEffectRow, StoredGroupSettlement, UnsettledGroupChild, decide_effect_claim,
};
use lash_core_execution::{
    EffectJournalRetirement, EffectRetirementGate, ExecutionScope, GroupExecutors,
    RuntimeEffectControllerError, RuntimeError, StoreEffectGroupClosing, StoreEffectGroupDrain,
    facade_support::LeaseTimings,
};

use std::sync::LazyLock;

use lash_store_sql::effect::EffectJournalStatements;
use lash_store_sql::effect::group::GroupStatements;
use lash_store_sql::effect::group_child::GroupChildStatements;
use lash_store_sql::effect::replay::ReplayStatements;

use super::*;
use crate::await_event::{SqliteAwaitEventBackend, sqlite_await_events, wait_sql};
use crate::scope_fence::{FenceLocations, RegistryAttachment, Schema, fence_sql};

mod row_store;
mod settlement_notify;

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary::sqlite();

lash_store_sql::statements! {
    /// Journal-wide statements only SQLite issues.
    pub(crate) struct EffectJournalSqliteStatements @ "effect_journal" {
        /// Preserve one retirement fence per execution scope owned by session
        /// `?1`, stamped `?2`, before the session's journal rows are deleted.
        ///
        /// SQLite stamps from the host clock and writes the boolean column as
        /// `0`; PostgreSQL stamps from the server clock and writes `FALSE`.
        insert_session_scope_fences = "INSERT INTO effect_scope_retirements (
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
             ON CONFLICT (scope_id) DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_replay` statements only SQLite issues.
    pub(crate) struct ReplaySqliteStatements @ "effect_replay" {
        /// The row a claim decision reads, for `?1` (scope) / `?2` (replay
        /// key).
        ///
        /// No lock suffix: every atom already runs under `BEGIN IMMEDIATE`,
        /// which holds the database write lock for the whole transaction.
        select_for_claim = "SELECT envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_expires_at_ms, due_at_ms, commit_state, drain_input
             FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2";

        /// No `ON CONFLICT`: the row was read as absent under the same
        /// `BEGIN IMMEDIATE` lock, so a conflict is a defect and the
        /// constraint error is the right report.
        insert_claimed = "INSERT INTO runtime_effect_replay (
                scope_id, session_id, replay_key, envelope_hash,
                envelope_json, status, outcome_json, error_json, lease_owner_id,
                lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
                commit_state, commit_seq, created_at_ms, updated_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8, ?9, ?10, ?13, NULL, 'pending', NULL, ?11, ?12)";

        /// Write the terminal under the lease fence and report the child's
        /// group: `?1` scope, `?2` replay key, `?3` envelope hash, `?4`
        /// owner, `?5` lease token, `?6` status, `?7` outcome, `?8` error,
        /// `?9` now, `?10` now.
        ///
        /// The lease instant is bound by the caller from the host clock;
        /// PostgreSQL reads its own `transaction_timestamp()` instead, which
        /// is why the two texts fork.
        finalize_terminal = "UPDATE runtime_effect_replay
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
               AND lease_expires_at_ms > ?10
             RETURNING group_key";

        /// Release an ungrouped, uncommitted derivation under the complete live lease fence.
        release_uncommitted_derivation = "UPDATE runtime_effect_replay
             SET lease_expires_at_ms = 0,
                 updated_at_ms = ?6
             WHERE scope_id = ?1
               AND replay_key = ?2
               AND envelope_hash = ?3
               AND lease_owner_id = ?4
               AND lease_token = ?5
               AND status = 'in_progress'
               AND group_key IS NULL
               AND commit_state = 'pending'
               AND lease_expires_at_ms > ?6";

        /// Extend the lease of `?1` / `?2` to `?6`, stamping `?7`, if `?4` /
        /// `?5` still hold it at `?8`. Forks for the same reason
        /// [`ReplaySqliteStatements::finalize_terminal`] does.
        renew_lease = "UPDATE runtime_effect_replay
             SET lease_expires_at_ms = ?6,
                 updated_at_ms = ?7
             WHERE scope_id = ?1
               AND replay_key = ?2
               AND envelope_hash = ?3
               AND lease_owner_id = ?4
               AND lease_token = ?5
               AND status = 'in_progress'
               AND lease_expires_at_ms > ?8";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_group` statements only SQLite issues.
    pub(crate) struct GroupSqliteStatements @ "effect_group" {
        /// SQLite reads the durable row back with
        /// [`GroupStatements::select_by_key`] unconditionally; PostgreSQL
        /// carries a `RETURNING` clause so the read-back only costs a second
        /// statement on the conflict path.
        insert_new = "INSERT INTO runtime_effect_group (
                group_key, scope_id, session_id, wake, loser_disposition,
                expected_children, next_seq, next_commit_seq, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)
             ON CONFLICT (group_key) DO NOTHING";

        /// The §7 lifecycle CAS: set `lifecycle = ?2` on `?1` while its phase
        /// tag is one of the `?3` JSON array's strings (an empty array is a
        /// guaranteed miss), and return the lifecycle now durable — the
        /// written value on a hit. `json_extract`/`json_each` are SQLite's
        /// JSON operators; PostgreSQL spells the same guard `= ANY(?3)`.
        transition_lifecycle = "UPDATE runtime_effect_group
             SET lifecycle = ?2
             WHERE group_key = ?1
               AND json_extract(lifecycle, '$.type') IN (
                   SELECT value FROM json_each(?3))
             RETURNING lifecycle";

        /// Every `live` or `closing` group under scope `?1` — the groups an
        /// opener's end closes and resumes finalizing (ADR 0099 §7).
        select_unsettled_by_scope = "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    expected_children, lifecycle, created_at_ms
             FROM runtime_effect_group
             WHERE scope_id = ?1
               AND json_extract(lifecycle, '$.type') != 'settled'
             ORDER BY created_at_ms, group_key";

        /// `(group_key, lifecycle)` for every non-`settled` group owned by
        /// session `?1` — the pins session deletion refuses on.
        select_session_pins = "SELECT group_key, lifecycle
             FROM runtime_effect_group
             WHERE session_id = ?1
               AND json_extract(lifecycle, '$.type') != 'settled'";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_group_child` statements only SQLite issues.
    pub(crate) struct GroupChildSqliteStatements @ "effect_group_child" {
        /// Retain one accepted child, keeping any existing row.
        ///
        /// `DO NOTHING` is reopen semantics, not a swallowed error: a reopen
        /// re-presents the membership it already accepted, and N1's reason for
        /// not resetting `next_seq` applies to the membership exactly as it
        /// does to the counter. SQLite reads the membership back with
        /// [`GroupChildStatements::select_membership`]; PostgreSQL carries a
        /// `RETURNING` clause for the same reason it does on the group insert.
        insert_accepted = "INSERT INTO runtime_effect_group_child (
                group_key, position, replay_key,
                envelope_json, command_version, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (group_key, position) DO NOTHING";
    }
}

/// Every effect-family statement, rendered for one schema.
pub(crate) struct EffectSql {
    /// Journal-wide statements both backends issue verbatim.
    pub(crate) journal: EffectJournalStatements,
    /// Journal-wide statements only SQLite issues.
    pub(crate) journal_sqlite: EffectJournalSqliteStatements,
    /// `runtime_effect_replay` statements both backends issue verbatim.
    pub(crate) replay: ReplayStatements,
    /// `runtime_effect_replay` statements only SQLite issues.
    pub(crate) replay_sqlite: ReplaySqliteStatements,
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub(crate) group: GroupStatements,
    /// `runtime_effect_group` statements only SQLite issues.
    pub(crate) group_sqlite: GroupSqliteStatements,
    /// `runtime_effect_group_child` statements both backends issue verbatim.
    pub(crate) group_child: GroupChildStatements,
    /// `runtime_effect_group_child` statements only SQLite issues.
    pub(crate) group_child_sqlite: GroupChildSqliteStatements,
}

impl EffectSql {
    fn render(schema: Schema) -> Self {
        let dialect = schema.dialect();
        Self {
            journal: EffectJournalStatements::render(dialect),
            journal_sqlite: EffectJournalSqliteStatements::render(dialect),
            replay: ReplayStatements::render(dialect),
            replay_sqlite: ReplaySqliteStatements::render(dialect),
            group: GroupStatements::render(dialect),
            group_sqlite: GroupSqliteStatements::render(dialect),
            group_child: GroupChildStatements::render(dialect),
            group_child_sqlite: GroupChildSqliteStatements::render(dialect),
        }
    }
}

static EFFECT_SQL: LazyLock<[EffectSql; 3]> = LazyLock::new(|| Schema::ALL.map(EffectSql::render));

/// The effect-family statements addressed through `schema`, rendered once at
/// first use and never again.
pub(crate) fn effect_sql(schema: Schema) -> &'static EffectSql {
    &EFFECT_SQL[schema.index()]
}

/// Whether a session catalog still pins `scope_id` through a cancellation
/// closure, addressed through `schema`.
///
/// `turn_cancel_closure_participants` belongs to the turn-ingress family, whose
/// module owns the statement; this host reaches it through every schema it has
/// attached, so the statement is rendered once per schema rather than built per
/// call (FIG-3383).
fn closure_participant_exists_sql(schema: Schema) -> &'static str {
    crate::turn_ingress::closure_participant_sql(schema)
        .exists_for_scope
        .sql()
}

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
    /// How long a group's finalization waits on a cancel-decided child's
    /// attempt body after the decision commits (ADR 0099 §7). Construction-
    /// level like `lease_timings`: the bound is operational, never semantic —
    /// it changes how long the finalizer waits, never what it commits.
    pub drain_budget: lash_core_execution::EffectGroupDrainBudget,
}

/// Deployment-level SQLite effect host.
///
/// This host persists runtime effect history in a local SQLite database and
/// returns scoped controllers that replay completed outcomes by
/// `(scope_id, replay_key)`.
#[derive(Clone)]
pub struct SqliteEffectHost {
    inner: Arc<SqliteEffectReplay>,
    turn_control_binding_id: Arc<str>,
    /// The journal file, when the host is file-backed: a session-store
    /// factory attaches it for the retention sweep.
    fence_database: Option<PathBuf>,
    /// The bound process registry's file, attached to the journal connection
    /// so process-scope fences live beside the process rows (ADR 0049).
    registry: Arc<RegistryAttachment>,
    closure_lifecycle: SqliteConnection,
    closure_registry: Arc<RegistryAttachment>,
}

/// Scoped SQLite-backed runtime effect controller.
#[derive(Clone)]
pub struct SqliteRuntimeEffectController {
    inner: Arc<SqliteEffectReplay>,
    scope: ExecutionScope,
    turn_control_binding_id: Arc<str>,
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

    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.turn_control_binding_id.to_string())
    }
}

lash_core_execution::impl_store_replay_await_event_resolver!(impl lash_core_execution::AwaitEventResolver for SqliteEffectHost);

#[async_trait::async_trait]
impl effect_replay_driver::StoreReplayHost for SqliteEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.turn_control_binding_id.to_string()
    }

    fn effect_scope_fence_database(&self) -> Option<PathBuf> {
        self.fence_database.clone()
    }

    fn bind_process_registry(&self, binding: lash_core_execution::ProcessRegistryBinding) {
        if let Some(path) = binding.fence_database {
            self.registry.request(path.clone());
            self.closure_registry.request(path);
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "the caller already resolved `scope.journal_identity()` at the top of this function, so re-reading it for the refusal message cannot fail"
    )]
    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let scope_id = scope.journal_identity()?.key().to_string();
        let scope_json = serde_json::to_string(scope).map_err(|error| {
            RuntimeError::new(
                lash_core_execution::RuntimeErrorCode::RecordEncodingFailed,
                error.to_string(),
            )
        })?;
        let participant_id = participant_id.to_string();
        let fences = self
            .closure_registry
            .ensure_attached(&self.closure_lifecycle)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })?;
        self.closure_lifecycle
            .write(move |tx| {
                if fences.is_fenced(tx, &scope_id)? {
                    return Ok(false);
                }
                tx.execute(
                    crate::turn_ingress::closure_participant_sql(Schema::Main)
                        .insert_new
                        .sql(),
                    params![scope_id, participant_id, scope_json],
                )?;
                Ok(true)
            })
            .await
            .map_err(|error| RuntimeError::new(
                lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                error.to_string(),
            ))?
            .then_some(())
            .ok_or_else(|| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::EffectScopeRetired,
                    format!(
                        "effect scope `{}` has been retired and cannot admit a cancellation-closure participant",
                        scope.journal_identity().expect("validated scope").key()
                    ),
                )
            })
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let scope_id = scope.journal_identity()?.key().to_string();
        let participant_id = participant_id.to_string();
        self.closure_lifecycle
            .write(move |tx| {
                tx.execute(
                    crate::turn_ingress::closure_participant_sql(Schema::Main)
                        .delete_participant
                        .sql(),
                    params![scope_id, participant_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }
}

impl effect_replay_driver::StoreReplayAdapter for SqliteRuntimeEffectController {
    type Persistence = SqliteEffectReplayRowStore;
    type AwaitEvents = SqliteAwaitEventBackend;
    fn replay_driver(&self) -> &Arc<SqliteEffectReplay> {
        &self.inner
    }

    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.turn_control_binding_id.to_string())
    }
}

lash_core_execution::impl_store_replay_await_event_resolver!(impl lash_core_execution::AwaitEventResolver for SqliteRuntimeEffectController);

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
        clock: Arc<dyn lash_core_execution::Clock>,
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
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "replay binding identity is the canonical form of the host-supplied database path (FIG-2971)"
    )]
    pub async fn open_with_options_and_clock(
        path: &Path,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_effect_host_path(path)?;
        // Opening creates the database before the host is returned, so the
        // canonical path is a stable identity across relative paths and
        // symlinked deployment configuration. Fall back only for platforms
        // that cannot canonicalize an already-open file.
        let binding_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let registry = Arc::new(RegistryAttachment::default());
        let inner = open_effect_replay_driver(
            path,
            &binding_path,
            StoreBacking::File,
            options,
            clock,
            Arc::clone(&registry),
        )
        .await?;
        let closure_lifecycle = SqliteConnection::open(path).await?;
        let closure_registry = Arc::new(RegistryAttachment::default());
        Ok(Self {
            inner,
            fence_database: Some(path.to_path_buf()),
            registry,
            closure_lifecycle,
            closure_registry,
            turn_control_binding_id: Arc::from(format!("sqlite:{}", binding_path.display())),
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
    }

    /// This is the host's one wiring seam: it is supplied here — by the host
    /// that owns those runners — rather than discovered from whatever session is
    /// in scope, and every path resolves through it, the open of a group, a
    /// retry, and the loser drain alike. Until it is called this host refuses
    /// every group method with
    /// [`EffectGroupUnsupported`](lash_core_execution::RuntimeErrorCode::EffectGroupUnsupported)
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

    /// The closing/finalization seam over this host's effect journal (ADR 0099
    /// §7): the durable `closing` fact this host's `close` writes, and the
    /// four-step cursor a finalizer — or a redriven turn's
    /// `resume_closing_groups` — advances.
    pub fn group_closing(&self) -> Arc<dyn StoreEffectGroupClosing> {
        Arc::clone(&self.inner).into_group_closing()
    }

    /// Testing seam (FIG-3524): arm this journal's next `claim`, `finalize`
    /// or `renew` on a named replay key to return the `Store` error instead
    /// of reaching the row store.
    #[cfg(feature = "testing")]
    pub fn effect_journal_faults(&self) -> effect_replay_driver::EffectJournalFaults {
        self.inner.journal_faults()
    }
}

impl SqliteRuntimeEffectController {
    pub async fn open(path: &Path, scope: ExecutionScope) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options(path, scope, SqliteEffectReplayOptions::default()).await
    }

    pub async fn open_with_clock(
        path: &Path,
        scope: ExecutionScope,
        clock: Arc<dyn lash_core_execution::Clock>,
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
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "replay binding identity is the canonical form of the host-supplied database path (FIG-2971)"
    )]
    pub async fn open_with_options_and_clock(
        path: &Path,
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_effect_host_path(path)?;
        let binding_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        Ok(Self {
            inner: open_effect_replay_driver(
                path,
                &binding_path,
                StoreBacking::File,
                options,
                clock,
                Arc::new(RegistryAttachment::default()),
            )
            .await?,
            scope,
            turn_control_binding_id: Arc::from(format!("sqlite:{}", binding_path.display())),
        })
    }

    #[cfg(feature = "testing")]
    pub async fn memory(scope: ExecutionScope) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options(scope, SqliteEffectReplayOptions::default()).await
    }

    #[cfg(feature = "testing")]
    pub async fn memory_with_clock(
        scope: ExecutionScope,
        clock: Arc<dyn lash_core_execution::Clock>,
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
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    #[cfg(feature = "testing")]
    pub async fn memory_with_options_and_clock(
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core_execution::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Ok(Self {
            inner: open_effect_replay_memory_driver(options, clock).await?,
            scope,
            turn_control_binding_id: Arc::from(format!("sqlite-memory:{}", uuid::Uuid::new_v4())),
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.start_replay();
    }

    /// Testing seam (FIG-3524): arm this journal's next `claim`, `finalize`
    /// or `renew` on a named replay key to return the `Store` error instead
    /// of reaching the row store.
    #[cfg(feature = "testing")]
    pub fn effect_journal_faults(&self) -> effect_replay_driver::EffectJournalFaults {
        self.inner.journal_faults()
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
    binding_path: &Path,
    backing: StoreBacking,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core_execution::Clock>,
    registry: Arc<RegistryAttachment>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open(path).await?;
    ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay).await?;
    let signing_secret = conn
        .call(|connection| {
            connection.query_row(
                wait_sql(Schema::Main)
                    .meta_sqlite
                    .select_signing_secret
                    .sql(),
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
        settlement_notify::SettlementNotifierKey::for_file(binding_path),
    )))
}

#[cfg(feature = "testing")]
async fn open_effect_replay_memory_driver(
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core_execution::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplay>> {
    let conn = SqliteConnection::open_in_memory().await?;
    ensure_versioned_schema(&conn, SqliteDatabase::EffectReplay).await?;
    let signing_secret = conn
        .call(|connection| {
            connection.query_row(
                wait_sql(Schema::Main)
                    .meta_sqlite
                    .select_signing_secret
                    .sql(),
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
        settlement_notify::SettlementNotifierKey::for_memory(),
    )))
}

fn build_effect_replay_driver(
    conn: SqliteConnection,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core_execution::Clock>,
    signing_secret: Vec<u8>,
    completion_keys: CompletionKeys,
    registry: Arc<RegistryAttachment>,
    settlement_key: settlement_notify::SettlementNotifierKey,
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
            settlement_key,
        },
        await_events,
        clock,
        options.lease_timings,
        options.drain_budget,
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
    clock: Arc<dyn lash_core_execution::Clock>,
    /// The bound process registry whose file holds process-scope fences.
    registry: Arc<RegistryAttachment>,
    /// This store's identity in the process-wide settlement-notifier registry:
    /// the canonical database path for a file journal, so two hosts over one
    /// file wake each other's parked settlement readers.
    settlement_key: settlement_notify::SettlementNotifierKey,
}

impl SqliteEffectReplayRowStore {
    async fn fence_locations(&self) -> Result<FenceLocations, RuntimeEffectControllerError> {
        self.registry
            .ensure_attached(&self.conn)
            .await
            .map_err(effect_sqlite_error)
    }

    /// Wake every waiter parked on `group_key`'s next settlement — this
    /// host's own awaiter or another host's over the same file. Called after
    /// a rank write's commit has landed.
    fn notify_group_settled(&self, group_key: &str) {
        settlement_notify::notify_group_settled(&self.settlement_key, group_key);
    }
}

impl effect_replay_driver::sealed::EffectReplayBackend for SqliteEffectReplayRowStore {}

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
    schema: Schema,
    scope_id: &str,
    scope_json: &str,
) -> rusqlite::Result<bool> {
    let live: bool = tx.query_row(
        effect_sql(schema).journal.scope_is_quiescent.sql(),
        params![scope_id, scope_json],
        |row| row.get(0),
    )?;
    Ok(!live)
}

/// Whether any session catalog still owns an authorization lifetime in this
/// physical promise-owner scope. Callers read this in the same write
/// transaction that would insert the retirement fence.
pub(crate) fn scope_has_turn_cancel_closure_participant(
    tx: &rusqlite::Transaction<'_>,
    schema: Schema,
    scope_id: &str,
) -> rusqlite::Result<bool> {
    tx.query_row(
        closure_participant_exists_sql(schema),
        params![scope_id],
        |row| row.get(0),
    )
}

/// Scope-exact retirement (N4) of one non-session scope whose fence shares the journal's file:
/// the permanent fence first, then the scope's effect rows, group rows, and promise rows, all
/// in the caller's transaction.
pub(crate) fn retire_scope_rows(
    tx: &rusqlite::Transaction<'_>,
    schema: Schema,
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
    schema: Schema,
    scope_id: &str,
    now_ms: u64,
) -> rusqlite::Result<()> {
    tx.execute(
        fence_sql(schema).sqlite.insert_fence.sql(),
        params![scope_id, now_ms as i64],
    )?;
    Ok(())
}

/// Delete the effect rows, group rows, and promise rows of one scope from `schema`'s journal
/// tables.
pub(crate) fn delete_scope_rows(
    tx: &rusqlite::Transaction<'_>,
    schema: Schema,
    scope_id: &str,
    scope_json: &str,
) -> rusqlite::Result<usize> {
    let sql = effect_sql(schema);
    let deleted = tx.execute(sql.replay.delete_by_scope.sql(), params![scope_id])?;
    // Membership before the group rows it keys off (see the session path).
    tx.execute(sql.group_child.delete_by_scope.sql(), params![scope_id])?;
    tx.execute(sql.group.delete_by_scope.sql(), params![scope_id])?;
    tx.execute(
        wait_sql(schema).shared.delete_by_scope_json.sql(),
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
    journal_schema: Schema,
    fences: FenceLocations,
) -> rusqlite::Result<usize> {
    let mut scopes: Vec<(String, String)> = Vec::new();
    {
        let mut keyed = tx.prepare(
            effect_sql(journal_schema)
                .journal
                .select_session_free_scope_ids
                .sql(),
        )?;
        for key in keyed.query_map([], |row| row.get::<_, String>(0))? {
            let key = key?;
            if let Some(scope) = lash_core_execution::ExecutionScope::from_journal_key(&key) {
                #[expect(
                    clippy::expect_used,
                    reason = "`ExecutionScope` is a derived-`Serialize` enum of strings, so encoding it cannot fail"
                )]
                let scope_json = serde_json::to_string(&scope).expect("execution scopes serialize");
                scopes.push((key, scope_json));
            }
        }
        let mut waited = tx.prepare(
            wait_sql(journal_schema)
                .shared
                .select_session_free_scope_json
                .sql(),
        )?;
        for scope_json in waited.query_map([], |row| row.get::<_, String>(0))? {
            let scope_json = scope_json?;
            if let Ok(scope) =
                serde_json::from_str::<lash_core_execution::ExecutionScope>(&scope_json)
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

fn effect_sqlite_error(err: rusqlite::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.store_code(), err.to_string())
}

#[cfg(test)]
mod tests;
