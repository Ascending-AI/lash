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
    AcceptedGroupChild, CompletionKeys, EffectCancelOutcome, EffectCancelRequest,
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectCommitState,
    EffectDischargeOutcome, EffectDischargeRequest, EffectFinalizeOutcome,
    EffectGroupChildCommitOutcome, EffectGroupChildCommitRequest, EffectGroupColumn,
    EffectGroupRecord, EffectLeaseFence, EffectLeaseStamp, EffectReplayCapabilities,
    EffectReplayRowStore, EffectReplayVocabulary, EffectRowDefect, EffectRowStatus, EffectTerminal,
    StoreEffectReplayDriver, StoredChildArbitration, StoredEffectRow, StoredGroupSettlement,
    ToolBatchRedrive, UnsettledGroupChild, decide_effect_claim,
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
                lease_expires_at_ms, due_at_ms, commit_state, drain_input
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
                commit_state, commit_seq, created_at_ms, updated_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8, ?9, ?10, ?13, NULL, 'pending', NULL, ?11, ?12)
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

        /// The shared `select_arbitration_by_key` read — plus `drain_input`
        /// for the §4 boundary's read-back — under the replay row's
        /// `FOR UPDATE` lock.
        ///
        /// The lock is the whole fork: a claim that reads the minting child's
        /// commit state without it would see the pre-commit value while a
        /// `decide_cancel` UPDATE is in flight, then admit a row whose insert
        /// outlives the decision. The lock waits that decision out, and
        /// PostgreSQL re-evaluates the row against what committed.
        select_arbitration_by_key_locked = "SELECT commit_state, commit_seq, group_key, drain_input FROM runtime_effect_replay
             WHERE scope_id = ?1 AND replay_key = ?2
             FOR UPDATE";

        /// Release an ungrouped, uncommitted derivation under the complete live lease fence.
        release_uncommitted_derivation = "UPDATE runtime_effect_replay
             SET lease_expires_at_ms = 0,
                 updated_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             WHERE scope_id = ?1
               AND replay_key = ?2
               AND envelope_hash = ?3
               AND lease_owner_id = ?4
               AND lease_token = ?5
               AND status = 'in_progress'
               AND group_key IS NULL
               AND commit_state = 'pending'
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint";

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
        /// The `RETURNING` clause is the fork: it saves the read-back on the
        /// insert path, which SQLite performs unconditionally.
        insert_new = "INSERT INTO runtime_effect_group (
                group_key, scope_id, session_id, wake, loser_disposition,
                expected_children, next_seq, next_commit_seq, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)
             ON CONFLICT (group_key) DO NOTHING
             RETURNING group_key, scope_id, session_id, wake, loser_disposition,
                       expected_children, created_at_ms";
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
                envelope_json, command_version, created_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (group_key, position) DO NOTHING
             RETURNING group_key, position, replay_key,
                       envelope_json, command_version";
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

        /// Forks on the same boolean representation.
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
            crate::turn_ingress::turn_ingress_sql()
                .closure_participants
                .insert_new
                .sql(),
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
            crate::turn_ingress::turn_ingress_sql()
                .closure_participants
                .delete_participant
                .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .closure_participants
            .exists_for_scope
            .sql(),
    )
    .bind(scope_id)
    .fetch_one(&mut **tx)
    .await
}

/// Scope-exact retirement (N4) of one non-session scope under the caller's scope lock: the
/// permanent fence first, then the scope's promise rows, effect rows, and group rows.
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
#[path = "effect_replay/row_store.rs"]
mod row_store;
#[path = "effect_replay/tests.rs"]
#[cfg(test)]
mod tests;
