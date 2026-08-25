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
    ClaimCandidate, MAX_SESSION_COMMAND_BATCHES_PER_CLAIM, QueuedWorkClaimOutcome,
    QueuedWorkClaimRefusal, WorkClaimLease, claim_scan_limit, derive_batch_id,
    select_exact_turn_work_claim_prefix, select_leading_session_command,
    select_turn_work_claim_prefix,
};
use lash_core::store::{
    HydratedSessionCheckpoint, PersistedSessionRead, RuntimeCommit, RuntimeCommitReceipt,
    SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core::{
    AbandonRequest, AttachmentId, AttachmentIntent, AttachmentManifest, AttachmentManifestEntry,
    AttachmentOwnerKind, AwaitEventResolver, BlobRef, DeliveryPolicy, EffectHost, ExecutionScope,
    GcReport, LeaseOwnerIdentity, PersistedSegmentHandover, ProcessAwaitOutput, ProcessChange,
    ProcessChangeCursor, ProcessContinuationStore, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest, ProcessExecutionWriteAuthority, ProcessExternalRef, ProcessLease,
    ProcessLeaseCompletion, ProcessLiveReferenceView, ProcessObserverBy, ProcessPruneReport,
    ProcessRecord, ProcessRegistration, ProcessRegistry, ProcessStartOutcome, ProcessStarted,
    QueuedWorkStore, RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimePersistence,
    ScopedEffectController, SessionCommitStore, SessionExecutionLease,
    SessionExecutionLeaseAcquisition, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionListFilter, SessionMeta,
    SessionNodeRecord, SessionRelationKind, SessionStoreCreateRequest, SessionStoreFactory,
    SessionSummary, StoreError, StoreMaintenance, TokenLedgerEntry, TurnInputStore, VacuumReport,
    facade_support::ProcessStartPlan, facade_support::ProcessTransition,
    facade_support::ProcessTransitionPlan, facade_support::registry_transitions,
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
// Version 56 adds the nullable trigger-occurrence reclaim eligibility arm and
// its partial maintenance index. Stores at 50 through 55 take a creation-only
// migration that arms legacy zero-fan-out rows from their occurrence time while
// leaving live-fan-out rows unarmed for the normal terminal transition.
// Version 57 adds the indexed checkpoint-manifest component-edge projection and
// root indexes used by exact-edge session-owner blob reclaim. Stores at 50
// through 56 take a creation-only migration at open.
// Version 58 adds core-owned session creation and last-commit timestamps and
// preserves their enumeration projection on permanent deletion tombstones.
// Creation-only migrations retain older stores with nullable catalog columns;
// enumeration reports zero for legacy rows whose creation time is unknowable.
// Version 59 persists per-turn cancellation requests and their undelivered
// input outcomes.
// Version 60 adds the nullable independently readable session-state generation
// beside durable session binding metadata. NULL is the version-zero legacy map.
// Version 61 removes the graph-node sequence column. Per-session generation is
// the sole durable graph ordering authority. Older stores are rejected and
// recreated; there is no migration for the removed column.
// Version 62 makes runtime append receipt identity columns all-or-none and
// removes the readerless requested-ancestor receipt column. Component-61 stores
// are rejected and recreated; there is no 61 -> 62 compatibility read or migration path.
const SCHEMA_VERSION: i32 = 62;

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
    /// The component schema is normally a reject-and-recreate boundary.
    /// Component 62 is a hard append-identity cutover from component 61. Every
    /// pre-61 graph shape also carries the retired graph-node sequence column
    /// and is refused before historical creation-only migration DDL can run.
    /// An older stamp over incompatible artifacts is refused with an
    /// inspect-and-recreate remedy; other mismatches are rejected at open.
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
mod preflight;
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
#[path = "postgres/session_blob_reclaim.rs"]
mod session_blob_reclaim;
#[path = "postgres/session_catalog.rs"]
mod session_catalog;
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
#[path = "postgres/turn_input_settlement.rs"]
mod turn_input_settlement;

pub use effect_replay::{
    PostgresEffectHost, PostgresEffectReplayOptions, PostgresRuntimeEffectController,
};
pub use preflight::PostgresStorePreflight;
use schema_shape::{
    AWAIT_EVENT_SIGNING_SECRET_BYTES, ComponentVersion, SchemaShape, read_component_version,
    read_search_path, resolve_installation, verify_schema_migration_source_shape,
    verify_schema_shape,
};
pub use schema_shape::{
    ColumnShape, ColumnValueSource, ForeignKeyAction, ForeignKeyShape, SchemaCheck, SchemaFinding,
    SchemaProvisioning, SchemaReport, UniqueGuard,
};
use {
    process_helpers::*, runtime_persistence::*, schema::*, session_factory::*, support::*,
    turn_input_settlement::*,
};

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
mod tests;
