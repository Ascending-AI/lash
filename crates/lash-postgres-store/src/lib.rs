//! PostgreSQL durable storage for Lash.
//!
//! One [`PostgresStorage`] owns the role pools of one [`PostgresHostConfig`]
//! and creates durable implementations for the runtime session store, process
//! registry, trigger store, Lashlang artifact store, process execution
//! environment store, and attachment manifest. Every connection it opens is
//! made by the role-aware factory in [`host`]: named, sized and guarded by the
//! one validated configuration (FIG-5240).
//!
//! # Who provisions the schema
//!
//! Workers never run DDL on PostgreSQL (FIG-3797). Schema is provisioned and
//! advanced by `lash migrate`, which a deployment runs before rolling workers,
//! or by the host's own tooling applying [`PostgresStorage::schema_ddl`] (the
//! same bytes committed as this crate's `schema.sql`). Copy those bytes;
//! never transcribe them.
//!
//! Open ends by reading the live catalog and comparing it against the shape
//! this build requires, so a database whose version stamp is right but whose
//! tables are not is rejected at open with a per-object diff rather than
//! failing at the first query — or silently losing a guard, which is what a
//! dropped unique index or a dropped cascade does. [`SchemaCheck`] controls
//! whether a structural mismatch is fatal. The compatibility row is a
//! separate, unconditional gate: open admits its version and reader floor
//! through the component descriptor and returns a typed refusal when needed.
//! [`PostgresStorage::verify_schema_for`] exposes the same check against a bare
//! pool so a host can gate its own migration CI on it. See ADR 0052.
//!
//! Do not run schema migrations concurrently with an open or a verification:
//! lash's advisory lock serializes only the participants that take it.
//! [`PostgresStorage::schema_advisory_lock_key`] publishes the key so a host's
//! migrations can participate.

use lash_sansio::SessionId;
mod namespace;
mod observed_sql;

use lash_core_execution::facade_support::StoreObserver;
use std::sync::Arc;

use lash_core_execution::runtime::{
    AdmissionBoundary, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
    QueuedWorkEnqueueOutcome, QueuedWorkKind, TurnLaneAdmissionPolicy,
};
use lash_core_execution::store::queued_work::{
    SESSION_COMMAND_BATCHES_PER_RUN, TurnLaneCandidate, admission_scan_limit, derive_batch_id,
    select_leading_session_command, select_turn_work_prefix,
};
use lash_core_execution::store::{
    HydratedCheckpointComponent, HydratedSessionCheckpoint, RuntimeCommit, RuntimeCommitReceipt,
    SessionCheckpoint, SessionHeadMeta, SessionHeadPayload,
};
use lash_core_execution::{
    AttachmentId, AttachmentReferrers, BlobRef, DeliveryPolicy, GcReport, ProcessAwaitOutput,
    ProcessChange, ProcessChangeCursor, ProcessEvent, ProcessEventAppendReceipt,
    ProcessEventAppendRequest, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessLiveReferenceView, ProcessObserverBy, ProcessPruneReport, ProcessRecord,
    ProcessRegistration, ProcessRegistry, ProcessStartOutcome, ProcessStarted, SessionCommitStore,
    SessionListFilter, SessionMeta, SessionNodeRecord, SessionRelationKind,
    SessionStoreCreateRequest, SessionView, StoreError, StoreMaintenance, VacuumReport,
    facade_support::ProcessStartPlan, facade_support::ProcessTransition,
    facade_support::ProcessTransitionPlan, facade_support::registry_transitions,
};
use lash_core_execution::{
    PluginError, TriggerDeliveryReservation, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
    TriggerStore, TriggerSubscriptionFilter, TriggerSubscriptionRecord,
};
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Acquire, Executor, Postgres, Row};

const SCHEMA_COMPONENT: &str = "lash-postgres-store";

/// Backend name this store reports in shared fencing diagnostics.
///
/// Every fenced write names its backend so
/// [`StoreError::FencedWriteVerdictDisagreed`](lash_core_execution::StoreError::FencedWriteVerdictDisagreed)
/// says which store's locked read and backstop predicate disagreed.
pub(crate) const POSTGRES_BACKEND: &str = "postgres";

