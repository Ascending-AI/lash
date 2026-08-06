//! PostgreSQL-backed runtime effect replay host.
//!
//! The claim/execute/renew/finalize state machine lives in
//! [`EffectReplayDriver`]; this module is the PostgreSQL half of its
//! persistence port plus the host and controller types that expose it. Every
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
    EffectClaimDecision, EffectClaimObservation, EffectClaimRequest, EffectLeaseFence,
    EffectLeaseStamp, EffectReplayDriver, EffectReplayPersistence, EffectReplayVocabulary,
    EffectRowDefect, EffectRowStatus, EffectTerminal, StoredEffectRow, decide_effect_claim,
};

use crate::await_event::{PostgresAwaitEventBackend, postgres_await_events};

const VOCABULARY: EffectReplayVocabulary = EffectReplayVocabulary {
    code_prefix: "postgres",
};

/// The PostgreSQL effect-replay driver: one shared state machine over
/// [`PostgresEffectReplayPersistence`].
type PostgresEffectReplay =
    EffectReplayDriver<PostgresEffectReplayPersistence, PostgresAwaitEventBackend>;

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
        Self {
            inner: Arc::new(build_effect_replay_driver(storage, options)),
        }
    }

    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for PostgresEffectHost {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
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
        Self {
            inner: Arc::new(build_effect_replay_driver(storage, options)),
            scope,
        }
    }

    pub fn start_replay(&self) {
        self.inner.start_replay();
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for PostgresRuntimeEffectController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
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

fn build_effect_replay_driver(
    storage: &PostgresStorage,
    options: PostgresEffectReplayOptions,
) -> PostgresEffectReplay {
    // The driver's clock only sleeps: `Sleep` effect due times, busy-retry
    // backoff, and the lease renewal interval. Every lease stamp and comparison
    // is the server's (`transaction_timestamp()`), so `PostgresStorage` needs no
    // injectable time source and this `SystemClock` is deliberately explicit
    // rather than a private `current_epoch_ms()` call per statement.
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::facade_support::SystemClock);
    let await_events = postgres_await_events(
        storage.pool.clone(),
        Arc::clone(&storage.await_event_signing_secret),
        Arc::clone(&clock),
    );
    EffectReplayDriver::new(
        PostgresEffectReplayPersistence {
            pool: storage.pool.clone(),
        },
        await_events,
        clock,
        options.lease_timings,
    )
}

/// PostgreSQL storage atoms for the durable effect journal.
struct PostgresEffectReplayPersistence {
    pool: PgPool,
}

impl effect_replay_driver::sealed::EffectReplayBackend for PostgresEffectReplayPersistence {}

#[async_trait::async_trait]
impl EffectReplayPersistence for PostgresEffectReplayPersistence {
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

    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let changed = sqlx::query(
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
               AND lease_expires_at_ms > floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
        )
        .bind(&fence.scope_id)
        .bind(&fence.replay_key)
        .bind(&fence.envelope_hash)
        .bind(&fence.owner_id)
        .bind(&fence.lease_token)
        .bind(terminal.status().column())
        .bind(terminal.outcome_json())
        .bind(terminal.error_json())
        .execute(&self.pool)
        .await
        .map_err(effect_store_error)?
        .rows_affected();
        Ok(changed == 1)
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

    async fn retire_journal(
        &self,
        retirement: &lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let result = match retirement {
            lash_core::EffectJournalRetirement::Session { session_id } => {
                sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE session_id = $1")
                    .bind(session_id)
                    .execute(&self.pool)
                    .await
            }
            lash_core::EffectJournalRetirement::Process { process_id } => {
                let identity = ExecutionScope::process(process_id.clone())
                    .journal_identity()
                    .expect("process scopes always form durable journal identities");
                sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE scope_id = $1")
                    .bind(identity.key())
                    .execute(&self.pool)
                    .await
            }
        }
        .map_err(|error| {
            RuntimeError::new("postgres_effect_journal_retirement", error.to_string())
        })?;
        Ok(result.rows_affected() as usize)
    }
}

impl PostgresEffectReplayPersistence {
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
    Ok(row.map(stored_effect_row))
}

fn stored_effect_row(row: PgRow) -> StoredEffectRow {
    StoredEffectRow {
        envelope_hash: row.get("envelope_hash"),
        envelope_json: row.get("envelope_json"),
        status: row.get("status"),
        outcome_json: row.get("outcome_json"),
        error_json: row.get("error_json"),
        lease_expires_at_ms: row.get::<i64, _>("lease_expires_at_ms") as u64,
        due_at_ms: row
            .get::<Option<i64>, _>("due_at_ms")
            .map(|value| value as u64),
    }
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
            lease_token, lease_expires_at_ms, due_at_ms, created_at_ms, updated_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, NULL, NULL, $7, $8, $9, $10, $11, $12)
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

fn effect_store_error(err: sqlx::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.code("store"), err.to_string())
}

fn effect_store_message(message: String) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(VOCABULARY.code("store"), message)
}
