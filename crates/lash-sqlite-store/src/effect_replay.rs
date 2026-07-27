//! SQLite-backed runtime effect replay host (tokio-rusqlite port of the the prior store
//! store's `effect_replay`).
//!
//! The public surface is identical to the prior implementation — same type names
//! (`SqliteEffectHost`, `SqliteRuntimeEffectController`, `SqliteEffectReplayOptions`),
//! same async signatures — so consumers swap backends with a path rename only.
//! The only mechanical change is the database layer: the prior store ran every op directly
//! on `&Connection` with `.await`; here every database body is a *synchronous*
//! rusqlite closure handed to the shared [`SqliteConnection`] wrapper. The
//! claim/finalize paths run inside `conn.write` (`BEGIN IMMEDIATE`) so the
//! cross-process write lock is taken up front — the lease fence WAL gives us.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CanonicalRuntimeEffectEnvelope,
    EffectHost, ExecutionScope, LeaseTimings, Resolution, ResolveOutcome, RuntimeAwaitEventOptions,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    ScopedEffectController, validate_replayed_effect_envelope,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::await_event::SqliteAwaitEvents;

const STATUS_IN_PROGRESS: &str = "in_progress";
const STATUS_COMPLETED: &str = "completed";
const STATUS_FAILED: &str = "failed";
const BUSY_POLL: Duration = Duration::from_millis(25);

static EFFECT_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Options for SQLite-backed runtime effect replay.
#[derive(Clone, Debug, Default)]
pub struct SqliteEffectReplayOptions {
    /// Effect-replay lease timing capability. Hosts share the same
    /// [`LeaseTimings`] they configure on the runtime so effect leases expire
    /// on the same failover window as session and process leases.
    pub lease_timings: LeaseTimings,
}

struct SqliteEffectReplayInner {
    conn: SqliteConnection,
    clock: Arc<dyn lash_core::Clock>,
    owner_id: String,
    lease_counter: AtomicU64,
    replay_mode: AtomicBool,
    lease_timings: LeaseTimings,
    await_events: SqliteAwaitEvents,
}

/// Deployment-level SQLite effect host.
///
/// This host persists runtime effect history in a local SQLite database and
/// returns scoped controllers that replay completed outcomes by
/// `(scope_id, replay_key)`.
#[derive(Clone)]
pub struct SqliteEffectHost {
    inner: Arc<SqliteEffectReplayInner>,
}

/// Scoped SQLite-backed runtime effect controller.
#[derive(Clone)]
pub struct SqliteRuntimeEffectController {
    inner: Arc<SqliteEffectReplayInner>,
    scope: ExecutionScope,
}

struct ClaimedEffect {
    scope_id: String,
    replay_key: String,
    envelope_hash: String,
    lease_token: String,
    due_at_ms: Option<u64>,
}