/// Parse a stored process id: a column this store only ever wrote from a
/// minted id, so any other spelling is corrupt stored data.
pub(crate) fn stored_process_id(
    value: &str,
) -> Result<lash_sansio::ProcessId, lash_core_execution::PluginError> {
    lash_sansio::ProcessId::parse(value).map_err(|error| {
        lash_core_execution::PluginError::Session(format!("corrupt stored process id: {error}"))
    })
}

async fn acquire_runtime_connection(
    pool: &PgPool,
    observer: &StoreObserver,
) -> Result<PoolConnection<Postgres>, StoreError> {
    #[cfg(feature = "perf-witness")]
    let perf_started_at = std::time::Instant::now();
    let metrics_started_at = observer.is_observed().then(std::time::Instant::now);
    let connection = pool.acquire().await;
    if let Some(started_at) = metrics_started_at {
        observer.pool_acquire_wait(
            started_at.elapsed(),
            if connection.is_ok() {
                "success"
            } else {
                "error"
            },
        );
    }
    #[cfg(feature = "perf-witness")]
    if connection.is_ok() {
        lash_core_execution::perf_witness::record_pool_checkout_wait(perf_started_at.elapsed());
    }
    connection.map_err(store_sqlx_error)
}

/// The version `schema.sql` provisions and the shape artifact names, in the
/// stamp column's type. The number is the PostgreSQL descriptor's, declared
/// once in [`lash_core_execution::compat::POSTGRES_SCHEMA_VERSION`] beside
/// the shapes it guards.
const SCHEMA_VERSION: i32 = lash_core_execution::compat::POSTGRES_SCHEMA_VERSION as i32;

/// The oldest component schema version this build admits at open (FIG-3797).
///
/// Workers open a database whose `lash_schema_versions` stamp falls inside
/// the supported range `[MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION]`; a
/// stamp outside it is refused with a typed error naming the found version and
/// the range. For the 1.0 cut the range is the single current version — a
/// compatibility release widens the floor when it is declared, never silently.
const MIN_SUPPORTED_SCHEMA_VERSION: i32 = SCHEMA_VERSION;

#[derive(Clone)]
pub struct PostgresStorage {
    /// The work pool: every store component's connections.
    pool: PgPool,
    /// Every role pool, the work pool's handle included.
    pools: Arc<host::RolePools>,
    /// The configuration this storage runs under, with an imported pool
    /// set's real sizing.
    config: Arc<PostgresHostConfig>,
    observer: StoreObserver,
    /// The random identity of the catalog this storage opened, from
    /// `lash_catalog_identity`: what a session catalog registers under with
    /// its turn-cancel-closure owner.
    catalog_id: Arc<str>,
    /// The writer fence every handle of this storage shares (ADR 0115 §2.2):
    /// this build's writable range and the fleet epoch `F` its fences last
    /// read, seeded with the open transaction's admitted `F`.
    fence: guarded_tx::WriterFence,
}

#[derive(Clone)]
pub struct PostgresStore {
    #[cfg(any(test, feature = "testing"))]
    lease_clock_for_testing: Option<Arc<dyn lash_core_execution::Clock>>,
    pool: PgPool,
    pools: Arc<host::RolePools>,
    observer: StoreObserver,
    catalog_id: Arc<str>,
    fence: guarded_tx::WriterFence,
    clock: Arc<dyn lash_core_execution::Clock>,
    #[cfg(any(test, feature = "testing"))]
    decoded_graph_node_bodies: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(any(test, feature = "testing"))]
    decoded_turn_receipts: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    checkpoint_probe_count: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    checkpoint_write_transaction_count: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone)]
