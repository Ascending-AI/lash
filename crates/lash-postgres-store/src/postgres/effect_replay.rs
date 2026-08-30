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

use lash_core::facade_support::effect_replay_driver;
use lash_core::facade_support::effect_replay_driver::{
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectFinalizeOutcome,
    EffectGroupColumn, EffectGroupRecord, EffectLeaseFence, EffectLeaseStamp, EffectReplayRowStore,
    EffectReplayVocabulary, EffectRowDefect, EffectRowStatus, EffectTerminal,
    StoreEffectReplayDriver, StoredEffectRow, StoredGroupSettlement, UnsettledGroupChild,
    decide_effect_claim,
};

use lash_core::{GroupExecutors, StoreEffectGroupDrain};

use crate::await_event::{PostgresAwaitEventBackend, postgres_await_events};
use tokio_util::sync::CancellationToken;

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary::postgres();

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
}

#[derive(Clone)]
pub struct PostgresRuntimeEffectController {
    inner: Arc<PostgresEffectReplay>,
    scope: ExecutionScope,
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
    /// retry, and the loser drain alike. Until it is called this host answers
    /// [`RuntimeEffectController::supports_effect_groups`] `false` and refuses an
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

#[async_trait::async_trait]
impl AwaitEventResolver for PostgresEffectHost {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(lash_core::CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(lash_core::CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, RuntimeError> {
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
impl EffectHost for PostgresEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate()?;
        let controller = PostgresRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
        };
        ScopedEffectController::shared(Arc::new(controller), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scope.validate()?;
        let controller = PostgresRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
        };
        Ok(Some(ScopedEffectController::shared(
            Arc::new(controller),
            scope,
        )?))
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash_core::ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> Result<lash_core::ToolIntentPreparation, RuntimeError> {
        Ok(lash_core::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    async fn retire_effect_journal(
        &self,
        retirement: lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.inner.retire_effect_journal(retirement).await
    }
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
        }
    }

    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for PostgresRuntimeEffectController {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(lash_core::CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(lash_core::CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> Result<lash_core::AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, RuntimeError> {
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
impl RuntimeEffectController for PostgresRuntimeEffectController {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: lash_core::RuntimeErrorCode,
    ) -> Result<lash_core::RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(lash_core::RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<lash_core::TurnControlParticipation, RuntimeError> {
        Ok(lash_core::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(
            envelope.command,
            lash_core::RuntimeEffectCommand::ToolBatch { .. }
        ) {
            // Re-enter the coordinator on redrive so each child command is
            // reconstructed and crosses its own key-addressed journal row.
            // The aggregate remains durable: after the child drain settles,
            // the ordinary driver records (or validates) the ToolBatch outcome.
            let settled = local_executor.execute(envelope.clone()).await;
            return Box::pin(self.inner.execute_effect(
                &self.scope,
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move { settled }),
            ))
            .await;
        }
        Box::pin(
            self.inner
                .execute_effect(&self.scope, envelope, local_executor),
        )
        .await
    }

    /// `true` exactly when this host has a registered
    /// [`GroupExecutors`] resolver.
    ///
    /// The group methods below are implemented against the durable journal, so
    /// the remaining question is where a child's runner comes from: the resolver
    /// is what supplies the `'static` executors the flag's other half requires,
    /// since a child must be able to outlive its caller to honor
    /// [`LoserPolicy::RunToCompletion`](lash_core::LoserPolicy::RunToCompletion).
    /// A host with no resolver would admit a group and then have nothing to run
    /// it with, which is the drift this answer forecloses.
    fn supports_effect_groups(&self) -> bool {
        self.inner.supports_effect_groups()
    }

    /// Delegated to the shared driver exactly as `execute_effect` is: the group
    /// host is one implementation over
    /// [`EffectReplayRowStore`](lash_core::facade_support::effect_replay_driver::EffectReplayRowStore),
    /// and this store contributes the substrate half of it rather than a second
    /// copy of the state machine.
    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
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
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Box::pin(self.inner.close_effect_group(&handle, disposition)).await
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
struct PostgresEffectReplayRowStore {
    pool: PgPool,
}

impl effect_replay_driver::sealed::EffectReplayBackend for PostgresEffectReplayRowStore {}

#[async_trait::async_trait]
impl EffectReplayRowStore for PostgresEffectReplayRowStore {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        // The server's transaction clock is the authoritative lease instant:
        // one stable value for every comparison and derived expiry below.
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let observation = self.claim_in_transaction(&mut tx, request, now_ms).await;
        tx.commit().await.map_err(effect_store_error)?;
        observation
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
        let claimed: Option<Option<String>> = sqlx::query_scalar(
            "UPDATE lash_runtime_effect_replay
             SET status = $6,
                 outcome_json = $7,
                 error_json = $8,
                 lease_owner_id = NULL,
                 lease_token = NULL,
                 lease_expires_at_ms = 0,
                 updated_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             WHERE scope_id = $1
               AND replay_key = $2
               AND envelope_hash = $3
               AND lease_owner_id = $4
               AND lease_token = $5
               AND status = 'in_progress'
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             RETURNING group_key",
        )
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
                let allocated: Option<i64> = sqlx::query_scalar(
                    "UPDATE lash_runtime_effect_group
                     SET next_seq = next_seq + 1
                     WHERE group_key = $1
                     RETURNING next_seq",
                )
                .bind(&group_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;
                let allocated = allocated.ok_or_else(|| missing_group_row(&group_key))?;
                sqlx::query(
                    "UPDATE lash_runtime_effect_replay
                     SET settlement_seq = $3
                     WHERE scope_id = $1 AND replay_key = $2",
                )
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
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
        let inserted = sqlx::query(
            "INSERT INTO lash_runtime_effect_group (
                group_key, scope_id, session_id, wake, loser_disposition,
                children, next_seq, created_at_ms
             )
             VALUES ($1, $2, $3, $4, $5, $6, 0, $7)
             ON CONFLICT (group_key) DO NOTHING
             RETURNING group_key, scope_id, session_id, wake, loser_disposition,
                       children, created_at_ms",
        )
        .bind(&record.group_key)
        .bind(&record.scope_id)
        .bind(record.session_id.as_deref())
        .bind(record.wake.column())
        .bind(record.loser_disposition.column())
        .bind(record.children as i64)
        .bind(record.created_at_ms as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(effect_store_error)?;
        if let Some(row) = inserted {
            return stored_group_record(row);
        }
        // The conflict path: some earlier open owns this key, and its row — not
        // the one just refused — is what a reopen must be fenced against.
        let existing = sqlx::query(
            "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    children, created_at_ms
             FROM lash_runtime_effect_group
             WHERE group_key = $1",
        )
        .bind(&record.group_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(effect_store_error)?
        .ok_or_else(|| missing_group_row(&record.group_key))?;
        stored_group_record(existing)
    }

    /// Reads the group row without writing one, so a drain reads the declared
    /// disposition instead of inserting a group it was only asking about.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
        let row = sqlx::query(
            "SELECT group_key, scope_id, session_id, wake, loser_disposition,
                    children, created_at_ms
             FROM lash_runtime_effect_group
             WHERE group_key = $1",
        )
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
        let rows = sqlx::query(
            "SELECT scope_id, replay_key, envelope_json, status, outcome_json, error_json, lease_expires_at_ms
             FROM lash_runtime_effect_replay
             WHERE group_key = $1 AND settlement_seq IS NULL
             ORDER BY replay_key",
        )
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
        let row = sqlx::query(
            "SELECT settlement_seq, replay_key, status, outcome_json, error_json
             FROM lash_runtime_effect_replay
             WHERE group_key = $1 AND settlement_seq IS NOT NULL
             ORDER BY settlement_seq
             LIMIT 1 OFFSET $2",
        )
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
        let changed = sqlx::query(
            "UPDATE lash_runtime_effect_replay
             SET lease_expires_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint + $6,
                 updated_at_ms = floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint
             WHERE scope_id = $1
               AND replay_key = $2
               AND envelope_hash = $3
               AND lease_owner_id = $4
               AND lease_token = $5
               AND status = 'in_progress'
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
        )
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
        let (children_sql, groups_sql, key) = match retirement {
            lash_core::EffectJournalRetirement::Session { session_id } => (
                "DELETE FROM lash_runtime_effect_replay WHERE session_id = $1",
                "DELETE FROM lash_runtime_effect_group WHERE session_id = $1",
                session_id.clone(),
            ),
            lash_core::EffectJournalRetirement::Process { process_id } => {
                let identity = ExecutionScope::process(process_id.clone())
                    .journal_identity()
                    .expect("process scopes always form durable journal identities");
                (
                    "DELETE FROM lash_runtime_effect_replay WHERE scope_id = $1",
                    "DELETE FROM lash_runtime_effect_group WHERE scope_id = $1",
                    identity.key().to_string(),
                )
            }
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        let children = sqlx::query(children_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?
            .rows_affected();
        sqlx::query(groups_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)?;
        Ok(children as usize)
    }
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
    let row = sqlx::query(
        "SELECT envelope_hash, envelope_json, status, outcome_json, error_json,
                lease_expires_at_ms, due_at_ms
         FROM lash_runtime_effect_replay
         WHERE scope_id = $1 AND replay_key = $2
         FOR UPDATE",
    )
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
    let inserted = sqlx::query(
        "INSERT INTO lash_runtime_effect_replay (
            scope_id, session_id, replay_key, envelope_hash,
            envelope_json, status, outcome_json, error_json, lease_owner_id,
            lease_token, lease_expires_at_ms, due_at_ms, group_key, settlement_seq,
            created_at_ms, updated_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, NULL, NULL, $7, $8, $9, $10, $13, NULL, $11, $12)
         ON CONFLICT (scope_id, replay_key) DO NOTHING",
    )
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
    sqlx::query(
        "UPDATE lash_runtime_effect_replay
         SET lease_owner_id = $3,
             lease_token = $4,
             lease_expires_at_ms = $5,
             due_at_ms = $6,
             updated_at_ms = $7
         WHERE scope_id = $1 AND replay_key = $2",
    )
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
        session_id: row.get("session_id"),
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
