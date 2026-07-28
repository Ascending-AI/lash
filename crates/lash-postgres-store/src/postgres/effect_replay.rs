use crate::*;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::await_event::PostgresAwaitEvents;

const POSTGRES_EFFECT_STATUS_IN_PROGRESS: &str = "in_progress";
const POSTGRES_EFFECT_STATUS_COMPLETED: &str = "completed";
const POSTGRES_EFFECT_STATUS_FAILED: &str = "failed";
const POSTGRES_EFFECT_BUSY_POLL: Duration = Duration::from_millis(25);

static POSTGRES_EFFECT_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub struct PostgresEffectReplayOptions {
    /// Effect-replay lease timing capability. Hosts share the same
    /// [`LeaseTimings`] they configure on the runtime so effect leases expire
    /// on the same failover window as session and process leases.
    pub lease_timings: lash_core::LeaseTimings,
}

struct PostgresEffectReplayInner {
    pool: PgPool,
    owner_id: String,
    lease_counter: AtomicU64,
    replay_mode: AtomicBool,
    lease_timings: lash_core::LeaseTimings,
    await_events: PostgresAwaitEvents,
}

#[derive(Clone)]
pub struct PostgresEffectHost {
    inner: Arc<PostgresEffectReplayInner>,
}

#[derive(Clone)]
pub struct PostgresRuntimeEffectController {
    inner: Arc<PostgresEffectReplayInner>,
    scope: ExecutionScope,
}

struct PostgresClaimedEffect {
    scope_id: String,
    replay_key: String,
    envelope_hash: String,
    lease_token: String,
    due_at_ms: Option<u64>,
}

enum PostgresPreparedEffect {
    ReplayMismatch {
        recorded_envelope: Box<CanonicalRuntimeEffectEnvelope>,
        stored_envelope_hash: String,
    },
    ReplayOutcome {
        outcome: Box<RuntimeEffectOutcome>,
        due_at_ms: Option<u64>,
    },
    ReplayError(RuntimeEffectControllerError),
    Claimed(PostgresClaimedEffect),
    Busy {
        retry_at_ms: u64,
    },
}

struct PostgresEffectRow {
    envelope_hash: String,
    envelope_json: String,
    status: String,
    outcome_json: Option<String>,
    error_json: Option<String>,
    lease_expires_at_ms: i64,
    due_at_ms: Option<i64>,
}

impl PostgresEffectHost {
    pub fn new(storage: &PostgresStorage) -> Self {
        Self::with_options(storage, PostgresEffectReplayOptions::default())
    }

    pub fn with_options(storage: &PostgresStorage, options: PostgresEffectReplayOptions) -> Self {
        Self {
            inner: Arc::new(PostgresEffectReplayInner::new(
                storage.pool.clone(),
                Arc::clone(&storage.await_event_signing_secret),
                options,
            )),
        }
    }