pub struct PostgresProcessRegistry {
    pool: PgPool,
    pools: Arc<host::RolePools>,
    clock: Arc<dyn lash_core_execution::Clock>,
    /// Effect hosts whose scope fence registration lifts (ADR 0049). The
    /// PostgreSQL journal's own fence rows share the pool and are cleared in
    /// the registration transaction itself.
    /// Where registration mints process ids (ADR 0107).
    process_id_mint: lash_core_execution::ProcessIdMint,
    /// The storage's writer fence: every mutation fences on `F`, and every
    /// wake-delivery and process-event payload the registry stamps goes
    /// through the fenced `F`'s `writer_version`, never a bare build constant
    /// (FIG-3796).
    fence: guarded_tx::WriterFence,
}

impl PostgresProcessRegistry {
    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Mint registered process ids from `mint` instead of at random: a fixture
    /// generator's artifacts regenerate byte-identically only when its ids do.
    #[doc(hidden)]
    pub fn with_process_id_mint_for_testing(
        mut self,
        mint: lash_core_execution::ProcessIdMint,
    ) -> Self {
        self.process_id_mint = mint;
        self
    }
}

#[derive(Clone)]
pub struct PostgresTriggerStore {
    pool: PgPool,
    fence: guarded_tx::WriterFence,
    clock: Arc<dyn lash_core_execution::Clock>,
    fixed_incarnation: Option<String>,
}

impl PostgresTriggerStore {
    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Pin otherwise-random trigger incarnation identity for durable fixture generation.
    pub fn with_incarnation_for_testing(mut self, incarnation: impl Into<String>) -> Self {
        self.fixed_incarnation = Some(incarnation.into());
        self
    }
}

#[derive(Clone)]
pub struct PostgresLashlangArtifactStore {
    pool: PgPool,
    fence: guarded_tx::WriterFence,
    /// The store set's clock: the instant a guard's cleanup is due and a
    /// referrer's fence is stamped, read on the clock the relay claims by.
    clock: Arc<dyn lash_core_execution::Clock>,
}

impl PostgresLashlangArtifactStore {
    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

impl PostgresStorage {
    /// Connect through `endpoints` under `config`.
    ///
    /// The configuration is validated before anything connects. When it
    /// declares its deployment, the server's capacity is read on the first
    /// work connection and the whole rolling budget checked before any other
    /// pool opens; every role pool is lazy. The open, schema gate included,
    /// runs within `guards.store_startup_ms`.
    ///
    /// # Errors
    ///
    /// [`PostgresHostError`]: a refused configuration or budget, a session
    /// endpoint over another catalog, a timed-out open, or the store's own
    /// refusal.
    pub async fn connect(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
        observer: StoreObserver,
    ) -> Result<Self, PostgresHostError> {
        config.validate()?;
        if config.connection.topology == ConnectionTopology::TransactionPool
            && endpoints.session().is_none()
        {
            return Err(PostgresHostError::Config(PostgresHostConfigError {
                field: "connection.topology".to_owned(),
                reason: "transaction_pool needs a session endpoint for the listener, schema, sweep, preflight and migration sessions".to_owned(),
            }));
        }
        let factory = PostgresConnectionFactory::new(endpoints.clone(), config.connection.clone());
        let pools = host::factory_pool_set(&factory, config);
        let has_session_endpoint = endpoints.session().is_some();
        let startup = config.guards.store_startup;
        tokio::time::timeout(startup, async {
            let storage = Box::pin(Self::open(
                pools,
                config.clone(),
                observer,
                lash_core_execution::FleetFormat::writable(),
            ))
            .await?;
            if has_session_endpoint {
                let session = crate::schema::read_catalog_id(&storage.pools.session)
                    .await
                    .map_err(store_sqlx_error)?
                    .ok_or_else(crate::schema::missing_catalog_identity_error)?;
                if *session != *storage.catalog_id {
                    return Err(PostgresHostError::EndpointCatalogMismatch {
                        primary: storage.catalog_id.to_string(),
                        session,
                    });
                }
            }
            Ok(storage)
        })
        .await
        .unwrap_or(Err(PostgresHostError::StartupTimedOut { after: startup }))
    }

