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
    AttachmentOwnerKind, AwaitEventResolver, BlobRef, CanonicalRuntimeEffectEnvelope,
    DeliveryPolicy, EffectHost, ExecutionScope, GcReport, LeaseOwnerIdentity, LeaseOwnerLiveness,
    MergeKey, NodeRefcountVerification, PersistedSegmentHandover, ProcessAwaitOutput,
    ProcessChangeCursor, ProcessEvent, ProcessEventAppendRequest, ProcessEventAppendResult,
    ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessHandleDescriptor,
    ProcessHandleGrant, ProcessLease, ProcessLeaseCompletion, ProcessLiveReferenceSummary,
    ProcessPruneReport, ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome,
    ProcessStartPlan, ProcessStarted, QueuedWorkStore, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimePersistence,
    ScopedEffectController, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseCompletion, SessionExecutionLeaseFence,
    SessionExecutionLeaseStore, SessionMeta, SessionNodeRecord, SessionReadScope, SessionScope,
    SessionStoreCreateRequest, SessionStoreFactory, SlotPolicy, StoreError, StoreMaintenance,
    TokenLedgerEntry, TurnInputStore, VacuumReport, validate_replayed_effect_envelope,
};
use lash_core::{
    PluginError, TriggerDeliveryReservation, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
    TriggerStore, TriggerSubscriptionFilter, TriggerSubscriptionRecord,
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
//
// Bumped to 15 for FIG-546 owner-bound attachment intents, following the
// independently assigned version 14 trigger schema. Pre-15 manifests
// cannot prove turn/process owner liveness and are rejected rather than read
// through a compatibility path.
//
// Bumped to 16 for FIG-562 durable AwaitEvent promises. The effect schema now
// owns authenticated promise rows, a store-resident HMAC secret, and durable
// session-revocation tombstones. Pre-16 databases are rejected and recreated.
//
// Bumped to 17 for FIG-579 canonical runtime-effect envelope persistence.
// Pre-17 databases cannot produce structural replay mismatch diagnostics and
// are rejected and recreated.
//
// Bumped to 18 for the second completion-authority payload cutover (ADR 0027):
// `ExternalOwner` no longer carries the unverified `granted_to` field, changing
// the terminal event's replay-key payload hash. Pre-18 databases are rejected
// and recreated so retries cannot compare terminal events across payload formats.
//
// Bumped to 19 for structural FrameOpen history and explicit graph parent edges.
// Bumped to 20 for transactional node refcounts and destructive-zero confirmation.
// Both are reject-and-recreate boundaries; older graph rows cannot satisfy the
// structural history and reclamation invariants.
//
// Bumped to 21 for first-class forks and continuation pins. `lash_node_anchors`
// joins live heads as both a graph-node root and checkpoint-blob root.
const SCHEMA_VERSION: i32 = 21;
const PROCESS_LEASE_SCHEMA_VERSION: u32 = lash_core::PROCESS_LEASE_SCHEMA_VERSION;

#[derive(Clone)]
pub struct PostgresStorage {
    pool: PgPool,
    await_event_signing_secret: Arc<[u8]>,
}

#[derive(Clone)]
pub struct PostgresSessionStoreFactory {
    pool: PgPool,
    process_registry_shared: bool,
    clock: Arc<dyn lash_core::Clock>,
}

#[derive(Clone)]
pub struct PostgresSessionStore {
    pool: PgPool,
    clock: Arc<dyn lash_core::Clock>,
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
/// History commits and deletion share a session-keyed transaction lock, then
/// existing-session commits lock and verify the head revision before changing
/// reachability counts. The final conditional write remains a stale-writer
/// fence. `lock_timeout` caps lock waits before surfacing retryable contention.
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
        let await_event_signing_secret = ensure_schema(&pool).await?;
        Ok(Self {
            pool,
            await_event_signing_secret: await_event_signing_secret.into(),
        })
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
        let await_event_signing_secret = ensure_schema(&pool).await?;
        Ok(Self {
            pool,
            await_event_signing_secret: await_event_signing_secret.into(),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn session_store_factory(&self) -> PostgresSessionStoreFactory {
        warn_postgres_process_registry_not_wired();
        PostgresSessionStoreFactory {
            pool: self.pool.clone(),
            process_registry_shared: false,
            clock: Arc::new(lash_core::SystemClock),
        }
    }

    /// Construct a session factory that explicitly declares this storage's
    /// Lash process registry shares the same PostgreSQL database.
    pub fn session_store_factory_with_shared_process_registry(
        &self,
    ) -> PostgresSessionStoreFactory {
        PostgresSessionStoreFactory {
            pool: self.pool.clone(),
            process_registry_shared: true,
            clock: Arc::new(lash_core::SystemClock),
        }
    }

    pub fn session_store(&self, session_id: impl Into<String>) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::new(lash_core::SystemClock),
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
            clock: Arc::new(lash_core::SystemClock),
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

    pub fn new_with_shared_process_registry(storage: &PostgresStorage) -> Self {
        storage.session_store_factory_with_shared_process_registry()
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

fn warn_postgres_process_registry_not_wired() {
    tracing::warn!(
        "PostgreSQL attachment GC process-owner liveness is not wired; process-owned intents will be retained indefinitely. Call PostgresStorage::session_store_factory_with_shared_process_registry()."
    );
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

mod await_event;

#[path = "postgres/artifact_store.rs"]
mod artifact_store;
#[path = "postgres/attachments.rs"]
mod attachments;
#[path = "postgres/effect_replay.rs"]
mod effect_replay;
#[path = "postgres/process_helpers.rs"]
mod process_helpers;
#[path = "postgres/process_registry.rs"]
mod process_registry;
#[path = "postgres/runtime_persistence.rs"]
mod runtime_persistence;
#[path = "postgres/schema.rs"]
mod schema;
#[path = "postgres/session_factory.rs"]
mod session_factory;
#[path = "postgres/support.rs"]
mod support;
#[path = "postgres/trigger_store.rs"]
mod trigger_store;

pub use effect_replay::{
    PostgresEffectHost, PostgresEffectReplayOptions, PostgresRuntimeEffectController,
};
use {process_helpers::*, runtime_persistence::*, schema::*, session_factory::*, support::*};

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod postgres_test_support;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    struct WarningCounter(Arc<AtomicUsize>);

    impl<S> Layer<S> for WarningCounter
    where
        S: tracing::Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn unwired_process_registry_factory_warns() {
        let warning_count = Arc::new(AtomicUsize::new(0));
        let subscriber = Registry::default().with(WarningCounter(Arc::clone(&warning_count)));

        tracing::subscriber::with_default(subscriber, warn_postgres_process_registry_not_wired);

        assert_eq!(
            warning_count.load(Ordering::Relaxed),
            1,
            "the legacy factory must warn that process-owner liveness is not wired"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_claim_completion_is_locked_and_zero_rows_roll_back_the_head() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres claim-completion fence: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect claim-completion fence storage");
        let session_id = format!("postgres-claim-fence:{}", uuid::Uuid::new_v4());
        let input_id = format!("input:{}", uuid::Uuid::new_v4());
        let stale = lash_core::TurnInputCompletion {
            session_id: session_id.clone(),
            claim_id: "claim-a".to_string(),
            lease_token: "token-a".to_string(),
            input_ids: vec![input_id.clone()],
            applications: Vec::new(),
        };
        sqlx::query(
            "INSERT INTO lash_sessions (session_id, head_revision, head_json)
             VALUES ($1, 7, '{}')",
        )
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("insert claim-fence session head");
        sqlx::query(
            "INSERT INTO lash_pending_turn_inputs (
                input_id, session_id, ingress_json, state, input_json, enqueued_at_ms,
                claim_id, claim_token, claim_fencing_token, claim_session_lease_generation
             )
             VALUES ($1, $2, '{}', $3, '{}', 1, $4, $5, 1, 1)",
        )
        .bind(&input_id)
        .bind(&session_id)
        .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
        .bind(&stale.claim_id)
        .bind(&stale.lease_token)
        .execute(storage.pool())
        .await
        .expect("insert claimed turn input");

        // Ownership validation locks the exact claim row. A superseder cannot
        // rewrite it between validation and completion.
        let mut validating = storage.pool().begin().await.expect("begin validating tx");
        ensure_turn_input_completion_tx(&mut validating, &stale)
            .await
            .expect("validate and lock stale claim");
        let mut blocked_superseder = storage.pool().begin().await.expect("begin superseder tx");
        sqlx::query("SET LOCAL lock_timeout = '50ms'")
            .execute(&mut *blocked_superseder)
            .await
            .expect("bound superseder lock wait");
        let blocked = sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET claim_id = 'claim-b', claim_token = 'token-b',
                 claim_fencing_token = 2, claim_session_lease_generation = 2
             WHERE session_id = $1 AND input_id = $2",
        )
        .bind(&session_id)
        .bind(&input_id)
        .execute(&mut *blocked_superseder)
        .await
        .expect_err("ownership row lock must block supersession");
        assert_eq!(
            blocked.as_database_error().and_then(|error| error.code()),
            Some(std::borrow::Cow::Borrowed("55P03")),
            "supersession must fail specifically on the held row lock: {blocked}"
        );
        validating
            .rollback()
            .await
            .expect("release validation lock");
        blocked_superseder
            .rollback()
            .await
            .expect("roll back timed-out superseder");

        // Reproduce the old TOCTOU transaction shape: A observed ownership
        // without locking, B committed a fresh generation+token, then A moved
        // the head before attempting token-qualified completion. The checked
        // zero-row completion must abort A's whole transaction.
        let mut stale_committer = storage.pool().begin().await.expect("begin stale tx");
        let observed: Option<i64> = sqlx::query_scalar(
            "SELECT 1::BIGINT FROM lash_pending_turn_inputs
             WHERE session_id = $1 AND input_id = $2 AND claim_id = $3 AND claim_token = $4",
        )
        .bind(&session_id)
        .bind(&input_id)
        .bind(&stale.claim_id)
        .bind(&stale.lease_token)
        .fetch_optional(&mut *stale_committer)
        .await
        .expect("old non-locking ownership validation");
        assert_eq!(observed, Some(1));

        let mut superseder = storage
            .pool()
            .begin()
            .await
            .expect("begin fresh superseder");
        sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET claim_id = 'claim-b', claim_token = 'token-b',
                 claim_fencing_token = 2, claim_session_lease_generation = 2
             WHERE session_id = $1 AND input_id = $2",
        )
        .bind(&session_id)
        .bind(&input_id)
        .execute(&mut *superseder)
        .await
        .expect("supersede stale claim");
        superseder
            .commit()
            .await
            .expect("commit fresh supersession");

        sqlx::query(
            "UPDATE lash_sessions SET head_revision = head_revision + 1 WHERE session_id = $1",
        )
        .bind(&session_id)
        .execute(&mut *stale_committer)
        .await
        .expect("tentatively move stale head");
        let error =
            complete_turn_input_claims_tx(&mut stale_committer, std::slice::from_ref(&stale))
                .await
                .expect_err("zero-row stale completion must trip the atomic fence");
        assert!(matches!(
            error,
            StoreError::TurnInputClaimSuperseded {
                ref session_id,
                ref claim_id
            } if session_id == &stale.session_id && claim_id == &stale.claim_id
        ));
        stale_committer
            .rollback()
            .await
            .expect("roll back stale head movement");

        let head_revision: i64 =
            sqlx::query_scalar("SELECT head_revision FROM lash_sessions WHERE session_id = $1")
                .bind(&session_id)
                .fetch_one(storage.pool())
                .await
                .expect("read head after rejected stale commit");
        assert_eq!(
            head_revision, 7,
            "rejected stale completion must not move the session head"
        );
        let current_claim: (String, String, i64) = sqlx::query_as(
            "SELECT claim_id, claim_token, claim_session_lease_generation
             FROM lash_pending_turn_inputs WHERE session_id = $1 AND input_id = $2",
        )
        .bind(&session_id)
        .bind(&input_id)
        .fetch_one(storage.pool())
        .await
        .expect("read winning claim");
        assert_eq!(
            current_claim,
            ("claim-b".to_string(), "token-b".to_string(), 2)
        );

        sqlx::query("DELETE FROM lash_pending_turn_inputs WHERE session_id = $1")
            .bind(&session_id)
            .execute(storage.pool())
            .await
            .expect("clean claim-fence input");
        sqlx::query("DELETE FROM lash_sessions WHERE session_id = $1")
            .bind(&session_id)
            .execute(storage.pool())
            .await
            .expect("clean claim-fence head");
    }

    #[tokio::test]
    async fn postgres_zero_confirmation_aborts_a_corrupt_low_count_transaction() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres refcount drift proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect refcount drift storage");
        let session_id = format!("postgres-refcount-drift:{}", uuid::Uuid::new_v4());
        let store = storage.session_store(session_id.clone());
        let mut state = lash_core::RuntimeSessionState {
            session_id: session_id.clone(),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("commit root frame");
        let frame_node_id = state.current_frame_node_id.clone().expect("frame node");
        sqlx::query("UPDATE lash_graph_nodes SET incoming_refs = 0 WHERE node_id = $1")
            .bind(&frame_node_id)
            .execute(storage.pool())
            .await
            .expect("corrupt cached count");
        let child_node_id = format!("postgres-refcount-child:{}", uuid::Uuid::new_v4());
        let child = lash_core::SessionNodeRecord {
            node_id: child_node_id.clone(),
            parent_node_id: Some(frame_node_id.clone()),
            timestamp: "2026-07-27T00:00:00Z".to_string(),
            payload: lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(
                    lash_core::ProtocolEvent::typed("refcount-drift", serde_json::Value::Null)
                        .expect("protocol event"),
                ),
            },
        };
        let mut commit = RuntimeCommit::persisted_state(&state, &[]);
        commit.expected_head_revision = 1;
        commit.graph = GraphCommitDelta::Append {
            nodes: vec![child],
            leaf_node_id: Some(child_node_id.clone()),
        };

        let error = store
            .commit_runtime_state(commit)
            .await
            .expect_err("zero-confirmation must abort");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                ref node_id,
                cached: 0,
                derived: 1,
            } if node_id == &frame_node_id
        ));
        let revision: i64 =
            sqlx::query_scalar("SELECT head_revision FROM lash_sessions WHERE session_id = $1")
                .bind(&session_id)
                .fetch_one(storage.pool())
                .await
                .expect("load unchanged head");
        assert_eq!(revision, 1);
        let child_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM lash_graph_nodes WHERE node_id = $1)")
                .bind(&child_node_id)
                .fetch_one(storage.pool())
                .await
                .expect("check rolled-back child");
        assert!(!child_exists);
    }

    #[tokio::test]
    async fn postgres_refcount_scrub_detects_corrupt_cached_count() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres refcount scrub proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect refcount scrub storage");
        let session_id = format!("postgres-refcount-scrub:{}", uuid::Uuid::new_v4());
        let store = storage.session_store(session_id.clone());
        let mut state = lash_core::RuntimeSessionState {
            session_id,
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();
        store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("commit root frame");
        let frame_node_id = state.current_frame_node_id.expect("frame node");
        sqlx::query("UPDATE lash_graph_nodes SET incoming_refs = 2 WHERE node_id = $1")
            .bind(&frame_node_id)
            .execute(storage.pool())
            .await
            .expect("corrupt cached count");

        let error = store
            .verify_node_refcounts()
            .await
            .expect_err("scrub must detect cached count drift");

        assert!(matches!(
            error,
            StoreError::NodeRefcountDrift {
                node_id,
                cached: 2,
                derived: 1,
            } if node_id == frame_node_id
        ));
    }

    #[tokio::test]
    async fn postgres_delete_fences_a_stale_first_commit_until_explicit_recreate() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres delete fence proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect delete fence storage");
        let factory = storage.session_store_factory_with_shared_process_registry();
        let session_id = format!("postgres-delete-fence:{}", uuid::Uuid::new_v4());
        let request = SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: Default::default(),
        };
        let stale_store = factory
            .create_store(&request)
            .await
            .expect("create stale store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: session_id.clone(),
            ..Default::default()
        };
        state.ensure_agent_frame_initialized();

        factory
            .delete_session(&session_id)
            .await
            .expect("delete before first commit");
        let error = stale_store
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect_err("stale first commit must not resurrect the session");
        assert!(matches!(
            error,
            StoreError::SessionDeleted {
                ref session_id
            } if session_id == &request.session_id
        ));

        let recreated = factory
            .create_store(&request)
            .await
            .expect("explicitly recreate deleted store");
        recreated
            .commit_runtime_state(RuntimeCommit::persisted_state(&state, &[]))
            .await
            .expect("recreated store accepts first commit");
    }

    #[tokio::test]
    async fn checkpoint_probe_skips_writes_for_deferred_head_when_configured() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres checkpoint counter: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
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
