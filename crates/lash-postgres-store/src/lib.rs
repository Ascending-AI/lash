//! PostgreSQL durable storage for Lash.
//!
//! One [`PostgresStorage`] owns a shared [`sqlx::PgPool`] and creates durable
//! implementations for the runtime session store, process registry, trigger
//! store, Lashlang artifact store, process execution environment store, and
//! attachment manifest.
//!
//! # Who provisions the schema
//!
//! By default lash applies its own DDL at open, which needs `CREATE` on the
//! target schema. A host that owns its migrations instead vendors
//! [`PostgresStorage::schema_ddl`] — the same bytes are committed as this crate's
//! `schema.sql` — into its own tooling and opens with
//! [`SchemaProvisioning::HostProvisioned`], which runs no DDL at all. Copy those
//! bytes; never transcribe them.
//!
//! Either way, open ends by reading the live catalog and comparing it against the
//! shape this build requires, so a database whose version stamp is right but whose
//! tables are not is rejected at open with a per-object diff rather than failing at
//! the first query — or silently losing a guard, which is what a dropped unique
//! index or a dropped cascade does. [`SchemaCheck`] controls whether a structural
//! mismatch is fatal. A component-version mismatch is fatal unless Lash-managed
//! `Enforce` mode carries an explicit migration from the exact published source
//! shape; no [`SchemaCheck`] relaxes the remaining boundary.
//! [`PostgresStorage::verify_schema_for`] exposes the same check against a bare
//! pool so a host can gate its own migration CI on it. See ADR 0052.
//!
//! Do not run schema migrations concurrently with an open or a verification:
//! lash's advisory lock serializes only the participants that take it.
//! [`PostgresStorage::schema_advisory_lock_key`] publishes the key so a host's
//! migrations can participate.

use std::sync::Arc;
use std::time::Duration;