    /// Build storage over pools the host built itself.
    ///
    /// Each pool keeps its own hooks, TLS identity and sizing, and the
    /// effective configuration records its real sizing; every transaction
    /// still begins with its role's guards, so the guard profile holds
    /// whatever the pools' own hooks set. The configuration is validated, the
    /// declared budget checked against the server, and the schema gate run,
    /// exactly as [`connect`](Self::connect) does.
    ///
    /// # Errors
    ///
    /// [`PostgresHostError`] as for [`connect`](Self::connect), and a
    /// renewal pool smaller than `roles.served_nodes`.
    pub async fn from_pool_set(
        pools: PostgresPoolSet,
        config: &PostgresHostConfig,
        observer: StoreObserver,
    ) -> Result<Self, PostgresHostError> {
        let config = host::effective_config(&pools, config)?;
        config.validate()?;
        Box::pin(Self::open(
            pools,
            config,
            observer,
            lash_core_execution::FleetFormat::writable(),
        ))
        .await
    }

    /// [`from_pool_set`](Self::from_pool_set), admitting `writable` as the
    /// opening build's fleet-format writable range.
    ///
    /// Testing seam for FIG-3796's rollout proofs: the recorded fleet-format
    /// row is read against `writable` rather than this binary's own
    /// [`lash_core_execution::FleetFormat::writable_range`], so a test can
    /// stand in for a build whose range does — or does not — still write the
    /// generation the fleet recorded. Production opens always pass this
    /// build's range.
    #[cfg(feature = "testing")]
    pub async fn from_pool_set_with_fleet_writable_range_for_testing(
        pools: PostgresPoolSet,
        config: &PostgresHostConfig,
        writable: lash_core_execution::compat::VersionRange,
    ) -> Result<Self, PostgresHostError> {
        let config = host::effective_config(&pools, config)?;
        config.validate()?;
        Box::pin(Self::open(
            pools,
            config,
            StoreObserver::default(),
            writable,
        ))
        .await
    }

    /// The validated open every constructor shares: the budget check, then
    /// the schema gate.
    async fn open(
        pools: PostgresPoolSet,
        config: PostgresHostConfig,
        observer: StoreObserver,
        writable: lash_core_execution::compat::VersionRange,
    ) -> Result<Self, PostgresHostError> {
        if let Some(deployment) = &config.deployment {
            let capacity = host::connection_capacity(&pools.work).await?;
            let per_process = config.connections_per_process().ok_or_else(|| {
                PostgresHostError::Config(PostgresHostConfigError {
                    field: "roles".to_owned(),
                    reason: "the per-process connection count overflows".to_owned(),
                })
            })?;
            deployment
                .connection_budget(per_process)
                .check(capacity)
                .map_err(PostgresHostError::Budget)?;
        }
        let pool = pools.work.clone();
        let (catalog_id, fleet_format) =
            ensure_schema(&pool, config.schema_check, writable).await?;
        let pools = Arc::new(host::RolePools::new(pools, &config));
        Ok(Self {
            pool,
            fence: guarded_tx::WriterFence::guarded(
                writable,
                fleet_format,
                pools.preludes.ordinary.clone(),
                config.retry.store,
            ),
            pools,
            config: Arc::new(config),
            observer,
            catalog_id: catalog_id.into(),
        })
    }

    /// The configuration this storage runs under: the host's, validated,
    /// with an imported pool set's real sizing.
    pub fn effective_config(&self) -> &PostgresHostConfig {
        &self.config
    }

    /// Per-role pool state, for a host's metrics.
    pub fn pool_metrics(&self) -> PostgresPoolMetrics {
        self.pools.metrics()
    }