enum PreparedEffect {
    ReplayMismatch {
        recorded_envelope: Box<CanonicalRuntimeEffectEnvelope>,
        stored_envelope_hash: String,
    },
    ReplayOutcome {
        outcome: Box<RuntimeEffectOutcome>,
        due_at_ms: Option<u64>,
    },
    ReplayError(RuntimeEffectControllerError),
    Claimed(ClaimedEffect),
    Busy {
        retry_at_ms: u64,
    },
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
        Self::open_with_options_and_clock(path, options, Arc::new(lash_core::SystemClock)).await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        validate_effect_host_path(path)?;
        Ok(Self {
            inner: open_effect_replay_inner(path, StoreBacking::File, options, clock).await?,
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.replay_mode.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for SqliteEffectHost {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_events.key_for(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.await_events.resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.await_events.peek(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
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

impl EffectHost for SqliteEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        let controller = SqliteRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
        };
        ScopedEffectController::shared(Arc::new(controller), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        let controller = SqliteRuntimeEffectController {
            inner: Arc::clone(&self.inner),
            scope: scope.clone(),
        };
        Ok(Some(ScopedEffectController::shared(
            Arc::new(controller),
            scope,
        )?))
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
        Self::open_with_options_and_clock(path, scope, options, Arc::new(lash_core::SystemClock))
            .await
    }

    pub async fn open_with_options_and_clock(
        path: &Path,
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Ok(Self {
            inner: open_effect_replay_inner(path, StoreBacking::File, options, clock).await?,
            scope,
        })
    }

    pub async fn memory(scope: ExecutionScope) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options(scope, SqliteEffectReplayOptions::default()).await
    }

    pub async fn memory_with_clock(
        scope: ExecutionScope,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(scope, SqliteEffectReplayOptions::default(), clock)
            .await
    }

    pub async fn memory_with_options(
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
    ) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(scope, options, Arc::new(lash_core::SystemClock)).await
    }

    pub async fn memory_with_options_and_clock(
        scope: ExecutionScope,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Ok(Self {
            inner: open_effect_replay_memory_inner(options, clock).await?,
            scope,
        })
    }

    /// Force strict replay mode: missing effect history fails instead of
    /// executing locally. Normal operation still replays any completed row.
    pub fn start_replay(&self) {
        self.inner.replay_mode.store(true, Ordering::SeqCst);
    }

    async fn prepare_effect(
        &self,
        envelope: &RuntimeEffectEnvelope,
        reconstructed_envelope: &CanonicalRuntimeEffectEnvelope,
    ) -> Result<PreparedEffect, RuntimeEffectControllerError> {
        let replay_key = envelope
            .invocation
            .replay_key()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    "sqlite_effect_replay_key_missing",
                    "runtime effect envelope requires replay.key",
                )
            })?
            .to_string();
        let envelope_hash = reconstructed_envelope.hash().to_string();
        let envelope_json =
            serde_json::to_string(reconstructed_envelope).map_err(effect_encode_error)?;
        let scope_id = self.scope.id().to_string();
        let now = self.inner.clock.timestamp_ms();
        let lease_token = self.inner.next_lease_token();
        let due_at_ms = sleep_due_at_ms(envelope, now);
        let lease_ttl_ms = self.inner.lease_timings.ttl_ms();
        let lease_expires_at_ms = now.saturating_add(lease_ttl_ms);
        let replay_mode = self.inner.replay_mode.load(Ordering::SeqCst);
        let owner_id = self.inner.owner_id.clone();

        // The `BEGIN IMMEDIATE` transaction is run on the connection thread via
        // `conn.write`. The closure returns a `rusqlite::Result` carrying our
        // own outcome `Result<PreparedEffect, RuntimeEffectControllerError>`:
        // the SQL committing is independent of whether the recorded effect was
        // a success, a failure, or a fresh claim — exactly the prior behaviour
        // (commit on `Ok(_)` for any `PreparedEffect` variant). Only a real
        // SQLite error rolls back.
        let outcome: Result<PreparedEffect, RuntimeEffectControllerError> = self
            .inner
            .conn
            .write(move |tx| {
                let row = tx
                    .query_row(
                        "SELECT envelope_hash, envelope_json, status, outcome_json, error_json,
                                lease_owner_id, lease_token, lease_expires_at_ms, due_at_ms
                         FROM runtime_effect_replay
                         WHERE scope_id = ?1 AND replay_key = ?2",
                        params![scope_id.as_str(), replay_key.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, Option<String>>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, i64>(7)?,
                                row.get::<_, Option<i64>>(8)?,
                            ))
                        },
                    )
                    .optional()?;

