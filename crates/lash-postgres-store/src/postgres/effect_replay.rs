//! PostgreSQL-backed runtime effect replay host.
//!
//! The claim/execute/renew/finalize state machine lives in
//! [`StoreEffectReplayDriver`]; this module is the PostgreSQL half of its
//! [`EffectReplayRowStore`] plug-in plus the host and controller types that
//! expose it. Row storage is all this module owns: the driver decides every
//! claim, replay, and drain. Every
//! atom runs in a server transaction that takes the row's write lock
//! (`SELECT … FOR UPDATE`, or a guarded `UPDATE`), so the read, the transition
//! decision, and the write it guards cannot interleave with a competing
//! claimant under `READ COMMITTED`.
//!
//! PostgreSQL's authoritative lease clock is the *server's*:
//! `transaction_timestamp()` stamps and compares every lease here, so fencing
//! survives skew between hosts — the database-authoritative lease boundary the
//! `Clock` contract states, pinned by `postgres_clock_contract`.
//! The driver's own clock is an explicit `SystemClock` because it only sleeps.

use crate::*;
use lash_sansio::SessionId;
use sha2::{Digest, Sha256};

use lash_core::facade_support::effect_replay_driver;
use lash_core::facade_support::effect_replay_driver::{
    AcceptedGroupChild, CompletionKeys, EffectClaimDecision, EffectClaimObservation,
    EffectClaimRequest, EffectFinalizeOutcome, EffectGroupColumn, EffectGroupRecord,
    EffectLeaseFence, EffectLeaseStamp, EffectReplayCapabilities, EffectReplayRowStore,
    EffectReplayVocabulary, EffectRowDefect, EffectRowStatus, EffectTerminal,
    StoreEffectReplayDriver, StoredEffectRow, StoredGroupSettlement, ToolBatchRedrive,
    UnsettledGroupChild, decide_effect_claim,
};

use lash_core::{GroupExecutors, StoreEffectGroupDrain};

use crate::await_event::{
    PostgresAwaitEventBackend, lock_scope, postgres_await_events, scope_is_retired, wait_sql,
};

use std::sync::LazyLock;

use lash_store_sql::Dialect;
use lash_store_sql::effect::EffectJournalStatements;
use lash_store_sql::effect::group::GroupStatements;
use lash_store_sql::effect::group_child::GroupChildStatements;
use lash_store_sql::effect::replay::ReplayStatements;
use lash_store_sql::effect::scope_retirement::ScopeRetirementStatements;

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary::postgres();