use lash_core::runtime::{
    QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, QueuedWorkClaimPolicy, QueuedWorkCompletion, QueuedWorkEnqueueOutcome,
    QueuedWorkItem, QueuedWorkKind,
};
use lash_core::store::queued_work::{
    ClaimCandidate, WorkClaimLease, claim_scan_limit, derive_batch_id,
    select_exact_turn_work_claim_prefix, select_leading_session_command,
    select_turn_work_claim_prefix,
};
use lash_core::store::{
    HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit, RuntimeCommitResult,
    SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, AwaitEventResolver, BlobRef, DeliveryPolicy, EffectHost, ExecutionScope,
    GcReport, LeaseOwnerIdentity, PersistedSegmentHandover, ProcessAwaitOutput, ProcessChange,
    ProcessChangeCursor, ProcessContinuationStore, ProcessEvent, ProcessEventAppendRequest,
    ProcessEventAppendResult, ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease,
    ProcessLeaseCompletion, ProcessLiveReferenceSummary, ProcessObserverBy, ProcessPruneReport,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome, ProcessStarted,
    QueuedWorkStore, RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimePersistence,
    ScopedEffectController, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionMeta, SessionNodeRecord,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError, StoreMaintenance, TokenLedgerEntry,
    TurnInputStore, VacuumReport, facade_support::ProcessStartPlan,
    facade_support::registry_transitions,
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
// Bumped to 22 so each anchor binds one continuation checkpoint and source
// session snapshot.
// Bumped to 23 so the then-reusable session names carried durable per-lifetime
// incarnation identity in session metadata.
// Bumped to 24 so PostgreSQL checkpoints use the shared manifest plus
// content-addressed component blobs. Pre-24 checkpoint blobs contain the
// removed backend-only envelope and are rejected at open.
// Bumped to 25 because runtime commit receipts no longer persist the removed
// realization digest; stores derive their lookup hash from commit content.
// Bumped to 26 to remove cached graph-node reference counts. Node retirement
// now derives liveness from parent edges, session heads, and anchors.
// Bumped to 27 for canonical typed effect-journal identity and indexed
// session/incarnation lifecycle joins. Pre-27 databases are rejected.
// Bumped to 28 for permanent session-id reuse rejection and removal of
// incarnation identity from session metadata and effect journals.
// Bumped to 29 for the process-wake outbox and receiver-side consumed-wake
// evidence. Pre-29 components are rejected and recreated.
// Bumped to 30 so terminal wake deliveries retain a durable exact-evidence
// cleanup reconciliation bit. Pre-30 components are rejected and recreated.
// Bumped to 31 to replace that lane with consumed high-water marks and add
// fair per-group retry scheduling. Pre-31 components are rejected and recreated.
// Bumped to 32 for the FIG-661 observer, indexed wake, and tombstone cutover.
// Bumped to 33 for enqueuing wake claims and raw session originator ids.
// Bumped to 34 for per-attempt wake-delivery claim tokens.
// Bumped to 35 to replace the consumed-watermark vocabulary with receiver
// allocation fences and add durable sender allocation floors. Process-event
// sequences remain small and monotone across pruned incarnations.
// Bumped to 36 for FIG-850 append-request identity receipts and idempotent
// usage publication. Nullable receipt columns preserve exact-commit-hash
// behavior for copied pre-upgrade rows; usage identities include the required
// payload-encoding version and canonical payload hash. This unreleased schema
// was completed in place under the existing reject-and-recreate operator flow.
// Version 37 is the coordinated FIG-886 reject-and-recreate identity cutover
// layered over the complete version-36 append-receipt and usage schema.
// Version 38 is the coordinated FIG-915 cutover: residual durable names and
// usage payloads use shared framing, while redundant process/trigger hashes
// are replaced by structural conflict checks.
// Version 39 adds the required per-turn budget to every durable session policy
// carrier. Older stores are rejected and recreated without a compatibility
// read path.
// Version 40 adds immutable graph generations and frame pointers plus
// zero-copy fork-lineage accelerators. Older stores are rejected and recreated;
// there is no backfill or compatibility read path.
// Version 41 indexes the bounded non-terminal recovery worklist by process id.
// Version 42 replaces the fixed checkpoint slots with a complete keyed
// component descriptor set carrying per-component encoding versions. Older
// roots are rejected under the existing drain-and-recreate policy.
// Version 43 removes the CLI-era session name, creation timestamp, model, and
// working-directory keys from the session metadata JSON payload. Older stores
// are rejected and recreated; there is no compatibility read path.
// Version 45 makes nested session metadata strict and includes enum/tag values
// in its registered payload shape.
// Version 46 replaces that JSON carrier with structural columns and narrow
// ordered child tables. Older databases are rejected and recreated; there is
// no JSON or compatibility read path.
// Version 47 cuts queued-work storage over from slot_policy/merge_key_json to
// work_kind/authority_json/nullable merge_key.
// Version 49 rejects completed tool-attempt outcomes whose frame-switch control
// still carries the pre-cutover `frame_id` field.
// Version 50 adds the runtime-minted executor discriminator and store-authored
// lease term to session lease rows. Older stores are rejected and recreated;
// there is no compatibility read path.
// Version 48 remains reserved by FIG-1133.
// Version 51 adds durable runtime-owned tool-intent first-submission rows and
// process-parent teardown retention. Lash-managed version-50 stores take the
// explicit 50 -> 51 creation-only migration at open.
// Version 52 adds the attachment GC fence's per-digest condemnation table.
// Lash-managed version-51 stores take the explicit 51 -> 52 creation-only
// migration at open.
// Version 53 indexes the ordering keys that let idle arbitration compare the
// earliest pending session command with the earliest pending turn input without
// hydrating either queue. Both indexes cover columns 52 already stores, so
// version-50, -51, and -52 stores take a creation-only migration at open.
// Version 54 adds the durable effect-group journal: a `lash_runtime_effect_group`
// row per open group carrying the settlement-sequence allocator, plus the
// `group_key` and `settlement_seq` columns that tie a journalled child to its
// group. Both columns are nullable with no default, so PostgreSQL adds them as
// catalog metadata and every already-journalled effect keeps its recorded
// `envelope_hash` — and therefore its lease fence — across the upgrade. Stores at
// 50 through 53 take a creation-only migration at open.
// Version 55 indexes the loser drain's queue read: one group's children that
// hold no settlement rank yet. An index and nothing else, so stores at 50
// through 54 take a creation-only migration at open; SQLite carries the same
// index unversioned, and `RUNTIME_EFFECT_REPLAY_GROUP_UNSETTLED_INDEX_DDL` says why.
const SCHEMA_VERSION: i32 = 55;

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
    session_id: String,
    #[cfg(test)]
    checkpoint_probe_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    checkpoint_write_transaction_count: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
pub struct PostgresProcessRegistry {
    pool: PgPool,
    wake_delivery_config: lash_core::WakeDeliveryConfig,
    clock: Arc<dyn lash_core::Clock>,
}

impl PostgresProcessRegistry {
    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

#[derive(Clone)]
pub struct PostgresTriggerStore {
    pool: PgPool,
    clock: Arc<dyn lash_core::Clock>,
    fixed_incarnation: Option<String>,
}

impl PostgresTriggerStore {
    /// Bind trigger record timestamps to an explicit clock.
    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Pin otherwise-random trigger incarnation identity for durable fixture generation.
    #[doc(hidden)]
    pub fn with_incarnation_for_testing(mut self, incarnation: impl Into<String>) -> Self {
        self.fixed_incarnation = Some(incarnation.into());
        self
    }
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
    /// Who owns the DDL. Default [`SchemaProvisioning::LashManaged`]: lash applies
    /// its own creation statements at open. Hosts that vendor
    /// [`PostgresStorage::schema_ddl`] into their own migration tooling set
    /// [`SchemaProvisioning::HostProvisioned`] so open runs no DDL at all.
    pub schema_provisioning: SchemaProvisioning,
    /// What open does when the live schema drifts from the shape this build
    /// expects. Default [`SchemaCheck::Enforce`].
    pub schema_check: SchemaCheck,
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
            schema_provisioning: SchemaProvisioning::default(),
            schema_check: SchemaCheck::default(),
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
        let await_event_signing_secret = ensure_schema(&pool, schema_open_options(&config)).await?;
        Ok(Self {
            pool,
            await_event_signing_secret: await_event_signing_secret.into(),
        })
    }

    /// Build storage over an already-constructed pool.
    ///
    /// This runs the same schema gate `connect`/`connect_with` do, so every public
    /// construction path enforces both the component schema version and the
    /// structural shape: a pre-cutover (e.g. version-10) database is rejected
    /// loudly with the same mismatch error rather than silently used, which would
    /// resurrect the cross-version hazards the version bump exists to prevent. The
    /// creation statements are idempotent, so running the gate against an
    /// already-provisioned pool is safe.
    ///
    /// Use [`PostgresStorage::from_pool_with`] to open a host-provisioned database
    /// without running any DDL.
    pub async fn from_pool(pool: PgPool) -> Result<Self, StoreError> {
        Self::from_pool_with(pool, PostgresStoreConfig::default()).await
    }

    /// Build storage over an already-constructed pool, choosing who provisions
    /// the schema and how a mismatch is handled.
    ///
    /// Only [`PostgresStoreConfig::schema_provisioning`] and
    /// [`PostgresStoreConfig::schema_check`] are read: the pool already exists, so
    /// its sizing and per-connection timeouts were fixed by whoever built it.
    pub async fn from_pool_with(
        pool: PgPool,
        config: PostgresStoreConfig,
    ) -> Result<Self, StoreError> {
        let await_event_signing_secret = ensure_schema(&pool, schema_open_options(&config)).await?;
        Ok(Self {
            pool,
            await_event_signing_secret: await_event_signing_secret.into(),
        })
    }

    /// Construct storage after the caller has already structurally verified this
    /// exact pool.
    ///
    /// This testing-only seam exists so the performance harness can subtract the
    /// structural catalog gate from an otherwise identical open. It still checks
    /// the unconditional component-version boundary and the signing-secret data
    /// precondition; only structural verification is skipped.
    #[cfg(feature = "testing")]
    #[doc(hidden)]
    pub async fn from_preverified_pool_for_testing(pool: PgPool) -> Result<Self, StoreError> {
        let found_version: Option<i32> =
            sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
                .bind(SCHEMA_COMPONENT)
                .fetch_optional(&pool)
                .await
                .map_err(store_sqlx_error)?;
        if found_version != Some(SCHEMA_VERSION) {
            return Err(version_mismatch_error(found_version));
        }
        let signing_secret: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT signing_secret FROM lash_await_event_meta WHERE singleton = TRUE",
        )
        .fetch_optional(&pool)
        .await
        .map_err(store_sqlx_error)?;
        let signing_secret = signing_secret.ok_or_else(|| {
            StoreError::Backend(
                "Postgres await-event signing secret row is missing from pre-verified pool"
                    .to_string(),
            )
        })?;
        if signing_secret.len() != AWAIT_EVENT_SIGNING_SECRET_BYTES {
            return Err(StoreError::Backend(format!(
                "Postgres await-event signing secret has {} bytes, expected {AWAIT_EVENT_SIGNING_SECRET_BYTES}",
                signing_secret.len()
            )));
        }
        Ok(Self {
            pool,
            await_event_signing_secret: signing_secret.into(),
        })
    }

    /// The exact DDL this build provisions, including the seed rows every open
    /// mode requires.
    ///
    /// A host that owns its own migrations should vendor these bytes verbatim —
    /// the same content is committed as `crates/lash-postgres-store/schema.sql` —
    /// and never transcribe them: lash verifies the resulting structure at open
    /// and rejects a mismatch with a per-object diff. Every statement is
    /// creation-only and idempotent, and nothing is schema-qualified, so the DDL
    /// provisions into whichever schema the session's `search_path` resolves.
    pub fn schema_ddl() -> &'static str {
        SCHEMA_DDL
    }

    /// The component schema version this build implements, as stamped in
    /// `lash_schema_versions`.
    ///
    /// The component schema is normally a reject-and-recreate boundary. This
    /// build has two explicit exceptions: Lash-managed `Enforce` mode can apply
    /// the creation-only migrations from the published component-50, -51, or
    /// -52 shapes to 53 after an exact source-shape preflight. An older
    /// stamp over newer artifacts is ledger/schema divergence and is refused
    /// with an inspect-and-recreate remedy; other mismatches are rejected at
    /// open.
    pub fn schema_version() -> i32 {
        SCHEMA_VERSION
    }

    /// Compares the live database against the schema this build expects and
    /// returns the structured result.
    ///
    /// This is the same check every open runs, exposed so a host can gate its own
    /// migration CI on it — the intent being that a production open is the
    /// backstop that never fires rather than the place drift is discovered.
    /// Unlike open, this never fails on drift: inspect
    /// [`SchemaReport::is_conformant`] and [`SchemaReport::findings`], or render
    /// the report for a sectioned expected-versus-found diff.
    ///
    /// Use [`PostgresStorage::verify_schema_for`] to inspect a database that is
    /// too broken to open, which is most of the ones worth inspecting.
    pub async fn verify_schema(&self) -> Result<SchemaReport, StoreError> {
        Self::verify_schema_for(&self.pool).await
    }

    /// Runs the same check against a pool, without opening storage over it.
    ///
    /// Constructing a [`PostgresStorage`] is strictly harder than verifying one:
    /// open additionally insists on a matching component version stamp and a
    /// usable await-event signing secret, and either of those can be exactly what
    /// a host's migration produced wrongly. A check reachable only through a
    /// successful open could therefore not describe the databases it exists to
    /// describe, so this form needs no receiver and no successful open — it
    /// reports every version, structural, and seed-row finding, including a
    /// signing secret seeded at the wrong width, and returns them rather than
    /// failing.
    ///
    /// Acquires [`PostgresStorage::schema_advisory_lock_key`] in shared mode and
    /// then reads inside one `REPEATABLE READ` transaction, so every `pg_catalog`
    /// read shares a single snapshot taken after the lock was granted and a host
    /// migration holding the same key exclusively is excluded for the duration.
    ///
    /// Because it takes the key itself, this cannot be called by something that
    /// already holds it — see [`PostgresStorage::verify_schema_on`] for that.
    ///
    /// This is the entry point for a host's migration CI:
    ///
    /// ```no_run
    /// # async fn gate(pool: sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
    /// let report = lash_postgres_store::PostgresStorage::verify_schema_for(&pool).await?;
    /// assert!(report.is_conformant(), "{report}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn verify_schema_for(pool: &PgPool) -> Result<SchemaReport, StoreError> {
        verify_schema_under_advisory_lock(pool).await
    }

    /// Runs the same check on a connection the caller already owns, taking no lock
    /// and starting no transaction of its own.
    ///
    /// This is the verifier for the published migration protocol. A CI job that
    /// holds [`PostgresStorage::schema_advisory_lock_key`] around its
    /// migrate-then-verify sequence cannot use
    /// [`PostgresStorage::verify_schema_for`]: that one acquires the key itself, so
    /// it would queue behind the caller's own exclusive hold and never proceed.
    /// Pass the locked connection here instead.
    ///
    /// The caller owns both guarantees this skips. Hold the key for the whole
    /// sequence, and read inside a `REPEATABLE READ` transaction if the catalog
    /// reads should share one snapshot — `SET TRANSACTION ISOLATION LEVEL
    /// REPEATABLE READ` must be the transaction's first statement, before anything
    /// that waits for a lock, or the snapshot predates the grant. An open
    /// [`sqlx::Transaction`] derefs to the connection this wants, so
    /// `verify_schema_on(&mut tx)` works directly.
    ///
    /// ```no_run
    /// # use lash_postgres_store::PostgresStorage;
    /// # async fn gate(pool: sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
    /// use sqlx::Connection as _;
    ///
    /// let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    /// let mut connection = pool.acquire().await?;
    /// sqlx::query("SELECT pg_advisory_lock($1, $2)")
    ///     .bind(namespace)
    ///     .bind(key)
    ///     .execute(&mut *connection)
    ///     .await?;
    /// // ... run the migrations here, still holding the key ...
    /// let report = PostgresStorage::verify_schema_on(&mut connection).await?;
    /// assert!(report.is_conformant(), "{report}");
    /// sqlx::query("SELECT pg_advisory_unlock($1, $2)")
    ///     .bind(namespace)
    ///     .bind(key)
    ///     .execute(&mut *connection)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn verify_schema_on(
        connection: &mut sqlx::PgConnection,
    ) -> Result<SchemaReport, StoreError> {
        verify_schema_shape(connection).await
    }

    /// The advisory-lock key lash holds while provisioning, opening, or verifying
    /// the schema, as `(namespace, key)` arguments to the `pg_advisory_lock` family.
    ///
    /// Open and provisioning take it exclusively;
    /// [`PostgresStorage::verify_schema_for`] takes it in shared mode. Between them
    /// that serializes everything lash does to the schema — but it cannot by itself
    /// coordinate a host migration that does not participate. A non-participating
    /// migration can commit before a verification's snapshot or after its commit, so
    /// the report describes the schema as of that snapshot rather than as of now.
    ///
    /// The supported protocol is therefore to take this key around migrations —
    /// `SELECT pg_advisory_xact_lock(715421, 907001)` in the migration's own
    /// transaction, or the session-level form around a multi-statement migration.
    /// A migration CI job should wrap it around the whole migrate-then-verify
    /// sequence and verify with [`PostgresStorage::verify_schema_on`], which does
    /// not try to take the key a second time. Deployments that can instead guarantee
    /// no migration runs concurrently with an open need nothing.
    pub fn schema_advisory_lock_key() -> (i32, i32) {
        SCHEMA_ADVISORY_LOCK_KEY
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn session_store_factory(&self) -> PostgresSessionStoreFactory {
        warn_postgres_process_registry_not_wired();
        PostgresSessionStoreFactory {
            pool: self.pool.clone(),
            process_registry_shared: false,
            clock: Arc::new(lash_core::facade_support::SystemClock),
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
            clock: Arc::new(lash_core::facade_support::SystemClock),
        }
    }

    /// Construct a handle bound to `session_id` without validating that the
    /// session already exists.
    ///
    /// Construction binds identity; it does not validate existence. Reads of a
    /// nonexistent session return `Ok(None)`, and a later admission or commit
    /// may create that id. Consequently, a mistyped id produces a valid absent
    /// handle that can subsequently create the mistyped session. Call
    /// [`SessionStoreFactory::open_existing_store`](lash_core::SessionStoreFactory::open_existing_store)
    /// through [`Self::session_store_factory`] when existence must be checked.
    pub fn session_store(&self, session_id: impl Into<String>) -> PostgresSessionStore {
        PostgresSessionStore {
            pool: self.pool.clone(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            session_id: session_id.into(),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn process_registry(&self) -> PostgresProcessRegistry {
        PostgresProcessRegistry {
            pool: self.pool.clone(),
            wake_delivery_config: lash_core::WakeDeliveryConfig::default(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
        }
    }

    pub fn process_registry_with_wake_delivery_config(
        &self,
        wake_delivery_config: lash_core::WakeDeliveryConfig,
    ) -> PostgresProcessRegistry {
        PostgresProcessRegistry {
            pool: self.pool.clone(),
            wake_delivery_config,
            clock: Arc::new(lash_core::facade_support::SystemClock),
        }
    }

    pub fn trigger_store(&self) -> PostgresTriggerStore {
        PostgresTriggerStore {
            pool: self.pool.clone(),
            clock: Arc::new(lash_core::facade_support::SystemClock),
            fixed_incarnation: None,
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
    /// Bind this handle to an explicit clock for deterministic embedding and tests.
    pub fn with_clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = clock;
        self
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

    fn bind_session_id(&self, attempted_session_id: &str) -> Result<(), StoreError> {
        if self.session_id == attempted_session_id {
            Ok(())
        } else {
            Err(StoreError::SessionBindingMismatch {
                bound_session_id: self.session_id.clone(),
                attempted_session_id: attempted_session_id.to_string(),
            })
        }
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
#[path = "postgres/schema_shape.rs"]
mod schema_shape;
#[path = "postgres/session_factory.rs"]
mod session_factory;
#[path = "postgres/session_meta.rs"]
mod session_meta;
#[path = "postgres/support.rs"]
mod support;
#[cfg(any(test, feature = "testing"))]
#[path = "postgres/testing.rs"]
pub mod testing;
#[path = "postgres/trigger_store.rs"]
mod trigger_store;

pub use effect_replay::{
    PostgresEffectHost, PostgresEffectReplayOptions, PostgresRuntimeEffectController,
};
use schema_shape::{
    AWAIT_EVENT_SIGNING_SECRET_BYTES, ComponentVersion, SchemaShape, read_component_version,
    read_search_path, resolve_installation, verify_schema_migration_source_shape,
    verify_schema_shape,
};
pub use schema_shape::{
    ColumnShape, ColumnValueSource, ForeignKeyAction, ForeignKeyShape, SchemaCheck, SchemaFinding,
    SchemaProvisioning, SchemaReport, UniqueGuard,
};
use {process_helpers::*, runtime_persistence::*, schema::*, session_factory::*, support::*};

/// Extracts the schema-gate knobs one open should use from a store config.
fn schema_open_options(config: &PostgresStoreConfig) -> SchemaOpenOptions {
    SchemaOpenOptions {
        provisioning: config.schema_provisioning,
        check: config.schema_check,
    }
}

#[cfg(test)]
#[path = "postgres/checkpoint_depth_tests.rs"]
mod checkpoint_depth_tests;
#[cfg(test)]
mod graph_integrity_tests;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn direct_session_store_defers_missing_identity_validation() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping direct-session-store contract: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect direct-session-store contract storage");
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables
             WHERE schemaname = 'public'
               AND tablename LIKE 'lash\\_%'
               AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
             ORDER BY tablename",
        )
        .fetch_all(storage.pool())
        .await
        .expect("list Lash tables for direct-constructor test reset");
        let truncate = format!("TRUNCATE {} RESTART IDENTITY CASCADE", tables.join(", "));
        sqlx::query(&truncate)
            .execute(storage.pool())
            .await
            .expect("reset direct-constructor test tables");
        let store = storage.session_store("missing");

        assert!(matches!(store.load_session_head_meta().await, Ok(None)));
        assert!(matches!(store.load_session_meta().await, Ok(None)));
        assert_eq!(
            store
                .admit_and_bind_session(&lash_core::SessionBinding::root("missing"))
                .await
                .expect("admit missing direct-constructor session"),
            lash_core::SessionAdmission::Created
        );
    }

    #[tokio::test]
    async fn one_id_selected_drain_touches_at_most_four_queue_rows() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping selected-drain plan proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect selected-drain plan storage");
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .execute(storage.pool())
            .await
            .expect("enable pg_stat_statements for selected-drain plan proof");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let session_id = format!("selected-plan-session:{nonce}");
        let batch_prefix = format!("selected-plan-batch:{nonce}:");
        let source_prefix = format!("selected-plan-source:{nonce}:");
        sqlx::query(
            "INSERT INTO lash_queued_work_batches
             (batch_id, session_id, source_key, delivery_policy, work_kind,
              authority_json, merge_key, available_at_ms, enqueued_at_ms)
             SELECT $1 || value::text, $2, $3 || value::text,
                    'earliest_safe_boundary', 'turn', '{}', NULL, 0, 1
             FROM generate_series(1, 10000) AS value",
        )
        .bind(&batch_prefix)
        .bind(&session_id)
        .bind(&source_prefix)
        .execute(storage.pool())
        .await
        .expect("seed 10,000 ready selected-drain batches");
        sqlx::query(
            "INSERT INTO lash_queued_work_items (batch_id, item_index, item_id, payload_json)
             SELECT $1 || value::text, 0, $1 || value::text || ':item:0',
                    '{\"type\":\"agent_frame_task\",\"frame_id\":\"selected-plan\",\"task\":\"selected plan row\"}'
             FROM generate_series(1, 10000) AS value",
        )
        .bind(&batch_prefix)
        .execute(storage.pool())
        .await
        .expect("seed selected-drain batch payloads");
        sqlx::query("ANALYZE lash_queued_work_batches")
            .execute(storage.pool())
            .await
            .expect("analyze selected-drain plan fixture");

        let store = storage.session_store(&session_id);
        let owner = LeaseOwnerIdentity::opaque(
            "selected-plan-owner",
            format!("selected-plan-owner:{nonce}"),
        );
        let lease = store
            .try_claim_session_execution_lease(
                &session_id,
                &owner,
                "schema-congruence-test-executor",
                60_000,
            )
            .await
            .expect("claim selected-drain plan lease")
            .acquired()
            .expect("selected-drain plan lane is free");
        sqlx::query("SELECT pg_stat_statements_reset()")
            .execute(storage.pool())
            .await
            .expect("reset selected-drain statement statistics");
        let selected_batch_id = format!("{batch_prefix}5000");
        let claim = store
            .claim_ready_queued_work_by_batch_ids(
                &session_id,
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&selected_batch_id),
                lash_core::testing::queued_work_claim_policy(64),
            )
            .await
            .expect("claim one selected row from 10,000")
            .expect("selected row is claimable");
        let selected_source_key = format!("{source_prefix}5000");
        assert_eq!(
            claim
                .batches
                .iter()
                .map(|batch| batch.source_key.as_deref())
                .collect::<Vec<_>>(),
            vec![Some(selected_source_key.as_str())]
        );
        let measured_queue_rows: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(rows), 0)::bigint
             FROM pg_stat_statements
             WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
               AND query LIKE '%lash_queued_work_batches%'
               AND query NOT LIKE '%pg_stat_statements%'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("measure selected-drain queue rows");
        assert!(
            measured_queue_rows <= 4,
            "one-ID selected drain may return or lock at most 4 queue rows, measured {measured_queue_rows}"
        );

        store
            .abandon_queued_work_claim(&claim)
            .await
            .expect("abandon selected-drain plan claim");
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release selected-drain plan lease");
        sqlx::query("DELETE FROM lash_queued_work_batches WHERE session_id = $1")
            .bind(&session_id)
            .execute(storage.pool())
            .await
            .expect("remove selected-drain plan fixture");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_commits_return_one_typed_head_revision_conflict() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping concurrent first-commit proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect concurrent first-commit storage");
        let factory = storage.session_store_factory();
        let session_id = format!("postgres-first-commit-race:{}", uuid::Uuid::new_v4());
        let request = SessionStoreCreateRequest {
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        let first_store = factory
            .create_store(&request)
            .await
            .expect("create first racing handle");
        let second_store = factory
            .create_store(&request)
            .await
            .expect("create second racing handle");
        let mut first_state = lash_core::RuntimeSessionState {
            session_id: session_id.clone(),
            ..lash_core::RuntimeSessionState::new(request.policy.clone())
        };
        first_state.ensure_agent_frame_initialized();
        let second_state = first_state.clone();
        let (first_commit, _) =
            lash_core::RuntimeCommit::persisted_state_for_test(&first_state, &[])
                .with_operation(lash_core::OperationId::turn(
                    &session_id,
                    "first-racer",
                    "final",
                ))
                .expect("build first racing commit");
        let (second_commit, _) =
            lash_core::RuntimeCommit::persisted_state_for_test(&second_state, &[])
                .with_operation(lash_core::OperationId::turn(
                    &session_id,
                    "second-racer",
                    "final",
                ))
                .expect("build second racing commit");
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let first_start = Arc::clone(&start);
        let second_start = Arc::clone(&start);
        let (first, second) = tokio::join!(
            async move {
                first_start.wait().await;
                first_store.commit_runtime_state(first_commit).await
            },
            async move {
                second_start.wait().await;
                second_store.commit_runtime_state(second_commit).await
            }
        );
        let results = [first, second];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(StoreError::HeadRevisionConflict {
                        expected: 0,
                        actual: 1
                    })
                ))
                .count(),
            1,
            "the losing first commit must fail with the typed CAS conflict: {results:?}"
        );
    }

    #[tokio::test]
    async fn postgres_graph_generation_uniqueness_is_typed() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping graph-generation error proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect graph-generation error storage");
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let session_id = format!("postgres-generation-collision:{nonce}");
        let first_node = format!("generation-node-a:{nonce}");
        let second_node = format!("generation-node-b:{nonce}");
        sqlx::query(
            "INSERT INTO lash_graph_nodes
             (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
             VALUES ($1, $2, NULL, 3, $2, '{}')",
        )
        .bind(&session_id)
        .bind(&first_node)
        .execute(storage.pool())
        .await
        .expect("seed graph-generation uniqueness fixture");
        let raw = sqlx::query(
            "INSERT INTO lash_graph_nodes
             (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
             VALUES ($1, $2, NULL, 3, $2, '{}')",
        )
        .bind(&session_id)
        .bind(&second_node)
        .execute(storage.pool())
        .await
        .expect_err("duplicate generation must violate Postgres uniqueness");
        let error = graph_node_insert_error(raw, &session_id, 3, &second_node);
        assert!(matches!(
            error,
            StoreError::GraphGenerationCollision {
                session_id: ref actual_session_id,
                generation: 3
            } if actual_session_id == &session_id
        ));
        sqlx::query("DELETE FROM lash_graph_nodes WHERE session_id = $1")
            .bind(&session_id)
            .execute(storage.pool())
            .await
            .expect("clean graph-generation uniqueness fixture");
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
            data: lash_core::TurnInputCompletionData {
                input_ids: vec![input_id.clone()],
                applications: Vec::new(),
            },
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
                ref claim_id,
                ..
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
    async fn postgres_delete_permanently_fences_stale_handles_and_session_id_reuse() {
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
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        let stale_store = factory
            .create_store(&request)
            .await
            .expect("create stale store");
        let mut state = lash_core::RuntimeSessionState {
            session_id: session_id.clone(),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();

        factory
            .delete_session(&session_id)
            .await
            .expect("delete before first commit");
        let error = stale_store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect_err("stale first commit must not resurrect the session");
        assert!(matches!(
            error,
            StoreError::SessionDeleted {
                ref session_id
            } if session_id == &request.session_id
        ));

        let reuse_error = match factory.create_store(&request).await {
            Ok(_) => panic!("deleted session id must never be reused"),
            Err(error) => error,
        };
        assert!(matches!(
            reuse_error,
            StoreError::SessionDeleted {
                ref session_id
            } if session_id == &request.session_id
        ));
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

    /// Arming a delete and a writer taking the digest back are the two halves of
    /// the same CAS: run concurrently against PostgreSQL, at most one of them
    /// may win.
    ///
    /// This is the law that catches a transition running bare on the pool
    /// instead of inside a transaction under the per-digest advisory key. With
    /// `arm_attachment_delete` unfenced, its `UPDATE` can commit *inside* the
    /// writer's open transaction — after the writer read `condemned` and before
    /// it deleted the row — so the writer erases a `deleting` row, is granted,
    /// and puts bytes into a delete that is already in flight. The post-delete
    /// probe is no defence against that: it only fires once the bytes are gone.
    ///
    /// Multi-threaded on purpose: the manifest surface is synchronous, so the
    /// writer blocks a thread on a detached runtime while the pool's IO driver
    /// and the sweeper half keep running on others.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn arming_a_delete_and_a_concurrent_writer_never_both_win() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping Postgres attachment fence race proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect attachment fence database");
        let session_id = format!("postgres-attachment-fence-race:{}", std::process::id());
        let store = std::sync::Arc::new(storage.session_store(&session_id));
        let factory = storage.session_store_factory();
        let attachment_id =
            lash_core::AttachmentId::parse(format!("fence-race-{}", std::process::id()))
                .expect("valid attachment id");
        sqlx::query("DELETE FROM lash_attachment_condemnations WHERE attachment_id = $1")
            .bind(attachment_id.as_str())
            .execute(storage.pool())
            .await
            .expect("clear condemnation fixture");
        let intent = {
            let session_id = session_id.clone();
            let attachment_id = attachment_id.clone();
            move || lash_core::AttachmentIntent {
                attachment_id: attachment_id.clone(),
                session_id: session_id.clone(),
                canonical_uri: format!("lash-attachment://sha256/{attachment_id}"),
                intent_at_epoch_ms: 1,
                owner_kind: None,
                owner_id: None,
            }
        };

        // Widen the writer's read-then-revoke window so the interleaving an
        // unfenced `arm` corrupts is reached on every odd round instead of once
        // in a blue moon. The fence does not care how wide the window is: a
        // concurrent `arm` waits on the per-digest advisory key either way.
        crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS
            .store(20, std::sync::atomic::Ordering::Relaxed);

        // Both orderings, every round: the fixed code holds for all of them.
        for round in 0..12 {
            assert_eq!(
                lash_core::AttachmentRootSet::condemn_attachment(&factory, &attachment_id, 0)
                    .await
                    .expect("condemn"),
                lash_core::AttachmentCondemnation::Condemned,
                "round {round}: the digest must start each round rootless and free"
            );

            let writer = tokio::task::spawn_blocking({
                let store = std::sync::Arc::clone(&store);
                let intent = intent.clone();
                move || lash_core::AttachmentManifest::begin_attachment_write(&*store, intent())
            });
            if round % 2 == 1 {
                // Alternate the stagger so both orderings are exercised: on odd
                // rounds the writer reaches its condemnation read first (the
                // interleaving an unfenced `arm` corrupts), on even rounds the
                // sweeper arms first. The pacing widens a window; it decides no
                // outcome, and the invariant below holds for either ordering.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let armed =
                lash_core::AttachmentRootSet::arm_attachment_delete(&factory, &attachment_id)
                    .await
                    .expect("arm");
            let fence = writer.await.expect("join writer").expect("fenced write");

            let holds_ref =
                lash_core::AttachmentManifest::holds_ref(&*store, &session_id, &attachment_id)
                    .expect("holds_ref");
            match (armed, fence) {
                // The sweeper won: the delete is armed and the writer parked
                // without recording anything, so no bytes can land inside it.
                (
                    lash_core::AttachmentDeleteArming::Armed,
                    lash_core::AttachmentWriteFence::ReclamationInFlight,
                ) => {
                    assert!(
                        !holds_ref,
                        "round {round}: a parked writer records no intent"
                    );
                }
                // The writer won: it took the digest back before the delete was
                // armed, and the sweeper issues no delete at all.
                (
                    lash_core::AttachmentDeleteArming::Revoked,
                    lash_core::AttachmentWriteFence::Granted,
                ) => {
                    assert!(
                        holds_ref,
                        "round {round}: a granted writer records its intent"
                    );
                }
                (armed, fence) => panic!(
                    "round {round}: arming and the writer must never both win \
                     (arm = {armed:?}, writer = {fence:?}); bytes would land inside an \
                     in-flight delete"
                ),
            }

            lash_core::AttachmentRootSet::release_attachment_condemnation(&factory, &attachment_id)
                .await
                .expect("release");
            if holds_ref {
                lash_core::AttachmentManifest::forget(&*store, &session_id, &attachment_id)
                    .expect("forget the ref");
            }
        }

        crate::attachments::FENCE_WRITER_WINDOW_DELAY_MS
            .store(0, std::sync::atomic::Ordering::Relaxed);
        factory
            .delete_session(&session_id)
            .await
            .expect("delete fence session");
    }

    #[tokio::test]
    async fn attachment_gc_refuses_an_empty_postgres_root_database() {
        let Some(database_url) = postgres_test_support::database_url() else {
            eprintln!("skipping empty Postgres attachment-root proof: database URL is not set");
            return;
        };
        let _database_lock =
            postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
        let storage = PostgresStorage::connect(&database_url)
            .await
            .expect("connect empty attachment-root database");
        sqlx::query("DELETE FROM lash_attachment_manifest")
            .execute(storage.pool())
            .await
            .expect("make the configured Postgres manifest empty");
        let wrong_factory = storage.session_store_factory();

        let live_factory = lash_core::runtime::InMemorySessionStoreFactory::new();
        let request = SessionStoreCreateRequest {
            session_id: "postgres-wrong-database-live-attachment".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        };
        let live_store = live_factory
            .create_store(&request)
            .await
            .expect("create live root authority");
        let backend = lash_core::attachments::InMemoryAttachmentStore::new();
        let attachment = lash_core::AttachmentStore::put(
            &backend,
            b"postgres-live-committed-blob".to_vec(),
            lash_sansio::AttachmentCreateMeta::new(
                lash_sansio::MediaType::parse("application/octet-stream").expect("media type"),
                None,
                Some("live".to_string()),
            ),
        )
        .await
        .expect("put shared backend blob");
        lash_core::AttachmentManifest::record_intent(
            &*live_store,
            lash_core::AttachmentIntent {
                attachment_id: attachment.id.clone(),
                session_id: request.session_id.clone(),
                canonical_uri: format!("lash-attachment://sha256/{}", attachment.id),
                intent_at_epoch_ms: 1,
                owner_kind: None,
                owner_id: None,
            },
        )
        .expect("record live attachment intent");
        lash_core::AttachmentManifest::commit_refs(
            &*live_store,
            &request.session_id,
            std::slice::from_ref(&attachment.id),
        )
        .expect("commit live attachment ref");

        let result = lash_core::attachments::reclaim_unreferenced_attachments(
            &wrong_factory,
            &backend,
            lash_core::AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
            },
        )
        .await;

        assert!(
            matches!(
                result,
                Err(lash_core::AttachmentStoreError::EmptyRootSetRefused)
            ),
            "an empty Postgres root database must refuse deletion: {result:?}"
        );
        lash_core::AttachmentStore::get(&backend, &attachment.id)
            .await
            .expect("live committed blob survives the refused sweep");
    }
}
