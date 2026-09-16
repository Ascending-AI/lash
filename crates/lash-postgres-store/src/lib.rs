//! PostgreSQL durable storage for Lash.
//!
//! One [`PostgresStorage`] owns a shared [`sqlx::PgPool`] and creates durable
//! implementations for the runtime session store, process registry, trigger
//! store, Lashlang artifact store, process execution environment store, and
//! attachment manifest.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lash_core::runtime::{
    ProcessHandleGrantEntry, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkCompletion, QueuedWorkItem,
};
use lash_core::store::queued_work::{
    ClaimCandidate, QueuedWorkClaimLease, claim_scan_limit, derive_batch_id,
    select_leading_session_command, select_turn_work_claim_prefix,
};
use lash_core::store::{
    GraphCommitDelta, HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit,
    RuntimeCommitResult, SessionCheckpoint, SessionHeadMeta,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AwaitEventResolver, BlobRef, DeliveryPolicy, DurabilityTier, EffectHost, ExecutionScope,
    GcReport, LeaseOwnerIdentity, LeaseOwnerLiveness, MergeKey, PersistedSegmentHandover,
    ProcessAwaitOutput, ProcessChangeCursor, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessExternalRef, ProcessHandleDescriptor, ProcessHandleGrant,
    ProcessLease, ProcessLeaseCompletion, ProcessLiveReferenceSummary, ProcessPruneReport,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStarted, QueuedWorkStore,
    RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    RuntimePersistence, ScopedEffectController, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion, SessionExecutionLeaseFence,
    SessionExecutionLeaseStore, SessionMeta, SessionNodeRecord, SessionReadScope, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy, StoreError, StoreMaintenance,
    TokenLedgerEntry, TurnInputStore, VacuumReport,
};
use lash_core::{
    PluginError, TriggerDeliveryReservation, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
    TriggerStore, TriggerSubscriptionDraft, TriggerSubscriptionFilter, TriggerSubscriptionRecord,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Executor, Row};

const SCHEMA_COMPONENT: &str = "lash-postgres-store";
// Bumped to 9: ADR 0020 process-row `change_seq` now uses a transactional
// clock row instead of a sequence. The schema is a reject-and-recreate
// boundary; pre-9 databases are rejected at open rather than migrated.
//
// Bumped to 11 for the combined completion-authority (ADR 0027) and attachment
// three-layer (ADR 0028) cutovers. This single component gates every table,
// including `lash_process_events` and `lash_attachment_manifest`. Pre-cutover
// terminal process events lack the `completion_authority` payload (so a
// cross-version replay-key hash would mismatch), and pre-cutover manifest rows
// name `sessions/<hash>/...` blob paths the flat layout cannot read. Rejecting
// and recreating pre-11 databases removes both hazards; the old `sessions/` blob
// prefix is unreachable garbage operators delete manually.
//
// Bumped to 12 for claim generation fencing (ADR 0029): `lash_queued_work_batches`
// and `lash_pending_turn_inputs` replace their per-claim `claim_claimed_at_ms` /
// `claim_expires_at_ms` columns with a single `claim_session_lease_generation`
// pinning the session-execution-lease generation the claim was taken under. This
// is a reject-and-recreate boundary; pre-12 databases are rejected at open.
const SCHEMA_VERSION: i32 = 13;
const PROCESS_LEASE_SCHEMA_VERSION: u32 = lash_core::PROCESS_LEASE_SCHEMA_VERSION;

#[derive(Clone)]
pub struct PostgresStorage {
    pool: PgPool,
}

#[derive(Clone)]
pub struct PostgresSessionStoreFactory {
    pool: PgPool,
}