    /// Run one phase of the schema's expand/backfill/contract discipline for
    /// the database `endpoints` reach: `lashctl migrate`'s engine (FIG-3816,
    /// FIG-3817).
    ///
    /// This is the separate operational step the open path deliberately is
    /// not. [`MigrationPhase::Expand`] takes the schema advisory lock
    /// exclusively, applies the pending expand migrations this build declares
    /// — or provisions an unprovisioned database outright — and records each
    /// applied step in the `lash_migrations` ledger.
    /// Each schema advisory-lock acquisition waits up to
    /// `maintenance.migration_lock_timeout_ms` (30 seconds by default) before
    /// returning [`StoreError::Contended`]. Migration statements retain the
    /// deployment's inherited timeouts unless
    /// `maintenance.migration_statement_timeout` sets one. See ADR 0106 §5.
    /// [`MigrationPhase::Backfill`] resumes every pending backfill from its
    /// ledger cursor and runs it to completion, and is refused typed before
    /// finalize. [`MigrationPhase::Contract`] is refused typed until finalize
    /// and every backfill it names are done. Rerunning any phase is a no-op.
    ///
    /// Workers must not call it: an open verifies the schema and never runs
    /// DDL, so a database that needs this is one that has not been provisioned
    /// or migrated yet.
    pub async fn migrate(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
        phase: MigrationPhase,
    ) -> Result<MigrationReport, MigrateError> {
        migrate::migrate(endpoints, config, phase).await
    }

    /// Plan what [`Self::migrate`] would apply without changing the database.
    ///
    /// The dry-run reads the installation under the shared advisory lock with
    /// the same snapshot discipline an open's verification uses, so its answer
    /// cannot describe a half-applied state. A backfill or contract step whose
    /// gate is closed is refused as the run would refuse it.
    pub async fn plan_migrations(
        endpoints: &PostgresEndpoints,
        config: &PostgresHostConfig,
        phase: MigrationPhase,
    ) -> Result<MigrationReport, MigrateError> {
        migrate::plan_migrations(endpoints, config, phase).await
    }