lash_store_sql::statements! {
    /// Journal-wide statements only PostgreSQL issues.
    pub(crate) struct EffectJournalPostgresStatements @ "effect_journal" {
        /// Preserve one retirement fence per execution scope owned by session
        /// `$1` before the session's journal rows are deleted.
        ///
        /// PostgreSQL stamps from the server clock and writes the boolean
        /// column as `FALSE`; SQLite binds a host-clock stamp and writes `0`.
        insert_session_scope_fences = "INSERT INTO effect_scope_retirements (
                 scope_id, retired_at_ms, artifact_cleanup_completed
             )
             SELECT scope_id,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    FALSE
             FROM (
                 SELECT DISTINCT scope_id FROM runtime_effect_replay
                 WHERE session_id = ?1
                 UNION
                 SELECT DISTINCT scope_id FROM runtime_effect_group
                 WHERE session_id = ?1
             ) AS retired
             ON CONFLICT (scope_id) DO NOTHING";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_replay` statements only PostgreSQL issues.
    pub(crate) struct ReplayPostgresStatements @ "effect_replay" {
        /// The row a claim decision reads, for `$1` (scope) / `$2` (replay
        /// key), under its write lock.
        ///
        /// `FOR UPDATE` is the whole fork: SQLite already holds the database
        /// write lock through `BEGIN IMMEDIATE` and needs no suffix.
        select_for_claim = "SELECT envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_expires_at_ms, due_at_ms
             FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2
             FOR UPDATE";

        /// Insert a fresh claim, reporting no row when a concurrent claimant
        /// won.
        ///
        /// `ON CONFLICT DO NOTHING` is how a concurrent inserter is detected:
        /// `FOR UPDATE` cannot lock a row that does not exist yet. SQLite's
        /// write lock makes the race unreachable, so its insert carries no
        /// conflict clause and a conflict stays an error.
        insert_claimed = "INSERT INTO runtime_effect_replay (
                scope_id, session_id, replay_key, envelope_hash,
                envelope_json, status, outcome_json, error_json, lease_owner_id,
                lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
                created_at_ms, updated_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8, ?9, ?10, ?13, NULL, ?11, ?12)
             ON CONFLICT (scope_id, replay_key) DO NOTHING";

        /// Write the terminal under the lease fence and report the child's
        /// group.
        ///
        /// The lease instant is the server's `transaction_timestamp()`, which
        /// is what makes PostgreSQL fencing survive host clock skew; SQLite
        /// binds its host clock instead, so the two texts fork.
        finalize_terminal = "UPDATE runtime_effect_replay
             SET status = ?6,
                 outcome_json = ?7,
                 error_json = ?8,
                 lease_owner_id = NULL,
                 lease_token = NULL,
                 lease_expires_at_ms = 0,
                 updated_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             WHERE scope_id = ?1
               AND replay_key = ?2
               AND envelope_hash = ?3
               AND lease_owner_id = ?4
               AND lease_token = ?5
               AND status = 'in_progress'
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             RETURNING group_key";

        /// Extend the lease of `?1` / `?2` by `?6` milliseconds if `?4` / `?5`
        /// still hold it. Forks for the same reason
        /// [`ReplayPostgresStatements::finalize_terminal`] does.
        renew_lease = "UPDATE runtime_effect_replay
             SET lease_expires_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint + ?6,
                 updated_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             WHERE scope_id = ?1
               AND replay_key = ?2
               AND envelope_hash = ?3
               AND lease_owner_id = ?4
               AND lease_token = ?5
               AND status = 'in_progress'
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_group` statements only PostgreSQL issues.
    pub(crate) struct GroupPostgresStatements @ "effect_group" {
        /// Record a group, returning the inserted row and nothing on a
        /// conflict.
        ///
        /// The `RETURNING` clause is the fork: it saves the read-back on the
        /// insert path, which SQLite performs unconditionally.
        insert_new = "INSERT INTO runtime_effect_group (
                group_key, scope_id, session_id, wake, loser_disposition,
                children, next_seq, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
             ON CONFLICT (group_key) DO NOTHING
             RETURNING group_key, scope_id, session_id, wake, loser_disposition,
                       children, created_at_ms";
    }
}

lash_store_sql::statements! {
    /// `runtime_effect_group_child` statements only PostgreSQL issues.
    pub(crate) struct GroupChildPostgresStatements @ "effect_group_child" {
        /// Retain one accepted child, returning the retained row and nothing
        /// on a conflict.
        ///
        /// The `RETURNING` clause is the fork, for the same reason it is on
        /// the group insert: it saves the read-back on the insert path, which
        /// SQLite performs unconditionally. `DO NOTHING` is reopen semantics —
        /// a reopen re-presents the membership it already accepted.
        insert_accepted = "INSERT INTO runtime_effect_group_child (
                group_key, position, replay_key,
                envelope_json, request_version, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (group_key, position) DO NOTHING
             RETURNING group_key, position, replay_key,
                       envelope_json, request_version";
    }
}

lash_store_sql::statements! {
    /// `effect_scope_retirements` statements only PostgreSQL issues.
    pub(crate) struct ScopeRetirementPostgresStatements @ "effect_scope_retirement" {
        /// Write the permanent fence of scope `?1`, keeping the first stamp.
        /// Stamped from the server clock, with a boolean cleanup flag.
        insert_fence = "INSERT INTO effect_scope_retirements (
                 scope_id, retired_at_ms, artifact_cleanup_completed
             )
             VALUES (?1, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT, FALSE)
             ON CONFLICT (scope_id) DO NOTHING";

        /// Every fenced scope whose artifact cleanup has not run. SQLite
        /// stores the flag as an integer.
        select_pending_artifact_cleanup = "SELECT scope_id FROM effect_scope_retirements
             WHERE artifact_cleanup_completed = FALSE
             ORDER BY scope_id";

        /// Record that scope `?1`'s artifact cleanup has run. Forks on the
        /// same boolean representation.
        complete_artifact_cleanup = "UPDATE effect_scope_retirements
             SET artifact_cleanup_completed = TRUE
             WHERE scope_id = ?1";
    }
}

/// Every effect-family statement, rendered once.
pub(crate) struct EffectSql {
    /// Journal-wide statements both backends issue verbatim.
    pub(crate) journal: EffectJournalStatements,
    /// Journal-wide statements only PostgreSQL issues.
    pub(crate) journal_postgres: EffectJournalPostgresStatements,
    /// `runtime_effect_replay` statements both backends issue verbatim.
    pub(crate) replay: ReplayStatements,
    /// `runtime_effect_replay` statements only PostgreSQL issues.
    pub(crate) replay_postgres: ReplayPostgresStatements,
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub(crate) group: GroupStatements,
    /// `runtime_effect_group` statements only PostgreSQL issues.
    pub(crate) group_postgres: GroupPostgresStatements,
    /// `runtime_effect_group_child` statements both backends issue verbatim.
    pub(crate) group_child: GroupChildStatements,
    /// `runtime_effect_group_child` statements only PostgreSQL issues.
    pub(crate) group_child_postgres: GroupChildPostgresStatements,
    /// `effect_scope_retirements` statements both backends issue verbatim.
    pub(crate) fence: ScopeRetirementStatements,
    /// `effect_scope_retirements` statements only PostgreSQL issues.
    pub(crate) fence_postgres: ScopeRetirementPostgresStatements,
}

static EFFECT_SQL: LazyLock<EffectSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    EffectSql {
        journal: EffectJournalStatements::render(dialect),
        journal_postgres: EffectJournalPostgresStatements::render(dialect),
        replay: ReplayStatements::render(dialect),
        replay_postgres: ReplayPostgresStatements::render(dialect),
        group: GroupStatements::render(dialect),
        group_postgres: GroupPostgresStatements::render(dialect),
        group_child: GroupChildStatements::render(dialect),
        group_child_postgres: GroupChildPostgresStatements::render(dialect),
        fence: ScopeRetirementStatements::render(dialect),
        fence_postgres: ScopeRetirementPostgresStatements::render(dialect),
    }
});

/// The effect-family statements, rendered once at first use and never again.
pub(crate) fn effect_sql() -> &'static EffectSql {
    &EFFECT_SQL
}

/// The PostgreSQL effect-replay driver: one shared state machine over
/// [`PostgresEffectReplayRowStore`].
type PostgresEffectReplay =
    StoreEffectReplayDriver<PostgresEffectReplayRowStore, PostgresAwaitEventBackend>;

#[derive(Clone, Debug, Default)]
pub struct PostgresEffectReplayOptions {
    /// Effect-replay lease timing capability. Hosts share the same
    /// [`LeaseTimings`] they configure on the runtime so effect leases expire
    /// on the same failover window as session and process leases.
    pub lease_timings: lash_core::facade_support::LeaseTimings,
}

#[derive(Clone)]
pub struct PostgresEffectHost {
    inner: Arc<PostgresEffectReplay>,
    pool: PgPool,
    turn_control_binding_id: Arc<str>,
}

#[derive(Clone)]
pub struct PostgresRuntimeEffectController {
    inner: Arc<PostgresEffectReplay>,
    scope: ExecutionScope,
    turn_control_binding_id: Arc<str>,
}

// The `AwaitEventResolver` / `EffectHost` / `RuntimeEffectController` surface of
// both types is the shared adapter in `effect_replay_driver::adapter`; this
// store only says which driver each handle forwards to.
impl effect_replay_driver::StoreReplayAdapter for PostgresEffectHost {
    type Persistence = PostgresEffectReplayRowStore;
    type AwaitEvents = PostgresAwaitEventBackend;
    fn replay_driver(&self) -> &Arc<PostgresEffectReplay> {
        &self.inner
    }
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.turn_control_binding_id.to_string())
    }
}

lash_core::impl_store_replay_await_event_resolver!(impl lash_core::AwaitEventResolver for PostgresEffectHost);

#[async_trait::async_trait]
impl effect_replay_driver::StoreReplayHost for PostgresEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.turn_control_binding_id.to_string()
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let scope_id = scope.journal_identity()?.key().to_string();
        let scope_json = serde_json::to_string(scope).map_err(|error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::RecordEncodingFailed,
                error.to_string(),
            )
        })?;
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        lock_scope(&mut tx, &scope_id)
            .await
            .map_err(retirement_error)?;
        let retired: bool = sqlx::query_scalar(effect_sql().fence.exists.sql())
            .bind(&scope_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(retirement_error)?;
        if retired {
            tx.rollback().await.map_err(retirement_error)?;
            return Err(RuntimeError::new(
                lash_core::RuntimeErrorCode::EffectScopeRetired,
                format!(
                    "effect scope `{scope_id}` has been retired and cannot admit a cancellation-closure participant"
                ),
            ));
        }
        sqlx::query(
            "INSERT INTO lash_turn_cancel_closure_participants
             (scope_id, participant_id, scope_json)
             VALUES ($1, $2, $3)
             ON CONFLICT (scope_id, participant_id) DO NOTHING",
        )
        .bind(&scope_id)
        .bind(participant_id)
        .bind(scope_json)
        .execute(&mut *tx)
        .await
        .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        let scope_id = scope.journal_identity()?.key().to_string();
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        lock_scope(&mut tx, &scope_id)
            .await
            .map_err(retirement_error)?;
        sqlx::query(
            "DELETE FROM lash_turn_cancel_closure_participants
             WHERE scope_id = $1 AND participant_id = $2",
        )
        .bind(scope_id)
        .bind(participant_id)
        .execute(&mut *tx)
        .await
        .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)
    }
}

impl effect_replay_driver::StoreReplayAdapter for PostgresRuntimeEffectController {
    type Persistence = PostgresEffectReplayRowStore;
    type AwaitEvents = PostgresAwaitEventBackend;
    fn replay_driver(&self) -> &Arc<PostgresEffectReplay> {
        &self.inner
    }
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.turn_control_binding_id.to_string())
    }
}

lash_core::impl_store_replay_await_event_resolver!(impl lash_core::AwaitEventResolver for PostgresRuntimeEffectController);

impl effect_replay_driver::StoreReplayController for PostgresRuntimeEffectController {
    fn execution_scope(&self) -> &ExecutionScope {
        &self.scope
    }
}

impl PostgresEffectHost {
    pub fn new(storage: &PostgresStorage) -> Self {
        Self::with_options(storage, PostgresEffectReplayOptions::default())
    }

    pub fn with_options(storage: &PostgresStorage, options: PostgresEffectReplayOptions) -> Self {
        Self::with_options_and_clock(
            storage,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
    }

    /// Construct a host with an explicit record/scheduling clock.
    pub fn with_options_and_clock(
        storage: &PostgresStorage,
        options: PostgresEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(build_effect_replay_driver(storage, options, clock)),
            pool: storage.pool.clone(),
            turn_control_binding_id: Arc::from(format!(
                "postgres:{}",
                hex_digest(&storage.await_event_signing_secret)
            )),
        }
    }

    pub fn start_replay(&self) {
        self.inner.start_replay();
    }

    /// Register the resolver that says how a grouped child is run.
    ///
    /// This is the host's one wiring seam: it is supplied here — by the host
    /// that owns those runners — rather than discovered from whatever session is
    /// in scope, and every path resolves through it, the open of a group, a
    /// retry, and the loser drain alike. Until it is called this host refuses an
    /// open rather than journaling a group nothing can run.
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

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl PostgresRuntimeEffectController {
    pub fn new(storage: &PostgresStorage, scope: ExecutionScope) -> Self {
        Self::with_options(storage, scope, PostgresEffectReplayOptions::default())
    }

    pub fn with_options(
        storage: &PostgresStorage,
        scope: ExecutionScope,
        options: PostgresEffectReplayOptions,
    ) -> Self {
        Self::with_options_and_clock(
            storage,
            scope,
            options,
            Arc::new(lash_core::facade_support::SystemClock),
        )
    }

    /// Construct a scoped controller with an explicit scheduling clock.
    ///
    /// PostgreSQL remains authoritative for lease timestamps and comparisons;
    /// this clock drives only effect sleeps, busy backoff, and renewal cadence.
    pub fn with_options_and_clock(
        storage: &PostgresStorage,
        scope: ExecutionScope,
        options: PostgresEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(build_effect_replay_driver(storage, options, clock)),
            scope,
            turn_control_binding_id: Arc::from(format!(
                "postgres:{}",
                hex_digest(&storage.await_event_signing_secret)
            )),
        }
    }

    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

fn build_effect_replay_driver(
    storage: &PostgresStorage,
    options: PostgresEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> PostgresEffectReplay {
    // The driver's clock only sleeps: `Sleep` effect due times, busy-retry
    // backoff, and the lease renewal interval. Every lease stamp and comparison
    // is the server's (`transaction_timestamp()`), so `PostgresStorage` needs no
    // injectable time source and this `SystemClock` is deliberately explicit
    // rather than a private `current_epoch_ms()` call per statement.
    let await_events = postgres_await_events(
        storage.pool.clone(),
        Arc::clone(&storage.await_event_signing_secret),
        Arc::clone(&clock),
    );
    StoreEffectReplayDriver::new(
        PostgresEffectReplayRowStore {
            pool: storage.pool.clone(),
        },
        await_events,
        clock,
        options.lease_timings,
    )
}

/// PostgreSQL storage atoms for the durable effect journal.
///
/// `pub` only because it names an associated type of the shared adapter; the
/// module is private, so nothing outside this crate can reach it.
pub struct PostgresEffectReplayRowStore {
    pool: PgPool,
}

impl effect_replay_driver::sealed::EffectReplayBackend for PostgresEffectReplayRowStore {}

#[async_trait::async_trait]
impl EffectReplayRowStore for PostgresEffectReplayRowStore {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    fn capabilities(&self) -> EffectReplayCapabilities {
        EffectReplayCapabilities {
            completion_keys: CompletionKeys::Issued,
            tool_batch_redrive: ToolBatchRedrive::ChildrenFirst,
        }
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        // Only session-free scopes can carry a retirement tombstone, so only
        // they take the scope lock retirement writes it under; a session scope
        // has nothing here to race with.
        if fence_session_free_scope(&mut tx, request.session_id.as_ref(), &request.scope_id).await?
        {
            tx.commit().await.map_err(effect_store_error)?;
            return Ok(EffectClaimObservation::ScopeRetired);
        }
        // The server's transaction clock is the authoritative lease instant:
        // one stable value for every comparison and derived expiry below.
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let observation = self.claim_in_transaction(&mut tx, request, now_ms).await;
        tx.commit().await.map_err(effect_store_error)?;
        observation
    }

    async fn replay_row_exists(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<bool, RuntimeEffectControllerError> {
        sqlx::query_scalar(effect_sql().replay.exists_by_key.sql())
            .bind(scope_id)
            .bind(replay_key)
            .fetch_one(&self.pool)
            .await
            .map_err(effect_store_error)
    }

    /// Writes the terminal and, for a grouped child, allocates its settlement
    /// rank — in the normative order (N1), in one transaction.
    ///
    /// The fenced `UPDATE` runs first and `RETURNING group_key` is what makes
    /// "bump only on rowcount 1" structural rather than remembered: no row
    /// returned is no bump, and the group bumped is the one the child's own row
    /// records rather than one passed in beside it.
    ///
    /// `UPDATE … SET next_seq = next_seq + 1` on a single row takes that row's
    /// lock and is correct under `READ COMMITTED`: no lost update, and no read
    /// of unfenced sibling state. It is also the group's serialization point —
    /// every sibling's finalize queues behind it — which ADR 0065 accepts with a
    /// pre-identified, backend-local escape (a per-group sequence generator or a
    /// sharded counter) that needs no contract movement.
    ///
    /// The lock order here is child row then group row, and the group row is
    /// created in its own committed transaction by
    /// [`open_group`](Self::open_group), so nothing ever takes them the other
    /// way round (N2).
    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<EffectFinalizeOutcome, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        let claimed: Option<Option<String>> =
            sqlx::query_scalar(effect_sql().replay_postgres.finalize_terminal.sql())
                .bind(&fence.scope_id)
                .bind(&fence.replay_key)
                .bind(&fence.envelope_hash)
                .bind(&fence.owner_id)
                .bind(&fence.lease_token)
                .bind(terminal.status().column())
                .bind(terminal.outcome_json())
                .bind(terminal.error_json())
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;

        let Some(group_key) = claimed else {
            // The fence moved. Roll back rather than commit, and allocate
            // nothing: a taken-over driver that burned a number here would
            // advance a group it no longer owns, and the unique index cannot
            // catch it because the burned number never reaches a child row.
            // Rolled back explicitly rather than by drop, so the statement that
            // discards the work is the one an implementor reads — and so a
            // rollback failure is reported instead of swallowed by a destructor.
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(EffectFinalizeOutcome::FenceMoved);
        };
        let settlement_seq = match group_key {
            None => None,
            Some(group_key) => {
                let allocated: Option<i64> =
                    sqlx::query_scalar(effect_sql().group.bump_next_seq.sql())
                        .bind(&group_key)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(effect_store_error)?;
                let allocated = allocated.ok_or_else(|| missing_group_row(&group_key))?;
                sqlx::query(effect_sql().replay.set_settlement_seq.sql())
                    .bind(&fence.scope_id)
                    .bind(&fence.replay_key)
                    .bind(allocated)
                    .execute(&mut *tx)
                    .await
                    .map_err(effect_store_error)?;
                Some(u64::try_from(allocated).map_err(|_| {
                    effect_store_message(
                        StoreError::StoredDataCorrupt {
                            record_kind: "RuntimeEffectGroup",
                            message: format!("next_seq must be non-negative, got {allocated}"),
                        }
                        .to_string(),
                    )
                })?)
            }
        };
        tx.commit().await.map_err(effect_store_error)?;
        Ok(EffectFinalizeOutcome::Written { settlement_seq })
    }

    /// Records the group and reports the row **as it stands durably**, so a
    /// reopen is fenced against what the journal holds rather than against the
    /// opening process's memory.
    ///
    /// One statement, so one transaction, committed before any child of this
    /// group claims (N2) — the read-back rides the same statement through
    /// `RETURNING` for the insert and a second query only when the insert
    /// conflicted, and neither touches a child row. `DO NOTHING` rather than an
    /// upsert: reopening a group must not reset `next_seq`, which would re-seat
    /// recorded children at ranks a caller has already consumed.
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
        membership: &[AcceptedGroupChild],
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
        // One transaction so the retirement fence and the insert are read and
        // written under the scope lock (N4); it still holds no child-row lock,
        // so the N2 lock order against `finalize` is unchanged.
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        if fence_session_free_scope(&mut tx, record.session_id.as_ref(), &record.scope_id).await? {
            tx.commit().await.map_err(effect_store_error)?;
            return Err(effect_replay_driver::scope_retired(&record.scope_id));
        }
        // Children before the group row, in this transaction (ADR 0065 N2), so
        // the group row's existence implies its complete membership
        // (ADR 0099 §3). The returned row is not read: on a first open it is
        // what was just offered, and on a reopen the conflict path leaves it
        // empty — either way the membership a caller acts on is the one
        // `read_group_membership` reports.
        for child in membership {
            sqlx::query(effect_sql().group_child_postgres.insert_accepted.sql())
                .bind(&record.group_key)
                .bind(child.position as i64)
                .bind(&child.replay_key)
                .bind(&child.envelope_json)
                .bind(i64::from(child.request_version))
                .bind(record.created_at_ms as i64)
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;
        }
        let inserted = sqlx::query(effect_sql().group_postgres.insert_new.sql())
            .bind(&record.group_key)
            .bind(&record.scope_id)
            .bind(record.session_id.as_deref())
            .bind(record.wake.column())
            .bind(record.loser_disposition.column())
            .bind(record.children as i64)
            .bind(record.created_at_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(effect_store_error)?;
        if let Some(row) = inserted {
            tx.commit().await.map_err(effect_store_error)?;
            return stored_group_record(row);
        }
        // The conflict path: some earlier open owns this key, and its row — not
        // the one just refused — is what a reopen must be fenced against.
        let existing = sqlx::query(effect_sql().group.select_by_key.sql())
            .bind(&record.group_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .ok_or_else(|| missing_group_row(&record.group_key))?;
        tx.commit().await.map_err(effect_store_error)?;
        stored_group_record(existing)
    }

    async fn read_group_membership(
        &self,
        group_key: &str,
    ) -> Result<Vec<AcceptedGroupChild>, RuntimeEffectControllerError> {
        let rows = sqlx::query(effect_sql().group_child.select_membership.sql())
            .bind(group_key)
            .fetch_all(&self.pool)
            .await
            .map_err(effect_store_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(AcceptedGroupChild {
                    position: u64_from_sql(
                        "RuntimeEffectGroupChild",
                        "position",
                        row.try_get::<i64, _>("position")
                            .map_err(effect_store_error)?,
                    )? as usize,
                    replay_key: row.try_get("replay_key").map_err(effect_store_error)?,
                    envelope_json: row.try_get("envelope_json").map_err(effect_store_error)?,
                    request_version: u64_from_sql(
                        "RuntimeEffectGroupChild",
                        "request_version",
                        row.try_get::<i64, _>("request_version")
                            .map_err(effect_store_error)?,
                    )? as u16,
                })
            })
            .collect()
    }

    /// Reads the group row without writing one, so a drain reads the declared
    /// disposition instead of inserting a group it was only asking about.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
        let row = sqlx::query(effect_sql().group.select_by_key.sql())
            .bind(group_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(effect_store_error)?;
        row.map(stored_group_record).transpose()
    }

    /// Reads the group's children that hold no rank: the complement of
    /// [`read_group_settlement`](Self::read_group_settlement)'s
    /// `settlement_seq IS NOT NULL`.
    ///
    /// Served by `idx_lash_runtime_effect_replay_group_unsettled`, whose
    /// predicate is exactly this filter. That index is the 55 generation's whole
    /// content, and it arrived with the drain (FIG-1536) — the workload that
    /// makes the plan matter — rather than with the read, because on this tier
    /// every relation is stamped into a component generation and an index is a
    /// `SCHEMA_VERSION` bump with a migration row per live generation. The
    /// asymmetry with sqlite, whose equivalent index shipped a generation
    /// earlier without a bump, is documented where that migration is declared.
    async fn read_unsettled_group_children(
        &self,
        group_key: &str,
    ) -> Result<Vec<UnsettledGroupChild>, RuntimeEffectControllerError> {
        let rows = sqlx::query(effect_sql().replay.select_unsettled_children.sql())
            .bind(group_key)
            .fetch_all(&self.pool)
            .await
            .map_err(effect_store_error)?;
        rows.into_iter().map(unsettled_group_child).collect()
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: usize,
    ) -> Result<Option<StoredGroupSettlement>, RuntimeEffectControllerError> {
        let Some(offset) = rank.checked_sub(1) else {
            return Ok(None);
        };
        let row = sqlx::query(effect_sql().replay.select_settlement_by_rank.sql())
            .bind(group_key)
            .bind(offset as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(effect_store_error)?;
        row.map(stored_group_settlement).transpose()
    }

    async fn renew(
        &self,
        fence: &EffectLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let changed = sqlx::query(effect_sql().replay_postgres.renew_lease.sql())
            .bind(&fence.scope_id)
            .bind(&fence.replay_key)
            .bind(&fence.envelope_hash)
            .bind(&fence.owner_id)
            .bind(&fence.lease_token)
            .bind(lease_ttl_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        Ok(changed == 1)
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
        retirement: &lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let sql = effect_sql();
        let (children_sql, membership_sql, groups_sql, key, fenced_scope) = match retirement {
            lash_core::EffectJournalRetirement::Session { session_id } => (
                sql.replay.delete_by_session.sql(),
                sql.group_child.delete_by_session.sql(),
                sql.group.delete_by_session.sql(),
                session_id.as_str().to_string(),
                None,
            ),
            #[expect(
                clippy::expect_used,
                reason = "`retired_scope` is `Some` for exactly the two variants this arm matches, and neither carries a session id whose validation could refuse the journal identity"
            )]
            lash_core::EffectJournalRetirement::Process { .. }
            | lash_core::EffectJournalRetirement::RuntimeOperation { .. } => {
                let scope = retirement
                    .retired_scope()
                    .expect("scope-exact retirements name their scope");
                let identity = scope.journal_identity().expect(
                    "process and runtime-operation scopes always form durable journal identities",
                );
                (
                    sql.replay.delete_by_scope.sql(),
                    sql.group_child.delete_by_scope.sql(),
                    sql.group.delete_by_scope.sql(),
                    identity.key().to_string(),
                    Some(scope),
                )
            }
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        // Scope-exact retirement (N4): tombstone first, under the scope lock
        // every admission path takes, then the rows; the promise rows go in
        // the same transaction so the fence and the deletions land together.
        if let Some(scope) = fenced_scope.as_ref() {
            lock_scope(&mut tx, &key).await.map_err(retirement_error)?;
            let has_closure_participant = scope_has_turn_cancel_closure_participant(&mut tx, &key)
                .await
                .map_err(retirement_error)?;
            if has_closure_participant {
                tx.rollback().await.map_err(retirement_error)?;
                return Err(effect_replay_driver::scope_not_quiescent(&key));
            }
            // The quiescence proof is read under the same scope lock the
            // fence is written under, so no child can start between the
            // proof and the deletions.
            let scope_json = serde_json::to_string(scope).map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                    err.to_string(),
                )
            })?;
            if retirement.gate() == Some(lash_core::EffectRetirementGate::WhenQuiescent)
                && !scope_is_quiescent(&mut tx, &key, &scope_json)
                    .await
                    .map_err(retirement_error)?
            {
                tx.rollback().await.map_err(retirement_error)?;
                return Err(effect_replay_driver::scope_not_quiescent(&key));
            }
            let children = retire_scope_rows_tx(&mut tx, &key, &scope_json)
                .await
                .map_err(retirement_error)?;
            tx.commit().await.map_err(retirement_error)?;
            return Ok(children);
        }
        sqlx::query(sql.journal_postgres.insert_session_scope_fences.sql())
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        let children = sqlx::query(children_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?
            .rows_affected();
        // Membership before the group rows it keys off: the statement selects
        // the retiring groups, so deleting them first would strand every
        // accepted request and leave it naming environment bytes this
        // retirement is about to reclaim (ADR 0099 §3).
        sqlx::query(membership_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        sqlx::query(groups_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)?;
        Ok(children as usize)
    }

    async fn reinstate_scope(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        lock_scope(&mut tx, scope_id)
            .await
            .map_err(retirement_error)?;
        sqlx::query(effect_sql().fence.delete_by_scope.sql())
            .bind(scope_id)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)
    }

    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<lash_core::ExecutionScope>, RuntimeError> {
        let keys: Vec<String> = sqlx::query_scalar(
            effect_sql()
                .fence_postgres
                .select_pending_artifact_cleanup
                .sql(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        })?;
        keys.into_iter()
            .map(|key| {
                lash_core::ExecutionScope::from_journal_key(&key).ok_or_else(|| {
                    RuntimeError::new(
                        lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                        format!("invalid retired effect scope key `{key}`"),
                    )
                })
            })
            .collect()
    }

    async fn complete_artifact_owner_retirement(&self, scope_id: &str) -> Result<(), RuntimeError> {
        sqlx::query(effect_sql().fence_postgres.complete_artifact_cleanup.sql())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                    error.to_string(),
                )
            })?;
        Ok(())
    }
}

/// Whether nothing under `scope_id` is still live: no `in_progress` effect
/// row, no group row still waiting for a child that has not been journaled
/// (an open group, or a run-to-completion close whose drain has not yet
/// claimed every loser), and no unresolved promise under the scope (a wait
/// row is a continuation's durable wait: it stays unresolved until the
/// promise settles or the wait is cancelled). Read under the scope lock.
pub(crate) async fn scope_is_quiescent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
    scope_json: &str,
) -> Result<bool, sqlx::Error> {
    let live: bool = sqlx::query_scalar(effect_sql().journal.scope_is_quiescent.sql())
        .bind(scope_id)
        .bind(scope_json)
        .fetch_one(&mut **tx)
        .await?;
    Ok(!live)
}

/// Whether any session catalog still owns an authorization lifetime in this
/// physical promise-owner scope. Callers hold the scope advisory lock and
/// retain it through any retirement-fence write.
pub(crate) async fn scope_has_turn_cancel_closure_participant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM lash_turn_cancel_closure_participants
            WHERE scope_id = $1
         )",
    )
    .bind(scope_id)
    .fetch_one(&mut **tx)
    .await
}

/// Scope-exact retirement (N4) of one non-session scope under the caller's
/// scope lock: the permanent fence first, then the scope's promise rows,
/// effect rows, and group rows. Returns the effect rows deleted.
pub(crate) async fn retire_scope_rows_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
    scope_json: &str,
) -> Result<usize, sqlx::Error> {
    let sql = effect_sql();
    sqlx::query(sql.fence_postgres.insert_fence.sql())
        .bind(scope_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(wait_sql().shared.delete_by_scope_json.sql())
        .bind(scope_json)
        .execute(&mut **tx)
        .await?;
    let children = sqlx::query(sql.replay.delete_by_scope.sql())
        .bind(scope_id)
        .execute(&mut **tx)
        .await?
        .rows_affected();
    // Membership before the group rows it keys off (see `retire_journal`).
    sqlx::query(sql.group_child.delete_by_scope.sql())
        .bind(scope_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query(sql.group.delete_by_scope.sql())
        .bind(scope_id)
        .execute(&mut **tx)
        .await?;
    Ok(children as usize)
}

impl PostgresEffectReplayRowStore {
    /// Read the row under its write lock, ask the transition table, and apply
    /// whatever write it prescribes.
    ///
    /// A fresh claim inserts with `ON CONFLICT DO NOTHING`: `FOR UPDATE` cannot
    /// lock a row that does not exist yet, so a concurrent inserter is detected
    /// by the conflict and the row is re-read under its lock. That re-read is
    /// decided by the same table, so a racing claimant sees `Busy` (or the
    /// terminal) rather than a second claim.
    async fn claim_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        request: &EffectClaimRequest,
        now_ms: u64,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let row = select_effect_row_for_update(tx, &request.scope_id, &request.replay_key).await?;
        let decision = decide_effect_claim(row.as_ref(), request, now_ms);
        let stamp = match decision {
            EffectClaimDecision::Insert(stamp) => {
                if insert_claimed_row(tx, request, &stamp).await? {
                    return Ok(EffectClaimObservation::Claimed {
                        due_at_ms: stamp.due_at_ms,
                    });
                }
                // A concurrent claimant inserted the row `FOR UPDATE` could not
                // lock because it did not exist yet. Re-read it under its lock
                // and let the same table decide again; the second decision can
                // no longer be `Insert`, so it settles as a takeover or a
                // report — never a second claim of a live lease.
                let Some(conflicted) =
                    select_effect_row_for_update(tx, &request.scope_id, &request.replay_key)
                        .await?
                else {
                    return Ok(EffectClaimObservation::CorruptRow {
                        defect: EffectRowDefect::VanishedUnderClaim,
                    });
                };
                match decide_effect_claim(Some(&conflicted), request, now_ms) {
                    EffectClaimDecision::TakeOver(stamp) => stamp,
                    EffectClaimDecision::Report(observation) => return Ok(observation),
                    EffectClaimDecision::Insert(_) => {
                        debug_assert!(
                            false,
                            "decide_effect_claim must never prescribe an insert for a row it \
                             was given: `Insert` is the no-row arm"
                        );
                        return Ok(EffectClaimObservation::CorruptRow {
                            defect: EffectRowDefect::VanishedUnderClaim,
                        });
                    }
                }
            }
            EffectClaimDecision::TakeOver(stamp) => stamp,
            EffectClaimDecision::Report(observation) => return Ok(observation),
        };
        take_over_expired_lease(tx, request, &stamp).await?;
        Ok(EffectClaimObservation::Claimed {
            due_at_ms: stamp.due_at_ms,
        })
    }
}

async fn select_effect_row_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
    replay_key: &str,
) -> Result<Option<StoredEffectRow>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay_postgres.select_for_claim.sql())
        .bind(scope_id)
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    row.map(stored_effect_row).transpose()
}

fn stored_effect_row(row: PgRow) -> Result<StoredEffectRow, RuntimeEffectControllerError> {
    let corrupt = |field, value| {
        effect_store_message(
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeEffectReplay",
                message: format!("{field} must be non-negative, got {value}"),
            }
            .to_string(),
        )
    };
    let lease_expires_at_ms = row.get::<i64, _>("lease_expires_at_ms");
    let due_at_ms = row.get::<Option<i64>, _>("due_at_ms");
    let state = effect_replay_driver::EffectRowState::from_columns(
        row.get("status"),
        row.get("outcome_json"),
        row.get("error_json"),
    );
    Ok(StoredEffectRow {
        envelope_hash: row.get("envelope_hash"),
        envelope_json: row.get("envelope_json"),
        state,
        lease_expires_at_ms: u64::try_from(lease_expires_at_ms)
            .map_err(|_| corrupt("lease_expires_at_ms", lease_expires_at_ms))?,
        due_at_ms: due_at_ms
            .map(|value| u64::try_from(value).map_err(|_| corrupt("due_at_ms", value)))
            .transpose()?,
    })
}

/// Insert a fresh claim, reporting `false` when a concurrent inserter won.
async fn insert_claimed_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> Result<bool, RuntimeEffectControllerError> {
    let inserted = sqlx::query(effect_sql().replay_postgres.insert_claimed.sql())
        .bind(&request.scope_id)
        .bind(request.session_id.as_deref())
        .bind(&request.replay_key)
        .bind(&request.envelope_hash)
        .bind(&request.envelope_json)
        .bind(EffectRowStatus::InProgress.column())
        .bind(&request.owner_id)
        .bind(&request.lease_token)
        .bind(stamp.lease_expires_at_ms as i64)
        .bind(stamp.due_at_ms.map(|value| value as i64))
        .bind(stamp.now_ms as i64)
        .bind(stamp.now_ms as i64)
        .bind(request.group_key.as_deref())
        .execute(&mut **tx)
        .await
        .map_err(effect_store_error)?
        .rows_affected();
    Ok(inserted == 1)
}

async fn take_over_expired_lease(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> Result<(), RuntimeEffectControllerError> {
    sqlx::query(effect_sql().replay.take_over_lease.sql())
        .bind(&request.scope_id)
        .bind(&request.replay_key)
        .bind(&request.owner_id)
        .bind(&request.lease_token)
        .bind(stamp.lease_expires_at_ms as i64)
        .bind(stamp.due_at_ms.map(|value| value as i64))
        .bind(stamp.now_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    Ok(())
}

fn stored_group_settlement(
    row: PgRow,
) -> Result<StoredGroupSettlement, RuntimeEffectControllerError> {
    let sequence = row.get::<i64, _>("settlement_seq");
    let state = effect_replay_driver::EffectRowState::from_columns(
        row.get("status"),
        row.get("outcome_json"),
        row.get("error_json"),
    );
    Ok(StoredGroupSettlement {
        sequence: u64::try_from(sequence).map_err(|_| {
            effect_store_message(
                StoreError::StoredDataCorrupt {
                    record_kind: "RuntimeEffectReplay",
                    message: format!("settlement_seq must be non-negative, got {sequence}"),
                }
                .to_string(),
            )
        })?,
        replay_key: row.get("replay_key"),
        state,
    })
}

/// The durably recorded group row, read back through the same column mapping
/// that wrote it.
fn stored_group_record(row: PgRow) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
    let children = row.get::<i64, _>("children");
    let created_at_ms = row.get::<i64, _>("created_at_ms");
    Ok(EffectGroupRecord {
        group_key: row.get("group_key"),
        scope_id: row.get("scope_id"),
        session_id: row
            .get::<Option<String>, _>("session_id")
            .map(SessionId::from),
        wake: group_column("wake rule", row.get("wake"))?,
        loser_disposition: group_column("loser disposition", row.get("loser_disposition"))?,
        children: usize::try_from(children)
            .map_err(|_| group_corrupt(format!("children must be non-negative, got {children}")))?,
        created_at_ms: u64::try_from(created_at_ms).map_err(|_| {
            group_corrupt(format!(
                "created_at_ms must be non-negative, got {created_at_ms}"
            ))
        })?,
    })
}

/// A persisted group column read back through the same mapping that wrote it,
/// refusing a value no version of this runtime writes.
fn group_column<T: EffectGroupColumn>(
    column: &'static str,
    value: String,
) -> Result<T, RuntimeEffectControllerError> {
    EffectGroupColumn::from_column(&value)
        .ok_or_else(|| group_corrupt(format!("unknown effect group {column} `{value}`")))
}

fn group_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroup",
            message,
        }
        .to_string(),
    )
}

fn unsettled_group_child(row: PgRow) -> Result<UnsettledGroupChild, RuntimeEffectControllerError> {
    let lease_expires_at_ms = row.get::<i64, _>("lease_expires_at_ms");
    let state = effect_replay_driver::EffectRowState::from_columns(
        row.get("status"),
        row.get("outcome_json"),
        row.get("error_json"),
    );
    Ok(UnsettledGroupChild {
        scope_id: row.get("scope_id"),
        replay_key: row.get("replay_key"),
        envelope_json: row.get("envelope_json"),
        state,
        lease_expires_at_ms: u64::try_from(lease_expires_at_ms).map_err(|_| {
            effect_store_message(
                StoreError::StoredDataCorrupt {
                    record_kind: "RuntimeEffectReplay",
                    message: format!(
                        "lease_expires_at_ms must be non-negative, got {lease_expires_at_ms}"
                    ),
                }
                .to_string(),
            )
        })?,
    })
}

/// A grouped child whose group row is gone is a corrupt journal, not a silently
/// ungrouped settlement: the rank it should have taken can never be served, so
/// reporting success would hide a group no caller can finish consuming.
fn missing_group_row(group_key: &str) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroup",
            message: format!(
                "grouped effect child finalized against missing group row `{group_key}`; \
                 its settlement rank can never be served"
            ),
        }
        .to_string(),
    )
}

/// Take the scope lock and read the retirement fence for a session-free scope.
/// Session scopes are never scope-retired and take no lock here, exactly as
/// their promise atoms take the session lock instead.
async fn fence_session_free_scope(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Option<&SessionId>,
    scope_id: &str,
) -> Result<bool, RuntimeEffectControllerError> {
    if session_id.is_some() {
        return Ok(false);
    }
    lock_scope(tx, scope_id).await.map_err(effect_store_error)?;
    scope_is_retired(&mut **tx, scope_id)
        .await
        .map_err(|err| effect_store_message(err.to_string()))
}

fn effect_store_error(err: sqlx::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.store_code(), err.to_string())
}

fn effect_store_message(message: String) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.store_code(), message)
}

// `#[path]` is load-bearing, not redundant: this file is itself reached by
// `#[path = "postgres/effect_replay.rs"]` from `lib.rs`, and Rust resolves a
// path-ed module's children against the *directory holding that file* rather
// than a directory named for the module. Without this, `mod tests;` looks for
// `src/postgres/tests.rs`. The SQLite sibling needs no attribute because its
// parent is an ordinary `mod effect_replay;`. `schema_shape.rs` carries the same
// workaround for the same reason.
#[path = "effect_replay/tests.rs"]
#[cfg(test)]
mod tests;