#[derive(Clone)]
pub struct PostgresSessionStore {
    pool: PgPool,
    /// Explicit session binding for handles created via the factory.
    session_id: Option<String>,
    /// In-memory bind-on-first-commit for an *unbound* handle. A session-store
    /// handle commits to exactly one session; an unbound handle latches the first
    /// session it commits and rejects others (Postgres is multi-session per
    /// database, so this can't be inferred from a singleton head row the way the
    /// single-file SQLite store does). Shared across clones via `Arc`.
    bound_session: Arc<OnceLock<String>>,
    #[cfg(test)]
    checkpoint_probe_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    checkpoint_write_transaction_count: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
pub struct PostgresProcessRegistry {
    pool: PgPool,
}

#[derive(Clone)]
pub struct PostgresTriggerStore {
    pool: PgPool,
}

#[derive(Clone)]
pub struct PostgresLashlangArtifactStore {
    pool: PgPool,
}

/// Connection-pool and per-connection timeout knobs for [`PostgresStorage`].
///
/// Mutating session work first claims the durable session execution lease.
/// Session commits then verify that lease fence and use **optimistic CAS** on
/// the head (`UPDATE … WHERE head_revision = expected`) as a stale-writer
/// backstop, not as the normal cross-worker concurrency primitive.
/// `lock_timeout` is defense in depth: it caps how long the fenced CAS write
/// may wait on the head row's lock before erroring (surfaced as a retryable
/// conflict), so a pathological burst can never starve the pool.
#[derive(Clone, Debug)]
pub struct PostgresStoreConfig {
    /// Maximum pooled connections. Default 16.
    pub max_connections: u32,
    /// Minimum idle connections kept warm. Default 0.
    pub min_connections: u32,
    /// How long `acquire` waits for a free connection before erroring. Default 30s.
    pub acquire_timeout: Duration,
    /// Close a connection after this idle period. Default 10m.
    pub idle_timeout: Option<Duration>,
    /// Recycle a connection after this lifetime. Default 30m.
    pub max_lifetime: Option<Duration>,
    /// Postgres `lock_timeout` applied to every connection. Default 10s.
    pub lock_timeout: Option<Duration>,
    /// Postgres `statement_timeout` applied to every connection. Default 30s — a
    /// backstop so a wedged query can never hold a connection indefinitely.
    pub statement_timeout: Option<Duration>,
}

impl Default for PostgresStoreConfig {
    fn default() -> Self {
        Self {
            max_connections: 16,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
            lock_timeout: Some(Duration::from_secs(10)),
            statement_timeout: Some(Duration::from_secs(30)),
        }
    }
}

impl PostgresStorage {
    /// Connect with [`PostgresStoreConfig::default`] pool/timeout settings.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        Self::connect_with(database_url, PostgresStoreConfig::default()).await
    }

    /// Connect with explicit pool sizing and per-connection timeouts.
    pub async fn connect_with(
        database_url: &str,
        config: PostgresStoreConfig,
    ) -> Result<Self, StoreError> {
        let lock_ms = config.lock_timeout.map(|d| d.as_millis().max(1) as u64);
        let statement_ms = config
            .statement_timeout
            .map(|d| d.as_millis().max(1) as u64);
        let mut options = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout);
        if let Some(timeout) = config.idle_timeout {
            options = options.idle_timeout(timeout);
        }
        if let Some(timeout) = config.max_lifetime {
            options = options.max_lifetime(timeout);
        }
        let pool = options
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    if let Some(ms) = lock_ms {
                        conn.execute(format!("SET lock_timeout = {ms}").as_str())
                            .await?;
                    }
                    if let Some(ms) = statement_ms {
                        conn.execute(format!("SET statement_timeout = {ms}").as_str())
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(database_url)
            .await
            .map_err(store_sqlx_error)?;
        ensure_schema(&pool).await?;
        Ok(Self { pool })
    }

    /// Build storage over an already-constructed pool.
    ///
    /// This runs the same [`ensure_schema`] gate `connect`/`connect_with` do, so
    /// every public construction path enforces the component schema version: a
    /// pre-cutover (e.g. version-10) database is rejected loudly with the same
    /// mismatch error rather than silently used, which would resurrect the
    /// cross-version hazards the version bump exists to prevent. The
    /// `CREATE TABLE IF NOT EXISTS` statements are idempotent, so running the gate
    /// against an already-provisioned pool is safe.
    pub async fn from_pool(pool: PgPool) -> Result<Self, StoreError> {
        ensure_schema(&pool).await?;
        Ok(Self { pool })
    }