    /// Construct storage after the caller has already structurally verified this
    /// exact pool.
    ///
    /// This testing-only seam exists so the performance harness can subtract the
    /// structural catalog gate from an otherwise identical open. It still checks
    /// the unconditional component-version boundary and the catalog-identity
    /// data precondition; only structural verification is skipped.
    #[cfg(feature = "testing")]
    pub async fn from_preverified_pool_for_testing(pool: PgPool) -> Result<Self, StoreError> {
        let descriptor = lash_core_execution::compat::descriptor(
            lash_core_execution::compat::ComponentId::POSTGRES,
        )
        .ok_or_else(|| StoreError::Backend("missing PostgreSQL compatibility descriptor".into()))?;
        let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
        let writing_release = crate::release_stamp::read_release_in_tx(&mut tx).await;
        if let Some(refusal) = lash_core_execution::compat::CompatRefusal::pre_release(
            descriptor.component.as_str(),
            writing_release.as_deref(),
            crate::release_stamp::BUILD_RELEASE,
        ) {
            return Err(StoreError::Incompatible { refusal });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        lash_core_execution::compat::admit(
            descriptor,
            crate::schema::read_compat_stamp(&pool, true).await,
        )
        .map_err(|refusal| StoreError::Incompatible { refusal })?;
        let catalog_id = crate::schema::read_catalog_id(&pool)
            .await
            .map_err(store_sqlx_error)?
            .ok_or_else(crate::schema::missing_catalog_identity_error)?;
        let fleet_format = match crate::fleet_format::read(&pool).await {
            lash_core_execution::FleetFormatState::Recorded(format) => format,
            lash_core_execution::FleetFormatState::Unreadable { reason } => {
                return Err(StoreError::Backend(reason));
            }
            _ => crate::fleet_format::unrecorded(lash_core_execution::FleetFormat::writable())?,
        };
        let pools = Arc::new(host::RolePools::sharing(pool.clone()));
        Ok(Self {
            pool,
            fence: guarded_tx::WriterFence::guarded(
                lash_core_execution::FleetFormat::writable(),
                fleet_format,
                pools.preludes.ordinary.clone(),
                pools.store_retry,
            ),
            pools,
            config: Arc::new(PostgresHostConfig::default()),
            observer: StoreObserver::default(),
            catalog_id: catalog_id.into(),
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

    /// The DDL that drops every object this build provisions, committed
    /// verbatim as `crates/lash-postgres-store/teardown.sql`.
    ///
    /// It names this build's objects only. An older build's catalog can hold
    /// tables this build no longer declares (component 132 retired the effect
    /// engine's tables), so at the reject-and-recreate boundary, when
    /// [`connect`][Self::connect] refuses an incompatible component stamp, drop the
    /// schema lash owns (`DROP SCHEMA ... CASCADE`) or recreate the database
    /// rather than applying this to the older catalog.
    ///
    /// The file is generated from the same object list [`schema_ddl`][Self::schema_ddl]
    /// declares, so a future table, or a future non-table object, cannot be
    /// added to creation without being added to teardown. Every statement is
    /// `DROP TABLE IF EXISTS ... CASCADE` — idempotent, order-free, and
    /// covering the indexes, constraints, and seed rows riding on each table.
    /// Like the schema DDL, nothing is schema-qualified: it tears down
    /// whichever schema the session's `search_path` resolves.
    ///
    /// Teardown is destructive and unscoped to the `lash_` prefix by design —
    /// it drops exactly the objects lash provisions, no more and no less, so
    /// it is safe to run in a schema the host shares with other components.
    pub fn teardown_ddl() -> &'static str {
        TEARDOWN_DDL
    }

    /// The version `schema.sql` provisions: the component version its seed
    /// row stamps into `lash_schema_versions`, the migration ledger records
    /// and the shape artifact names. It is the PostgreSQL descriptor's number
    /// ([`lash_core_execution::compat::POSTGRES_SCHEMA_VERSION`]).
    pub fn schema_version() -> i32 {
        SCHEMA_VERSION
    }

    /// The oldest component version this build's `schema.sql` supersedes
    /// without a migration.
    pub fn min_supported_schema_version() -> i32 {
        MIN_SUPPORTED_SCHEMA_VERSION
    }

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
        let _session = self
            .pools
            .schema_sessions
            .acquire()
            .await
            .map_err(|_| StoreError::Backend("the schema session limit is closed".into()))?;
        Self::verify_schema_for(&self.pools.session).await
    }

    /// Constructing a [`PostgresStorage`] is strictly harder than verifying one:
    /// open additionally insists on a matching component version stamp and a
    /// catalog identity row, and either of those can be exactly what
    /// a host's migration produced wrongly. A check reachable only through a
    /// successful open could therefore not describe the databases it exists to
    /// describe, so this form needs no receiver and no successful open — it
    /// reports every version, structural, and seed-row finding, and returns
    /// them rather than failing.
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
    /// Between them that serializes everything lash does to the schema — but it cannot by
    /// itself coordinate a host migration that does not participate.
    /// A non-participating migration can commit before a verification's snapshot or after its
    /// commit, so the report describes the schema as of that snapshot rather than as of now.
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

    /// The catalog this storage opened, `<database>.<schema>`: the storage's
    /// identity.
    pub fn catalog_id(&self) -> &str {
        &self.catalog_id
    }

    pub fn session_store_factory(&self) -> PostgresStore {
        self.store()
    }

    /// The fleet format this storage's durable writers emit — the `F` of ADR
    /// 0106 §1 as its writer fences last read it, or as the open admitted it
    /// before any fence ran (ADR 0115 §2.3).
    ///
    /// This is the hook durable writers consult for their writer version:
    /// `fleet_format.writer_version(CURRENT_…)` maps a format's build-newest
    /// version onto the generation the fleet agreed to write.
    pub fn fleet_format(&self) -> lash_core_execution::FleetFormat {
        self.fence.fleet()
    }

    /// Pass every guarded transaction of this storage, through any handle it
    /// hands out, through `seam` right after its writer fence (ADR 0115 §6).
    #[cfg(any(test, feature = "testing"))]
    pub fn with_after_fence_for_testing(self, seam: testing::AfterFence) -> Self {
        self.fence.install_after_fence(seam);
        self
    }

    /// Pass every transaction of this storage that wrote a turn change,
    /// through any handle it hands out, through `seam` right before its
    /// `COMMIT`.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_before_turn_commit_for_testing(self, seam: testing::BeforeTurnCommit) -> Self {
        self.fence.install_before_turn_commit(seam);
        self
    }

    /// One multi-session store over this catalog.
    pub fn store(&self) -> PostgresStore {
        PostgresStore {
            pool: self.pool.clone(),
            pools: Arc::clone(&self.pools),
            observer: self.observer.clone(),
            catalog_id: Arc::clone(&self.catalog_id),
            fence: self.fence.clone(),
            #[cfg(any(test, feature = "testing"))]
            lease_clock_for_testing: None,
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
            #[cfg(any(test, feature = "testing"))]
            decoded_graph_node_bodies: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(any(test, feature = "testing"))]
            decoded_turn_receipts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            checkpoint_probe_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            checkpoint_write_transaction_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn process_registry(&self) -> PostgresProcessRegistry {
        PostgresProcessRegistry {
            pool: self.pool.clone(),
            pools: Arc::clone(&self.pools),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
            process_id_mint: lash_core_execution::ProcessIdMint::default(),
            fence: self.fence.clone(),
        }
    }

    pub fn trigger_store(&self) -> PostgresTriggerStore {
        PostgresTriggerStore {
            pool: self.pool.clone(),
            fence: self.fence.clone(),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
            fixed_incarnation: None,
        }
    }

    pub fn lashlang_artifact_store(&self) -> PostgresLashlangArtifactStore {
        PostgresLashlangArtifactStore {
            pool: self.pool.clone(),
            fence: self.fence.clone(),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
        }
    }

    pub fn process_env_store(&self) -> PostgresLashlangArtifactStore {
        PostgresLashlangArtifactStore {
            pool: self.pool.clone(),
            fence: self.fence.clone(),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
        }
    }

    /// The retained tool-material store (FIG-4889), over the same artifact
    /// and referrer tables.
    pub fn tool_material_store(&self) -> PostgresLashlangArtifactStore {
        PostgresLashlangArtifactStore {
            pool: self.pool.clone(),
            fence: self.fence.clone(),
            clock: Arc::new(lash_core_execution::facade_support::SystemClock),
        }
    }

    /// The durability engine's store over this catalog.
    pub fn durable_store(&self) -> PostgresDurableStore {
        PostgresDurableStore::new(
            Arc::clone(&self.pools),
            self.fence.clone(),
            self.observer.clone(),
        )
    }

    /// The durability engine's node wakes over this catalog: wake hints
    /// after commit and node liveness locks.
    pub fn node_wakes(&self) -> PostgresNodeWakes {
        PostgresNodeWakes::new(self.durable_store())
    }

    /// The store→engine delivery obligation ledger of `kind` over this
    /// catalog (ADR 0109 §1.3).
    pub fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        Arc::new(crate::obligation_ledger::PostgresObligationLedger::new(
            kind,
            self.pool.clone(),
            self.fence.clone(),
        ))
    }

    pub fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        Arc::new(crate::obligation_ledger::PostgresObligationLedger::new(
            lash_core_execution::store::ObligationKind::ArtifactCleanup,
            self.pool.clone(),
            self.fence.clone(),
        ))
    }
}

impl PostgresStore {
    pub fn new(storage: &PostgresStorage) -> Self {
        storage.store()
    }