                let Some((
                    existing_hash,
                    existing_envelope_json,
                    status,
                    outcome_json,
                    error_json,
                    lease_expires_row,
                    existing_due_row,
                )) = row
                else {
                    if replay_mode {
                        return Ok(Err(RuntimeEffectControllerError::new(
                            "sqlite_effect_replay_missing",
                            format!(
                                "no recorded runtime effect for scope `{scope_id}` and replay key `{replay_key}`"
                            ),
                        )));
                    }
                    let due_at_param = due_at_ms.map(|value| value as i64);
                    tx.execute(
                        "INSERT INTO runtime_effect_replay (
                            scope_id, replay_key, envelope_hash, envelope_json, status, outcome_json,
                            error_json, lease_owner_id, lease_token, lease_expires_at_ms,
                            due_at_ms, created_at_ms, updated_at_ms
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            scope_id.as_str(),
                            replay_key.as_str(),
                            envelope_hash.as_str(),
                            envelope_json.as_str(),
                            STATUS_IN_PROGRESS,
                            owner_id.as_str(),
                            lease_token.as_str(),
                            lease_expires_at_ms as i64,
                            due_at_param,
                            now as i64,
                            now as i64,
                        ],
                    )?;
                    return Ok(Ok(PreparedEffect::Claimed(ClaimedEffect {
                        scope_id,
                        replay_key,
                        envelope_hash,
                        lease_token,
                        due_at_ms,
                    })));
                };

                if existing_hash != envelope_hash {
                    let recorded_envelope: CanonicalRuntimeEffectEnvelope =
                        match serde_json::from_str(&existing_envelope_json) {
                            Ok(envelope) => envelope,
                            Err(err) => return Ok(Err(effect_decode_error(err))),
                        };
                    return Ok(Ok(PreparedEffect::ReplayMismatch {
                        recorded_envelope: Box::new(recorded_envelope),
                        stored_envelope_hash: existing_hash,
                    }));
                }

                let lease_expires_at_ms = lease_expires_row as u64;
                let existing_due_at_ms = existing_due_row.map(|value| value as u64);

                match status.as_str() {
                    STATUS_COMPLETED => {
                        let Some(json) = outcome_json else {
                            return Ok(Err(RuntimeEffectControllerError::new(
                                "sqlite_effect_replay_corrupt_row",
                                "completed runtime effect row is missing outcome_json",
                            )));
                        };
                        let outcome = match serde_json::from_str(&json) {
                            Ok(outcome) => outcome,
                            Err(err) => return Ok(Err(effect_decode_error(err))),
                        };
                        Ok(Ok(PreparedEffect::ReplayOutcome {
                            outcome: Box::new(outcome),
                            due_at_ms: existing_due_at_ms,
                        }))
                    }
                    STATUS_FAILED => {
                        let Some(json) = error_json else {
                            return Ok(Err(RuntimeEffectControllerError::new(
                                "sqlite_effect_replay_corrupt_row",
                                "failed runtime effect row is missing error_json",
                            )));
                        };
                        let err = match serde_json::from_str(&json) {
                            Ok(err) => err,
                            Err(err) => return Ok(Err(effect_decode_error(err))),
                        };
                        Ok(Ok(PreparedEffect::ReplayError(err)))
                    }
                    STATUS_IN_PROGRESS if lease_expires_at_ms > now => Ok(Ok(PreparedEffect::Busy {
                        retry_at_ms: lease_expires_at_ms,
                    })),
                    STATUS_IN_PROGRESS => {
                        let due_at_ms = existing_due_at_ms.or(due_at_ms);
                        let due_at_param = due_at_ms.map(|value| value as i64);
                        tx.execute(
                            "UPDATE runtime_effect_replay
                             SET lease_owner_id = ?3,
                                 lease_token = ?4,
                                 lease_expires_at_ms = ?5,
                                 due_at_ms = ?6,
                                 updated_at_ms = ?7
                             WHERE scope_id = ?1 AND replay_key = ?2",
                            params![
                                scope_id.as_str(),
                                replay_key.as_str(),
                                owner_id.as_str(),
                                lease_token.as_str(),
                                now.saturating_add(lease_ttl_ms) as i64,
                                due_at_param,
                                now as i64,
                            ],
                        )?;
                        Ok(Ok(PreparedEffect::Claimed(ClaimedEffect {
                            scope_id,
                            replay_key,
                            envelope_hash,
                            lease_token,
                            due_at_ms,
                        })))
                    }
                    other => Ok(Err(RuntimeEffectControllerError::new(
                        "sqlite_effect_replay_corrupt_row",
                        format!("unknown runtime effect replay status `{other}`"),
                    ))),
                }
            })
            .await
            .map_err(effect_sqlite_error)?;
        outcome
    }

    async fn finalize_effect(
        &self,
        claim: &ClaimedEffect,
        outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (status, outcome_json, error_json) = match outcome {
            Ok(outcome) => (
                STATUS_COMPLETED,
                Some(serde_json::to_string(outcome).map_err(effect_encode_error)?),
                None,
            ),
            Err(err) => (
                STATUS_FAILED,
                None,
                Some(serde_json::to_string(err).map_err(effect_encode_error)?),
            ),
        };
        let now = self.inner.clock.timestamp_ms();
        let scope_id = claim.scope_id.clone();
        let replay_key = claim.replay_key.clone();
        let envelope_hash = claim.envelope_hash.clone();
        let owner_id = self.inner.owner_id.clone();
        let lease_token = claim.lease_token.clone();
        let status = status.to_string();

        let result: Result<(), RuntimeEffectControllerError> = self
            .inner
            .conn
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
                        scope_id.as_str(),
                        replay_key.as_str(),
                        envelope_hash.as_str(),
                        owner_id.as_str(),
                        lease_token.as_str(),
                        status.as_str(),
                        outcome_json,
                        error_json,
                        now as i64,
                        now as i64,
                    ],
                )?;
                if changed != 1 {
                    return Ok(Err(RuntimeEffectControllerError::new(
                        "sqlite_effect_replay_lease_lost",
                        format!(
                            "runtime effect replay lease was lost before finalizing scope `{scope_id}` replay key `{replay_key}`"
                        ),
                    )));
                }
                Ok(Ok(()))
            })
            .await
            .map_err(effect_sqlite_error)?;
        result
    }

    async fn renew_effect_lease(
        &self,
        claim: &ClaimedEffect,
    ) -> Result<(), RuntimeEffectControllerError> {
        let now = self.inner.clock.timestamp_ms();
        let renewed_expires_at = now.saturating_add(self.inner.lease_timings.ttl_ms());
        let scope_id = claim.scope_id.clone();
        let replay_key = claim.replay_key.clone();
        let envelope_hash = claim.envelope_hash.clone();
        let owner_id = self.inner.owner_id.clone();
        let lease_token = claim.lease_token.clone();

        let result: Result<(), RuntimeEffectControllerError> = self
            .inner
            .conn
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
                        scope_id.as_str(),
                        replay_key.as_str(),
                        envelope_hash.as_str(),
                        owner_id.as_str(),
                        lease_token.as_str(),
                        renewed_expires_at as i64,
                        now as i64,
                        now as i64,
                    ],
                )?;
                if changed != 1 {
                    return Ok(Err(RuntimeEffectControllerError::new(
                        "sqlite_effect_replay_lease_lost",
                        format!(
                            "runtime effect replay lease was lost while executing scope `{scope_id}` replay key `{replay_key}`"
                        ),
                    )));
                }
                Ok(Ok(()))
            })
            .await
            .map_err(effect_sqlite_error)?;
        result
    }

    async fn execute_claimed_effect_with_renewal(
        &self,
        claim: &ClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let renew_every = self.inner.lease_timings.renew_interval();
        let effect = self.execute_claimed_effect(claim, envelope, local_executor);
        tokio::pin!(effect);

        loop {
            tokio::select! {
                result = &mut effect => return result,
                _ = self.inner.clock.sleep(renew_every) => {
                    self.renew_effect_lease(claim).await?;
                }
            }
        }
    }

    async fn execute_claimed_effect(
        &self,
        claim: &ClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if matches!(envelope.command, RuntimeEffectCommand::Sleep { .. }) {
            sleep_until_due(self.inner.clock.as_ref(), claim.due_at_ms).await;
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
                let RuntimeAwaitEventOptions {
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
impl AwaitEventResolver for SqliteRuntimeEffectController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_events.key_for(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.await_events.resolve(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.await_events.peek(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
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
impl RuntimeEffectController for SqliteRuntimeEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let reconstructed_envelope = envelope.canonical_form()?;
        let replay_trace = local_executor.replay_validation_trace().cloned();
        loop {
            match self
                .prepare_effect(&envelope, &reconstructed_envelope)
                .await?
            {
                PreparedEffect::ReplayMismatch {
                    recorded_envelope,
                    stored_envelope_hash,
                } => {
                    validate_replayed_effect_envelope(
                        recorded_envelope.as_ref(),
                        &reconstructed_envelope,
                        "sqlite_effect_replay_hash_conflict",
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
                PreparedEffect::ReplayOutcome { outcome, due_at_ms } => {
                    sleep_until_due(self.inner.clock.as_ref(), due_at_ms).await;
                    return Ok(*outcome);
                }
                PreparedEffect::ReplayError(err) => return Err(err),
                PreparedEffect::Claimed(claim) => {
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
                PreparedEffect::Busy { retry_at_ms } => {
                    sleep_until_retry(self.inner.clock.as_ref(), retry_at_ms).await;
                }
            }
        }
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

async fn open_effect_replay_inner(
    path: &Path,
    backing: StoreBacking,
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplayInner>> {
    let conn = SqliteConnection::open(path).await?;
    let signing_secret = ensure_effect_schema(&conn).await?;
    apply_pragmas(&conn, backing).await?;
    Ok(Arc::new(SqliteEffectReplayInner::new(
        conn,
        options,
        clock,
        signing_secret,
    )))
}

async fn open_effect_replay_memory_inner(
    options: SqliteEffectReplayOptions,
    clock: Arc<dyn lash_core::Clock>,
) -> tokio_rusqlite::Result<Arc<SqliteEffectReplayInner>> {
    let conn = SqliteConnection::open_in_memory().await?;
    let signing_secret = ensure_effect_schema(&conn).await?;
    apply_pragmas(&conn, StoreBacking::Memory).await?;
    Ok(Arc::new(SqliteEffectReplayInner::new(
        conn,
        options,
        clock,
        signing_secret,
    )))
}

impl SqliteEffectReplayInner {
    fn new(
        conn: SqliteConnection,
        options: SqliteEffectReplayOptions,
        clock: Arc<dyn lash_core::Clock>,
        signing_secret: Vec<u8>,
    ) -> Self {
        let sequence = EFFECT_OWNER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let timestamp_ms = clock.timestamp_ms();
        let await_events = SqliteAwaitEvents::new(conn.clone(), signing_secret, Arc::clone(&clock));
        Self {
            conn,
            clock,
            owner_id: format!("pid{}-{sequence}-{}", std::process::id(), timestamp_ms),
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

fn sleep_due_at_ms(envelope: &RuntimeEffectEnvelope, now: u64) -> Option<u64> {
    match envelope.command {
        RuntimeEffectCommand::Sleep { duration_ms } => Some(now.saturating_add(duration_ms)),
        _ => None,
    }
}

async fn sleep_until_due(clock: &dyn lash_core::Clock, due_at_ms: Option<u64>) {
    let Some(due_at_ms) = due_at_ms else {
        return;
    };
    let now = clock.timestamp_ms();
    if due_at_ms > now {
        clock.sleep(Duration::from_millis(due_at_ms - now)).await;
    }
}

async fn sleep_until_retry(clock: &dyn lash_core::Clock, retry_at_ms: u64) {
    let now = clock.timestamp_ms();
    let delay = if retry_at_ms > now {
        Duration::from_millis(retry_at_ms - now).min(BUSY_POLL)
    } else {
        BUSY_POLL
    };
    clock.sleep(delay).await;
}

fn effect_sqlite_error(err: rusqlite::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new("sqlite_effect_replay_store", err.to_string())
}

fn effect_encode_error(err: serde_json::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        "sqlite_effect_replay_encode",
        format!("failed to encode runtime effect replay row: {err}"),
    )
}

fn effect_decode_error(err: serde_json::Error) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        "sqlite_effect_replay_decode",
        format!("failed to decode runtime effect replay row: {err}"),
    )
}