    pub fn start_replay(&self) {
        self.inner.replay_mode.store(true, Ordering::SeqCst);
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
        scope.validate_for_durable_host()?;
        self.inner.await_events.key_for(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, RuntimeError> {
        self.inner.await_events.resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, RuntimeError> {
        self.inner.await_events.peek(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, RuntimeError> {
        self.inner
            .await_events
            .await_resolution(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.await_events.revoke_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.await_events.cancel_session(session_id).await
    }
}

#[async_trait::async_trait]
impl EffectHost for PostgresEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        scope.validate_for_durable_host()?;
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
        scope.validate_for_durable_host()?;
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
        let result = match retirement {
            lash_core::EffectJournalRetirement::Session {
                session_id,
                incarnation_id,
            } => {
                sqlx::query(
                    "DELETE FROM lash_runtime_effect_replay
                 WHERE session_id = $1 AND incarnation_id = $2",
                )
                .bind(session_id)
                .bind(incarnation_id.as_str())
                .execute(&self.inner.pool)
                .await
            }
            lash_core::EffectJournalRetirement::Process { process_id } => {
                let identity = ExecutionScope::process(process_id)
                    .journal_identity()
                    .expect("process scopes always form durable journal identities");
                sqlx::query("DELETE FROM lash_runtime_effect_replay WHERE scope_id = $1")
                    .bind(identity.key())
                    .execute(&self.inner.pool)
                    .await
            }
        }
        .map_err(|error| {
            RuntimeError::new("postgres_effect_journal_retirement", error.to_string())
        })?;
        Ok(result.rows_affected() as usize)
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
            inner: Arc::new(PostgresEffectReplayInner::new(
                storage.pool.clone(),
                Arc::clone(&storage.await_event_signing_secret),
                options,
            )),
            scope,
        }
    }

    pub fn start_replay(&self) {
        self.inner.replay_mode.store(true, Ordering::SeqCst);
    }

    async fn prepare_effect(
        &self,
        envelope: &RuntimeEffectEnvelope,
        reconstructed_envelope: &CanonicalRuntimeEffectEnvelope,
    ) -> Result<PostgresPreparedEffect, RuntimeEffectControllerError> {
        let replay_key = envelope
            .invocation
            .replay_key()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    "postgres_effect_replay_key_missing",
                    "runtime effect envelope requires replay.key",
                )
            })?
            .to_string();
        let envelope_hash = reconstructed_envelope.hash().to_string();
        let envelope_json =
            serde_json::to_string(reconstructed_envelope).map_err(postgres_effect_encode_error)?;
        let journal_identity = self
            .scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?;
        let scope_id = journal_identity.key().to_string();
        let session_id = journal_identity.session_id().map(str::to_string);
        let incarnation_id = journal_identity
            .incarnation_id()
            .map(|value| value.as_str().to_string());
        let lease_token = self.inner.next_lease_token();
        let replay_mode = self.inner.replay_mode.load(Ordering::SeqCst);
        let owner_id = self.inner.owner_id.clone();

        let mut tx = self
            .inner
            .pool
            .begin()
            .await
            .map_err(postgres_effect_store_error)?;
        let now = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| postgres_effect_store_message(err.to_string()))?;
        let due_at_ms = postgres_effect_sleep_due_at_ms(envelope, now);
        let lease_expires_at_ms = now.saturating_add(self.inner.lease_timings.ttl_ms());
        let outcome = match postgres_select_effect_row_for_update(&mut tx, &scope_id, &replay_key)
            .await?
        {
            Some(row) => {
                self.prepare_locked_effect_row(
                    &mut tx,
                    PostgresPrepareInputs {
                        row,
                        scope_id,
                        replay_key,
                        envelope_hash,
                        owner_id,
                        lease_token,
                        due_at_ms,
                        now,
                    },
                )
                .await
            }
            None if replay_mode => Err(RuntimeEffectControllerError::new(
                "postgres_effect_replay_missing",
                format!(
                    "no recorded runtime effect for scope `{scope_id}` and replay key `{replay_key}`"
                ),
            )),
            None => {
                let due_at_param = due_at_ms.map(|value| value as i64);
                let inserted = sqlx::query(
                    "INSERT INTO lash_runtime_effect_replay (
                        scope_id, session_id, incarnation_id, replay_key, envelope_hash,
                        envelope_json, status, outcome_json, error_json, lease_owner_id,
                        lease_token, lease_expires_at_ms, due_at_ms, created_at_ms, updated_at_ms
                     )
                     VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, NULL, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (scope_id, replay_key) DO NOTHING",
                )
                .bind(&scope_id)
                .bind(session_id)
                .bind(incarnation_id)
                .bind(&replay_key)
                .bind(&envelope_hash)
                .bind(&envelope_json)
                .bind(POSTGRES_EFFECT_STATUS_IN_PROGRESS)
                .bind(&owner_id)
                .bind(&lease_token)
                .bind(lease_expires_at_ms as i64)
                .bind(due_at_param)
                .bind(now as i64)
                .bind(now as i64)
                .execute(&mut *tx)
                .await
                .map_err(postgres_effect_store_error)?
                .rows_affected();
                if inserted == 1 {
                    Ok(PostgresPreparedEffect::Claimed(PostgresClaimedEffect {
                        scope_id,
                        replay_key,
                        envelope_hash,
                        lease_token,
                        due_at_ms,
                    }))
                } else {
                    let row =
                        postgres_select_effect_row_for_update(&mut tx, &scope_id, &replay_key)
                            .await?
                            .ok_or_else(|| {
                                RuntimeEffectControllerError::new(
                                    "postgres_effect_replay_corrupt_row",
                                    "effect replay insert conflicted but no row could be selected",
                                )
                            })?;
                    self.prepare_locked_effect_row(
                        &mut tx,
                        PostgresPrepareInputs {
                            row,
                            scope_id,
                            replay_key,
                            envelope_hash,
                            owner_id,
                            lease_token,
                            due_at_ms,
                            now,
                        },
                    )
                    .await
                }
            }
        };
        tx.commit().await.map_err(postgres_effect_store_error)?;
        outcome
    }

    async fn prepare_locked_effect_row(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        inputs: PostgresPrepareInputs,
    ) -> Result<PostgresPreparedEffect, RuntimeEffectControllerError> {
        let PostgresPrepareInputs {
            row,
            scope_id,
            replay_key,
            envelope_hash,
            owner_id,
            lease_token,
            due_at_ms,
            now,
        } = inputs;
        if row.envelope_hash != envelope_hash {
            let recorded_envelope: CanonicalRuntimeEffectEnvelope =
                serde_json::from_str(&row.envelope_json).map_err(postgres_effect_decode_error)?;
            return Ok(PostgresPreparedEffect::ReplayMismatch {
                recorded_envelope: Box::new(recorded_envelope),
                stored_envelope_hash: row.envelope_hash,
            });
        }
        let existing_due_at_ms = row.due_at_ms.map(|value| value as u64);
        match row.status.as_str() {
            POSTGRES_EFFECT_STATUS_COMPLETED => {
                let Some(json) = row.outcome_json else {
                    return Err(RuntimeEffectControllerError::new(
                        "postgres_effect_replay_corrupt_row",
                        "completed runtime effect row is missing outcome_json",
                    ));
                };
                let outcome = serde_json::from_str(&json).map_err(postgres_effect_decode_error)?;
                Ok(PostgresPreparedEffect::ReplayOutcome {
                    outcome: Box::new(outcome),
                    due_at_ms: existing_due_at_ms,
                })
            }
            POSTGRES_EFFECT_STATUS_FAILED => {
                let Some(json) = row.error_json else {
                    return Err(RuntimeEffectControllerError::new(
                        "postgres_effect_replay_corrupt_row",
                        "failed runtime effect row is missing error_json",
                    ));
                };
                let err = serde_json::from_str(&json).map_err(postgres_effect_decode_error)?;
                Ok(PostgresPreparedEffect::ReplayError(err))
            }
            POSTGRES_EFFECT_STATUS_IN_PROGRESS if row.lease_expires_at_ms as u64 > now => {
                Ok(PostgresPreparedEffect::Busy {
                    retry_at_ms: row.lease_expires_at_ms as u64,
                })
            }
            POSTGRES_EFFECT_STATUS_IN_PROGRESS => {
                let due_at_ms = existing_due_at_ms.or(due_at_ms);
                let due_at_param = due_at_ms.map(|value| value as i64);
                sqlx::query(
                    "UPDATE lash_runtime_effect_replay
                     SET lease_owner_id = $3,
                         lease_token = $4,
                         lease_expires_at_ms = $5,
                         due_at_ms = $6,
                         updated_at_ms = $7
                     WHERE scope_id = $1 AND replay_key = $2",
                )
                .bind(&scope_id)
                .bind(&replay_key)
                .bind(&owner_id)
                .bind(&lease_token)
                .bind(now.saturating_add(self.inner.lease_timings.ttl_ms()) as i64)
                .bind(due_at_param)
                .bind(now as i64)
                .execute(&mut **tx)
                .await
                .map_err(postgres_effect_store_error)?;
                Ok(PostgresPreparedEffect::Claimed(PostgresClaimedEffect {
                    scope_id,
                    replay_key,
                    envelope_hash,
                    lease_token,
                    due_at_ms,
                }))
            }
            other => Err(RuntimeEffectControllerError::new(
                "postgres_effect_replay_corrupt_row",
                format!("unknown runtime effect replay status `{other}`"),
            )),
        }
    }

    async fn finalize_effect(
        &self,
        claim: &PostgresClaimedEffect,
        outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (status, outcome_json, error_json) = match outcome {
            Ok(outcome) => (
                POSTGRES_EFFECT_STATUS_COMPLETED,
                Some(serde_json::to_string(outcome).map_err(postgres_effect_encode_error)?),
                None,
            ),
            Err(err) => (
                POSTGRES_EFFECT_STATUS_FAILED,
                None,
                Some(serde_json::to_string(err).map_err(postgres_effect_encode_error)?),
            ),
        };
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
        .bind(&claim.scope_id)
        .bind(&claim.replay_key)
        .bind(&claim.envelope_hash)
        .bind(&self.inner.owner_id)
        .bind(&claim.lease_token)
        .bind(status)
        .bind(outcome_json)
        .bind(error_json)
        .execute(&self.inner.pool)
        .await
        .map_err(postgres_effect_store_error)?
        .rows_affected();
        if changed != 1 {
            return Err(RuntimeEffectControllerError::new(
                "postgres_effect_replay_lease_lost",
                format!(
                    "runtime effect replay lease was lost before finalizing scope `{}` replay key `{}`",
                    claim.scope_id, claim.replay_key
                ),
            ));
        }
        Ok(())
    }

    async fn renew_effect_lease(
        &self,
        claim: &PostgresClaimedEffect,
    ) -> Result<(), RuntimeEffectControllerError> {
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
        .bind(&claim.scope_id)
        .bind(&claim.replay_key)
        .bind(&claim.envelope_hash)
        .bind(&self.inner.owner_id)
        .bind(&claim.lease_token)
        .bind(self.inner.lease_timings.ttl_ms() as i64)
        .execute(&self.inner.pool)
        .await
        .map_err(postgres_effect_store_error)?
        .rows_affected();
        if changed != 1 {
            return Err(RuntimeEffectControllerError::new(
                "postgres_effect_replay_lease_lost",
                format!(
                    "runtime effect replay lease was lost while executing scope `{}` replay key `{}`",
                    claim.scope_id, claim.replay_key
                ),
            ));
        }
        Ok(())
    }

    async fn execute_claimed_effect_with_renewal(
        &self,
        claim: &PostgresClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let renew_every = self.inner.lease_timings.renew_interval();
        let effect = self.execute_claimed_effect(claim, envelope, local_executor);
        tokio::pin!(effect);
        let renew_sleep = tokio::time::sleep(renew_every);
        tokio::pin!(renew_sleep);

        loop {
            tokio::select! {
                result = &mut effect => return result,
                _ = &mut renew_sleep => {
                    self.renew_effect_lease(claim).await?;
                    renew_sleep.as_mut().reset(tokio::time::Instant::now() + renew_every);
                }
            }
        }
    }

    async fn execute_claimed_effect(
        &self,
        claim: &PostgresClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(envelope.command, RuntimeEffectCommand::Sleep { .. }) {
            postgres_effect_sleep_until_due(claim.due_at_ms).await;
            return Ok(RuntimeEffectOutcome::Sleep);
        }
        match envelope.command {
            RuntimeEffectCommand::PeekAwaitEvent { key } => {
                let resolution = self
                    .peek_await_event(&key)
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution })
            }
            RuntimeEffectCommand::AwaitEvent { key } => {
                let lash_core::RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    ..
                } = local_executor.into_await_event_options()?;
                let resolution = self
                    .inner
                    .await_events
                    .await_resolution_with_clock(&key, cancellation, deadline, clock.as_ref())
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
            }
            RuntimeEffectCommand::Process { command } => {
                let result = local_executor.into_process()?.execute(*command).await?;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            _ => local_executor.execute(envelope).await,
        }
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
        scope.validate_for_durable_host()?;
        self.inner.await_events.key_for(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveOutcome, RuntimeError> {
        self.inner.await_events.resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
    ) -> Result<Option<lash_core::Resolution>, RuntimeError> {
        self.inner.await_events.peek(key).await
    }

    async fn await_await_event(
        &self,
        key: &lash_core::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<lash_core::Resolution, RuntimeError> {
        self.inner
            .await_events
            .await_resolution(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.await_events.revoke_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.await_events.cancel_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for PostgresRuntimeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.scope
            .validate_for_durable_host()
            .map_err(RuntimeEffectControllerError::from)?;
        let reconstructed_envelope = envelope.canonical_form()?;
        let replay_trace = local_executor.replay_validation_trace().cloned();
        loop {
            match self
                .prepare_effect(&envelope, &reconstructed_envelope)
                .await?
            {
                PostgresPreparedEffect::ReplayMismatch {
                    recorded_envelope,
                    stored_envelope_hash,
                } => {
                    validate_replayed_effect_envelope(
                        recorded_envelope.as_ref(),
                        &reconstructed_envelope,
                        "postgres_effect_replay_hash_conflict",
                        replay_trace.as_ref(),
                    )?;
                    return Err(RuntimeEffectControllerError::new(
                        "runtime_effect_envelope_canonical_hash_invariant",
                        format!(
                            "stored envelope_hash {stored_envelope_hash} did not match the persisted canonical envelope hash {}",
                            recorded_envelope.hash()
                        ),
                    ));
                }
                PostgresPreparedEffect::ReplayOutcome { outcome, due_at_ms } => {
                    postgres_effect_sleep_until_due(due_at_ms).await;
                    return Ok(*outcome);
                }
                PostgresPreparedEffect::ReplayError(err) => return Err(err),
                PostgresPreparedEffect::Claimed(claim) => {
                    let result = self
                        .execute_claimed_effect_with_renewal(&claim, envelope, local_executor)
                        .await;
                    let finalize = self.finalize_effect(&claim, &result).await;
                    return match (result, finalize) {
                        (Ok(outcome), Ok(())) => Ok(outcome),
                        (Err(err), Ok(())) => Err(err),
                        (_, Err(err)) => Err(err),
                    };
                }
                PostgresPreparedEffect::Busy { retry_at_ms } => {
                    postgres_effect_sleep_until_retry(retry_at_ms).await;
                }
            }
        }
    }
}

struct PostgresPrepareInputs {
    row: PostgresEffectRow,
    scope_id: String,
    replay_key: String,
    envelope_hash: String,
    owner_id: String,
    lease_token: String,
    due_at_ms: Option<u64>,
    now: u64,
}

impl PostgresEffectReplayInner {
    fn new(pool: PgPool, signing_secret: Arc<[u8]>, options: PostgresEffectReplayOptions) -> Self {
        let sequence = POSTGRES_EFFECT_OWNER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let await_events = PostgresAwaitEvents::new(pool.clone(), signing_secret);
        Self {
            pool,
            owner_id: format!(
                "pid{}-{sequence}-{}",
                std::process::id(),
                current_epoch_ms()
            ),
            lease_counter: AtomicU64::new(1),
            replay_mode: AtomicBool::new(false),
            lease_timings: options.lease_timings,
            await_events,
        }
    }

    fn next_lease_token(&self) -> String {
        let sequence = self.lease_counter.fetch_add(1, Ordering::SeqCst);
        format!("{}:{sequence}", self.owner_id)
    }
}

async fn postgres_select_effect_row_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
    replay_key: &str,
) -> Result<Option<PostgresEffectRow>, RuntimeEffectControllerError> {
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
    .map_err(postgres_effect_store_error)?;
    Ok(row.map(postgres_effect_row))
}

fn postgres_effect_row(row: PgRow) -> PostgresEffectRow {
    PostgresEffectRow {
        envelope_hash: row.get("envelope_hash"),
        envelope_json: row.get("envelope_json"),
        status: row.get("status"),
        outcome_json: row.get("outcome_json"),
        error_json: row.get("error_json"),
        lease_expires_at_ms: row.get("lease_expires_at_ms"),
        due_at_ms: row.get("due_at_ms"),
    }
}

fn postgres_effect_sleep_due_at_ms(envelope: &RuntimeEffectEnvelope, now: u64) -> Option<u64> {
    match envelope.command {
        RuntimeEffectCommand::Sleep { duration_ms } => Some(now.saturating_add(duration_ms)),
        _ => None,
    }
}

async fn postgres_effect_sleep_until_due(due_at_ms: Option<u64>) {
    let Some(due_at_ms) = due_at_ms else {
        return;
    };
    let now = current_epoch_ms();
    if due_at_ms > now {
        tokio::time::sleep(Duration::from_millis(due_at_ms - now)).await;
    }
}

async fn postgres_effect_sleep_until_retry(retry_at_ms: u64) {
    let now = current_epoch_ms();
    let delay = if retry_at_ms > now {
        Duration::from_millis(retry_at_ms - now).min(POSTGRES_EFFECT_BUSY_POLL)
    } else {
        POSTGRES_EFFECT_BUSY_POLL
    };
    tokio::time::sleep(delay).await;
}

fn postgres_effect_store_error(err: sqlx::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new("postgres_effect_replay_store", err.to_string())
}

fn postgres_effect_store_message(message: String) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new("postgres_effect_replay_store", message)
}

fn postgres_effect_encode_error(err: serde_json::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        "postgres_effect_replay_encode",
        format!("failed to encode runtime effect replay row: {err}"),
    )
}

fn postgres_effect_decode_error(err: serde_json::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        "postgres_effect_replay_decode",
        format!("failed to decode runtime effect replay row: {err}"),
    )
}