    /// Open a host-provisioned schema without DDL or seed writes.
    ///
    /// Only the component version is checked at runtime. Run [`Self::verify_schema`]
    /// after host migrations and before rolling out the application to check all
    /// required relations, columns, and the process-change clock seed.
    pub async fn from_provisioned_pool(pool: PgPool) -> Result<Self, StoreError> {
        let mut connection = pool.acquire().await.map_err(store_sqlx_error)?;
        verify_schema_version(&mut connection).await?;
        drop(connection);
        Ok(Self { pool })
    }

    /// Check a host-provisioned schema in a read-only transaction.
    ///
    /// Checks the component version, required relations and columns, and the
    /// process-change clock seed. It does not compare column types, indexes, or
    /// constraints; host migration and conformance tests own those checks.
    pub async fn verify_schema(pool: &PgPool) -> Result<(), StoreError> {
        verify_provisioned_schema(pool).await
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn session_store_factory(&self) -> PostgresSessionStoreFactory {
        PostgresSessionStoreFactory {
            pool: self.pool.clone(),
        }
    }

    pub fn session_store(&self, session_id: impl Into<String>) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            session_id: Some(session_id.into()),
            bound_session: Arc::new(OnceLock::new()),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn unbound_session_store(&self) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            session_id: None,
            bound_session: Arc::new(OnceLock::new()),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn process_registry(&self) -> PostgresProcessRegistry {
        PostgresProcessRegistry {
            pool: self.pool.clone(),
        }
    }

    pub fn trigger_store(&self) -> PostgresTriggerStore {
        PostgresTriggerStore {
            pool: self.pool.clone(),
        }
    }

    pub fn lashlang_artifact_store(&self) -> PostgresLashlangArtifactStore {
        PostgresLashlangArtifactStore {
            pool: self.pool.clone(),
        }
    }

    pub fn process_env_store(&self) -> PostgresLashlangArtifactStore {
        PostgresLashlangArtifactStore {
            pool: self.pool.clone(),
        }
    }

    pub fn effect_host(&self) -> PostgresEffectHost {
        PostgresEffectHost::new(self)
    }

    pub fn runtime_effect_controller(
        &self,
        scope: ExecutionScope,
    ) -> PostgresRuntimeEffectController {
        PostgresRuntimeEffectController::new(self, scope)
    }
}

impl PostgresSessionStoreFactory {
    pub fn new(storage: &PostgresStorage) -> Self {
        storage.session_store_factory()
    }
}

impl PostgresSessionStore {
    pub fn unbound(storage: &PostgresStorage) -> Self {
        storage.unbound_session_store()
    }

    #[cfg(test)]
    fn checkpoint_claim_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    async fn selected_session_id(&self) -> Result<Option<String>, StoreError> {
        if let Some(session_id) = &self.session_id {
            return Ok(Some(session_id.clone()));
        }
        sqlx::query_scalar("SELECT session_id FROM lash_sessions ORDER BY session_id ASC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(store_sqlx_error)
    }
}

include!("postgres/schema.rs");
include!("postgres/schema_check.rs");
include!("postgres/support.rs");
include!("postgres/attachments.rs");
include!("postgres/effect_replay.rs");
include!("postgres/session_factory.rs");
include!("postgres/runtime_persistence.rs");
include!("postgres/process_helpers.rs");
include!("postgres/process_registry.rs");
include!("postgres/trigger_store.rs");
include!("postgres/artifact_store.rs");

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_probe_skips_writes_for_deferred_head_when_configured() {
        let Ok(database_url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
            eprintln!("skipping Postgres checkpoint counter: database URL is not set");
            return;
        };
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect checkpoint counter storage");
        let session_id = format!("postgres-checkpoint-counter:{}", std::process::id());
        let store = Arc::new(storage.session_store(&session_id));
        lash_core::testing::conformance::checkpoint_claim_probe_transaction_counts(
            Arc::clone(&store) as Arc<dyn RuntimePersistence>,
            &session_id,
            || store.checkpoint_claim_counts(),
        )
        .await;
        storage
            .session_store_factory()
            .delete_session(&session_id)
            .await
            .expect("delete checkpoint counter session");
    }
}