    pub fn with_clock(mut self, clock: Arc<dyn lash_core_execution::Clock>) -> Self {
        self.clock = clock;
        self
    }
}

impl PostgresStore {
    #[cfg(test)]
    fn checkpoint_admission_counts(&self) -> (usize, usize) {
        (
            self.checkpoint_probe_count
                .load(std::sync::atomic::Ordering::Relaxed),
            self.checkpoint_write_transaction_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

#[path = "postgres/artifact_store.rs"]
mod artifact_store;
#[path = "postgres/attachments.rs"]
mod attachments;
#[path = "postgres/backend.rs"]
mod backend;
#[path = "postgres/blobs.rs"]
mod blobs;
#[path = "postgres/change_feed.rs"]
mod change_feed;
#[path = "postgres/connection_sql.rs"]
mod connection_sql;
#[path = "postgres/durable/mod.rs"]
mod durable;
#[path = "postgres/evidence_retention.rs"]
mod evidence_retention;
#[path = "postgres/fleet_format.rs"]
mod fleet_format;
#[path = "postgres/guarded_tx.rs"]
mod guarded_tx;
#[path = "postgres/migrate.rs"]
mod migrate;
#[path = "postgres/obligation_ledger.rs"]
mod obligation_ledger;
#[path = "postgres/pending_turn_inputs.rs"]
mod pending_turn_inputs;
mod preflight;
#[path = "postgres/process_helpers.rs"]
mod process_helpers;

#[path = "postgres/process_registry.rs"]
mod process_registry;
#[path = "postgres/process_sql.rs"]
mod process_sql;
#[path = "postgres/queued_work.rs"]
mod queued_work;
#[path = "postgres/recovery_leader.rs"]
mod recovery_leader;
#[path = "postgres/release_stamp.rs"]
mod release_stamp;
#[cfg(test)]
mod rendered_statement_sets_tests;
#[path = "postgres/replayable.rs"]
mod replayable;
#[path = "postgres/revisions.rs"]
mod revisions;
#[path = "postgres/runtime_persistence/mod.rs"]
mod runtime_persistence;
#[path = "postgres/schema.rs"]
mod schema;
#[cfg(test)]
#[path = "postgres/schema_compat_tests.rs"]
mod schema_compat_tests;
#[path = "postgres/schema_shape.rs"]
mod schema_shape;
#[path = "postgres/session_blob_reclaim.rs"]
mod session_blob_reclaim;
#[path = "postgres/session_catalog.rs"]
mod session_catalog;
#[path = "postgres/session_factory.rs"]
mod session_factory;
#[path = "postgres/session_ingress.rs"]
mod session_ingress;
#[path = "postgres/session_meta.rs"]
mod session_meta;
#[path = "postgres/session_runs.rs"]
mod session_runs;
#[path = "postgres/session_sql.rs"]
mod session_sql;
#[path = "postgres/support.rs"]
mod support;
#[cfg(any(test, feature = "testing"))]
#[path = "postgres/test_support.rs"]
mod test_support;
#[cfg(any(test, feature = "testing"))]
#[path = "postgres/testing.rs"]
pub mod testing;
/// Planner witnesses for the named trigger listings, and the dispatch that
/// picks them.
#[cfg(test)]
#[path = "postgres/trigger_listing_plan_tests.rs"]
mod trigger_listing_plan_tests;
#[path = "postgres/trigger_store.rs"]
mod trigger_store;
#[path = "postgres/turn_ingress.rs"]
mod turn_ingress;

pub use backend::PostgresStoreSet;
pub mod host;
pub use durable::{PostgresDurableStore, PostgresNodeWakes};
use guarded_tx::begin_guarded;
pub use host::{
    ConnectionRole, ConnectionTopology, PostgresConnectionFactory, PostgresEndpoints,
    PostgresHostConfig, PostgresHostConfigError, PostgresHostError, PostgresPoolMetrics,
    PostgresPoolSet, PostgresRolePoolMetrics, TransactionPrelude,
};
pub use migrate::{MigrateError, MigrationPhase, MigrationRefusal, MigrationReport, MigrationStep};
mod connection_budget;
pub use connection_budget::{
    PostgresConnectionBudget, PostgresConnectionBudgetRefusal, PostgresConnectionBudgetReport,
    PostgresConnectionCapacity,
};
pub use preflight::PostgresStorePreflight;
use schema_shape::verify_schema_shape;
pub use schema_shape::{
    ColumnShape, ColumnValueSource, ForeignKeyAction, ForeignKeyShape, SchemaCheck, SchemaFinding,
    SchemaReport, UniqueGuard,
};
use {pending_turn_inputs::*, process_helpers::*, queued_work::*, schema::*, support::*};

// `tests/support/mod.rs` is also compiled into this crate's unit tests (as
// `postgres_test_support`), so it can only name this crate uniformly if the
// crate answers to its own extern name.
extern crate self as lash_postgres_store;

#[cfg(test)]
#[path = "postgres/checkpoint_depth_tests.rs"]
mod checkpoint_depth_tests;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod postgres_test_support;

#[cfg(test)]
mod tests;
