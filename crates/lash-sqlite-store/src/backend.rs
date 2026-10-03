//! [`SqliteStoreSet`]: every storage port of one SQLite substrate, from one
//! typed [`SqliteLocation`] (ADR 0102).
//!
//! A file store set keeps its three databases under one root directory; a
//! memory store set keeps them as named `memdb` databases pinned by anchor
//! connections. The location decides where each database is and what the
//! store set is called. Every component is opened once and handed out as a
//! shared handle.

use std::path::Path;
use std::sync::Arc;

use lash_core_execution::Clock;

use crate::location::{DatabaseLocation, MemoryAnchors, SqliteLocation};
use crate::{
    BuiltinBlobProfile, SqliteAttachmentStore, SqliteDatabase, SqliteProcessRegistry, SqliteStore,
    SqliteTriggerStore, StoreOptions,
};

/// Construction-time choices for a [`SqliteStoreSet`], which opens no effect
/// journal and so has no effect-replay options.
#[derive(Clone, Debug, Default)]
pub struct SqliteStoreSetOptions {
    /// Physical store observations; SQLite has no connection-pool wait to report.
    pub observer: lash_core_execution::facade_support::StoreObserver,
    /// Blob and connection policy for the durable-core catalog.
    pub store: StoreOptions,
    /// Retention and staleness bounds of the process registry's wake
    /// deliveries.
    pub wake_delivery: lash_core_execution::WakeDeliveryConfig,
    /// Where the process registry mints process ids (ADR 0107): at random,
    /// unless a test that replays committed bytes needs them deterministic.
    #[doc(hidden)]
    pub process_id_mint: lash_core_execution::ProcessIdMint,
    /// Where the open-time migration backs the store up before it changes
    /// it, and how many backups it keeps ([`crate::migration`]).
    pub migration_backup: crate::SqliteMigrationBackup,
    /// In-store pauses, installed on every session store the store set's
    /// factory opens.
    #[cfg(feature = "testing")]
    pub pauses: Option<crate::testing::SqlitePauses>,
    /// Observes, pauses, crashes or fails the open-time migration at each of
    /// its steps.
    #[cfg(feature = "testing")]
    pub migration_hook: Option<crate::testing::SqliteMigrationHook>,
    /// Observes finalize's commits for crash-injection laws.
    #[cfg(feature = "testing")]
    pub finalize_hook: Option<crate::testing::SqliteFinalizeHook>,
}

impl SqliteStoreSetOptions {
    /// The options [`SqliteStoreSet::memory`] uses: uncompressed blobs,
    /// since an in-memory catalog spends CPU, not disk, on compression.
    pub fn memory() -> Self {
        Self {
            store: StoreOptions {
                blob_profile: BuiltinBlobProfile::LowLatency,
                ..StoreOptions::default()
            },
            ..Self::default()
        }
    }
}

/// Every persistence port of one SQLite substrate:
/// the [`StoreSet`](lash_core_execution::StoreSet) a Restate backend
/// journals its effects beside (ADR 0102, D2).
///
/// The effect engine owns its journal. Cloning shares the store set.
#[derive(Clone)]
pub struct SqliteStoreSet {
    inner: Arc<StoreParts>,
}

struct StoreParts {
    location: SqliteLocation,
    /// The substrate's identity: the one value every storage component's
    /// identity is taken from.
    identity: Arc<str>,
    /// [`Self::identity`] as the store set's binding identity.
    binding: lash_core_execution::StoreBindingId,
    anchors: Option<Arc<MemoryAnchors>>,
    options: SqliteStoreSetOptions,
    clock: Arc<dyn Clock>,
    process_registry: Arc<SqliteProcessRegistry>,
    trigger_store: Arc<SqliteTriggerStore>,
    process_env_store: Arc<SqliteStore>,
    attachment_store: Arc<SqliteAttachmentStore>,
    recovery_leader: Arc<crate::recovery_leader::SqliteRecoveryLeader>,
}

/// Validate and create a file root, answering its canonical location.
#[expect(
    clippy::disallowed_methods,
    reason = "a file backend creates the host-supplied root before naming its databases (FIG-2971)"
)]
pub(crate) fn file_location(
    root: &Path,
    owner: &'static str,
) -> tokio_rusqlite::Result<SqliteLocation> {
    crate::location::validate_file_database_path(root, owner)?;
    std::fs::create_dir_all(root).map_err(|error| {
        tokio_rusqlite::Error::Error(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(format!(
                "{owner} could not create its root {}: {error}",
                root.display()
            )),
        ))
    })?;
    Ok(SqliteLocation::File {
        root: crate::location::canonical_path(root),
    })
}

fn system_clock() -> Arc<dyn Clock> {
    Arc::new(lash_core_execution::facade_support::SystemClock)
}

impl SqliteStoreSet {
    /// The file store set under `root`, created if all databases are absent.
    /// A root containing only some databases refuses with
    /// [`CompatRefusal::IncompleteStoreSet`](lash_core_execution::compat::CompatRefusal::IncompleteStoreSet)
    /// in the error's source chain.
    pub async fn open(root: impl AsRef<Path>) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(root, system_clock()).await
    }

    /// The file store set under `root` on `clock`.
    pub async fn open_with_clock(
        root: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(root, SqliteStoreSetOptions::default(), clock).await
    }

    /// The file store set under `root` with explicit options and clock.
    pub async fn open_with_options_and_clock(
        root: impl AsRef<Path>,
        options: SqliteStoreSetOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let location = file_location(root.as_ref(), "SqliteStoreSet")?;
        let identity: Arc<str> = Arc::from(location.identity());
        Self::assemble(location, identity, None, options, clock).await
    }

    /// A fresh named SQLite in-memory store set. SQLite caps each memdb at 1 GiB.
    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_clock(system_clock()).await
    }

    /// A fresh named SQLite in-memory store set on `clock`.
    pub async fn memory_with_clock(clock: Arc<dyn Clock>) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(SqliteStoreSetOptions::memory(), clock).await
    }

    /// A fresh named SQLite in-memory store set with explicit options and clock.
    pub async fn memory_with_options_and_clock(
        options: SqliteStoreSetOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let location = SqliteLocation::fresh_memory();
        let anchors = MemoryAnchors::pin(&location).map_err(tokio_rusqlite::Error::Error)?;
        let identity: Arc<str> = Arc::from(location.identity());
        Self::assemble(location, identity, Some(anchors), options, clock).await
    }

    /// Fresh handles on this store set's databases: every store opened again
    /// over the same location, identity, options and clock — what a second
    /// runtime over the same store set is.
    pub async fn reopen(&self) -> tokio_rusqlite::Result<Self> {
        self.reopen_with_clock(Arc::clone(&self.inner.clock)).await
    }

    /// [`Self::reopen`] on `clock`.
    pub async fn reopen_with_clock(&self, clock: Arc<dyn Clock>) -> tokio_rusqlite::Result<Self> {
        self.reopen_with_options_and_clock(self.inner.options.clone(), clock)
            .await
    }

    /// [`Self::reopen`] with other construction-time options: another
    /// runtime over the same databases, configured differently.
    pub async fn reopen_with_options_and_clock(
        &self,
        options: SqliteStoreSetOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::assemble(
            self.inner.location.clone(),
            Arc::clone(&self.inner.identity),
            self.inner.anchors.clone(),
            options,
            clock,
        )
        .await
    }

    async fn assemble(
        location: SqliteLocation,
        identity: Arc<str>,
        anchors: Option<Arc<MemoryAnchors>>,
        options: SqliteStoreSetOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        crate::compat::check_set_files(&location).map_err(tokio_rusqlite::Error::Error)?;
        crate::finalize::recover_on_open(&location, options.store.connection_policy.busy_timeout)
            .await
            .map_err(|error| tokio_rusqlite::Error::Error(crate::sqlite_conversion_error(error)))?;
        // A store older than this build is backed up whole and migrated
        // before any component opens it; a set an interrupted migration left
        // part way is completed or restored first.
        #[cfg(feature = "testing")]
        let probe = crate::migration::Probe::hooked(options.migration_hook.clone());
        #[cfg(not(feature = "testing"))]
        let probe = crate::migration::Probe::default();
        crate::migration::migrate_on_open(
            &location,
            &options.migration_backup,
            options.store.connection_policy.busy_timeout,
            clock.as_ref(),
            probe,
        )
        .await
        .map_err(|error| tokio_rusqlite::Error::Error(crate::sqlite_conversion_error(error)))?;
        crate::compat::check_set(&location).map_err(tokio_rusqlite::Error::Error)?;
        let database = |database| {
            DatabaseLocation::in_backend(&location, &identity, database, anchors.as_ref())
        };
        let core = database(SqliteDatabase::DurableCore);
        let registry = database(SqliteDatabase::ProcessRegistry);
        let triggers = database(SqliteDatabase::Triggers);

        let mut process_env_store = SqliteStore::open_at(
            &core,
            options.store,
            Arc::clone(&clock),
            None,
            None,
            lash_core_execution::FleetFormat::writable(),
            #[cfg(feature = "testing")]
            options.pauses.clone(),
        )
        .await?;
        // Each database carries its own copy of `F`, which its writer fence
        // reads (ADR 0115 §1.2); `check_set` above refused a set whose copies
        // disagree.
        let process_registry = SqliteProcessRegistry::open_at(
            &registry,
            Arc::clone(&clock),
            #[cfg(feature = "testing")]
            None,
        )
        .await?
        .with_wake_delivery_config(options.wake_delivery)
        .with_process_id_mint_for_testing(options.process_id_mint.clone());
        crate::lifecycle::attach_process_registry(
            &process_env_store.conn,
            registry.target(),
            options.store.connection_policy,
        )
        .await?;
        process_env_store.process_registry = Some(registry.target().clone());
        // The evidence sweep reaches the trigger database on a connection of
        // its own: a cross-database write goes through that database's own
        // writer and fence, the same discipline `delete_session` uses for the
        // process registry.
        process_env_store.trigger_store = Some(triggers.target().clone());
        let process_env_store = Arc::new(process_env_store);
        let trigger_store =
            Arc::new(SqliteTriggerStore::open_at(&triggers, Arc::clone(&clock)).await?);
        // The registry reads a delivery's binding when it registers the
        // delivery's start (FIG-4369).
        let process_registry = Arc::new(
            process_registry
                .with_attached_trigger_store(&triggers)
                .await?,
        );
        let attachment_store = Arc::new(SqliteAttachmentStore::for_store(&process_env_store));
        let recovery_leader = Arc::new(crate::recovery_leader::SqliteRecoveryLeader::new(
            process_env_store.conn.clone(),
        ));
        Ok(Self {
            inner: Arc::new(StoreParts {
                binding: lash_core_execution::StoreBindingId::new(Arc::clone(&identity)),
                identity,
                location,
                anchors,
                options,
                clock,
                process_registry,
                trigger_store,
                process_env_store,
                attachment_store,
                recovery_leader,
            }),
        })
    }

    /// Where this store set's databases are.
    pub fn location(&self) -> &SqliteLocation {
        &self.inner.location
    }

    /// Finalize the release this build belongs to over this store set (ADR
    /// 0106 §2, ADR 0115 §2.2): the last step of `retired`'s drain.
    ///
    /// It is refused typed unless `retired` reads drained in this store and
    /// `registry` holds no deployment serving it. It then moves `F` in each
    /// of the three databases to this build's `F_self`, holding all three
    /// exclusively, and every writer whose writable range excludes the new
    /// `F` is fenced from its next transaction on. A crash between the three
    /// commits leaves the set partially finalized; a durable intent records
    /// the checked retirement before any commit, and a fresh open completes
    /// that authorized transition before admitting the store.
    ///
    /// A SQLite store has no operator hold: the hold stops the fleet's
    /// automatic finalize, `lashctl finalize` over PostgreSQL, and a host
    /// that owns a SQLite store finalizes exactly when it calls this.
    ///
    /// `plugins` are the finalizing build's plugin registrations (FIG-4746):
    /// the durable core's transaction that moves `F` also raises each
    /// registered plugin's writer range to its native format, so the two
    /// never disagree, and the sealed intent carries the ranges for the open
    /// that completes a crashed finalize. A registration that would change a
    /// recorded range while `F` already is this build's epoch is refused
    /// typed, and nothing changes.
    pub async fn finalize(
        &self,
        retired: &lash_core_execution::engine::BuildGeneration,
        registry: &dyn lash_core_execution::store::fleet_finalize::DeploymentRegistry,
        plugins: &[lash_core_execution::store::plugin_writers::PluginWriterRegistration],
        now_ms: u64,
    ) -> Result<
        lash_core_execution::store::fleet_finalize::FleetEpochFlip,
        lash_core_execution::store::fleet_finalize::FinalizeError,
    > {
        self.finalize_as(
            retired,
            registry,
            plugins,
            now_ms,
            lash_core_execution::FleetFormat::writable(),
        )
        .await
    }

    /// [`Self::finalize`] as a build whose writable range is `writable`.
    pub(crate) async fn finalize_as(
        &self,
        retired: &lash_core_execution::engine::BuildGeneration,
        registry: &dyn lash_core_execution::store::fleet_finalize::DeploymentRegistry,
        plugins: &[lash_core_execution::store::plugin_writers::PluginWriterRegistration],
        now_ms: u64,
        writable: lash_core_execution::compat::VersionRange,
    ) -> Result<
        lash_core_execution::store::fleet_finalize::FleetEpochFlip,
        lash_core_execution::store::fleet_finalize::FinalizeError,
    > {
        use lash_core_execution::StoreSet as _;
        use lash_core_execution::store::fleet_finalize::{FinalizeError, require_retired};
        let busy_timeout = self.inner.options.store.connection_policy.busy_timeout;
        let ownership =
            crate::store_ownership::exclusive(&self.inner.location, busy_timeout).await?;
        let drain = lash_core_execution::store::generation_drain::GenerationDrainStatus::collect(
            self.generation_drain().as_ref(),
            self.session_delete_ledger().as_ref(),
            |kind| self.obligation_ledger(kind),
            registry,
            retired,
            now_ms,
        )
        .await?;
        require_retired(&drain, registry).await?;
        let location = self.inner.location.clone();
        let plugins = plugins.to_vec();
        #[cfg(feature = "testing")]
        let hook = self.inner.options.finalize_hook.clone();
        tokio::task::spawn_blocking(move || {
            let _ownership = ownership;
            crate::finalize::finalize(&location, busy_timeout, writable, drain, &plugins, |step| {
                #[cfg(feature = "testing")]
                if let (Some(hook), crate::compat::AdvanceStep::Committed(database)) = (&hook, step)
                {
                    hook.committed(database);
                }
                #[cfg(not(feature = "testing"))]
                let _ = step;
                Ok(())
            })
            .map_err(crate::finalize::finalize_error)
        })
        .await
        .map_err(|error| {
            FinalizeError::Store(lash_core_execution::StoreError::Backend(format!(
                "the SQLite finalize task ended: {error}"
            )))
        })?
    }

    /// `sqlite:<canonical durable-core.db path>` or `sqlite-memory:<id>`.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// The options this store set was opened with.
    pub fn options(&self) -> &SqliteStoreSetOptions {
        &self.inner.options
    }

    /// The URI a raw SQLite connection opens `database` through. An
    /// inspection affordance; see [`SqliteLocation::database_uri`].
    pub fn database_uri(&self, database: SqliteDatabase) -> String {
        self.inner.location.database_uri(database)
    }

    /// The factory every session of this store set is created and reopened
    /// through, over the durable-core catalog.
    pub fn session_store_factory(&self) -> Arc<SqliteStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The process registry, pruning process-owned sessions out of the
    /// store set's own catalog.
    pub fn process_registry(&self) -> Arc<SqliteProcessRegistry> {
        Arc::clone(&self.inner.process_registry)
    }

    /// The trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<SqliteTriggerStore> {
        Arc::clone(&self.inner.trigger_store)
    }

    /// The durable-core [`SqliteStore`] that serves process execution environments
    /// and Lashlang artifacts. Unbound to any session.
    pub fn process_env_store(&self) -> Arc<SqliteStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The attachment byte store over the durable-core catalog, beside the
    /// manifest its garbage collection reads.
    pub fn attachment_store(&self) -> Arc<SqliteAttachmentStore> {
        Arc::clone(&self.inner.attachment_store)
    }

    /// A new unbound [`SqliteStore`] on this store set's durable-core catalog, on
    /// a connection of its own.
    pub async fn open_store(&self) -> tokio_rusqlite::Result<Arc<SqliteStore>> {
        Ok(Arc::clone(&self.inner.process_env_store))
    }
}

impl lash_core_execution::StoreSet for SqliteStoreSet {
    fn binding_identity(&self) -> &lash_core_execution::StoreBindingId {
        &self.inner.binding
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::DeploymentStore> {
        SqliteStoreSet::session_store_factory(self)
    }
    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::AttachmentReferrers> {
        SqliteStoreSet::session_store_factory(self)
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        SqliteStoreSet::process_registry(self)
    }

    fn process_continuations(&self) -> Arc<dyn lash_core_execution::ProcessContinuationStore> {
        SqliteStoreSet::process_registry(self)
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        SqliteStoreSet::trigger_store(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        SqliteStoreSet::process_env_store(self)
    }

    fn worker_recovery(
        &self,
    ) -> Arc<dyn lash_core_execution::store::worker_recovery::WorkerRecoveryStore> {
        SqliteStoreSet::process_env_store(self)
    }

    fn attachment_store(&self) -> Arc<dyn lash_core_execution::AttachmentStore> {
        SqliteStoreSet::attachment_store(self)
    }

    /// The durable-core store that keeps the process execution environments
    /// keeps the Lashlang module artifacts too.
    fn module_artifacts(&self) -> Arc<dyn lash_core_execution::ModuleArtifactStore> {
        SqliteStoreSet::process_env_store(self)
    }

    /// Definition descriptors live beside the modules and environments their
    /// manifests name, so one transaction holds a whole closure.
    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        SqliteStoreSet::process_env_store(self)
    }

    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        self.inner.recovery_leader.clone()
    }

    /// The drain marks and live processes live in the process registry
    /// file; the parked turns it counts, in the durable core.
    fn generation_drain(
        &self,
    ) -> Arc<dyn lash_core_execution::store::generation_drain::GenerationDrainStore> {
        Arc::new(crate::generation_drain::SqliteGenerationDrain::new(
            self.inner.process_registry.conn.clone(),
            self.inner.process_env_store.conn.clone(),
        ))
    }

    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        // Each kind's table lives in one database: the process registry file
        // holds plans and processes, the trigger store's file deliveries, the
        // durable core everything else.
        // Ingress spans two tables of the durable core.
        if kind == lash_core_execution::store::ObligationKind::Ingress {
            return crate::ingress_obligation::ingress_ledger(&self.inner.process_env_store.conn);
        }
        if kind == lash_core_execution::store::ObligationKind::ArtifactCleanup {
            return Arc::new(crate::obligation_ledger::SqliteArtifactCleanupLedger::new(
                self.inner.process_env_store.conn.clone(),
                self.inner.process_registry.conn.clone(),
            ));
        }
        let conn = if kind == lash_core_execution::store::ObligationKind::TriggerDelivery {
            self.inner.trigger_store.conn.clone()
        } else if crate::obligation_ledger::in_process_registry(kind) {
            self.inner.process_registry.conn.clone()
        } else {
            self.inner.process_env_store.conn.clone()
        };
        Arc::new(crate::obligation_ledger::SqliteObligationLedger::new(
            kind, conn,
        ))
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        Arc::new(crate::obligation_ledger::SqliteArtifactCleanupLedger::new(
            self.inner.process_env_store.conn.clone(),
            self.inner.process_registry.conn.clone(),
        ))
    }

    fn session_delete_ledger(
        &self,
    ) -> Arc<dyn lash_core_execution::store::session_delete::SessionDeleteLedger> {
        Arc::new(
            crate::session_delete_ledger::SqliteSessionDeleteLedger::new(
                self.inner.process_env_store.conn.clone(),
                self.inner.process_registry.conn.clone(),
            ),
        )
    }
}

impl std::fmt::Debug for SqliteStoreSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteStoreSet")
            .field("location", &self.inner.location)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_execution::SessionCatalogStore as _;

    #[tokio::test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture: compare host database bytes before and after a refused open"
    )]
    async fn an_incomplete_store_set_refuses_without_changing_its_databases() {
        // Exercise every one- and two-file omission at the current tier and
        // its oldest readable tier. An old partial set must not report a
        // migration that cannot run, nor provision empty replacement files.
        let mut versions = vec![
            SqliteDatabase::DurableCore.expected_version(),
            i64::from(
                lash_core_execution::compat::descriptor(SqliteDatabase::DurableCore.component())
                    .expect("core descriptor")
                    .reads
                    .min(),
            ),
        ];
        versions.dedup();
        for version in versions {
            for mask in 1..7 {
                let root = tempfile::tempdir().expect("store root");
                let mut missing = Vec::new();
                let mut surviving = Vec::new();
                for (index, database) in SqliteDatabase::ALL.into_iter().enumerate() {
                    let path = root.path().join(database.file_name());
                    if mask & (1 << index) != 0 {
                        missing.push(database);
                        continue;
                    }
                    let mut connection = rusqlite::Connection::open(&path).expect("database");
                    let tx = crate::schema::prepare_versioned_schema(&mut connection, database)
                        .expect("provision the surviving database");
                    tx.execute("UPDATE lash_compat SET version = ?1", [version])
                        .expect("select the fixture tier");
                    tx.commit().expect("commit fixture");
                    drop(connection);
                    surviving.push((path.clone(), std::fs::read(&path).expect("fixture bytes")));
                }

                let error = SqliteStoreSet::open(root.path())
                    .await
                    .expect_err("a partial set must refuse");
                let tokio_rusqlite::Error::Error(rusqlite::Error::ToSqlConversionFailure(source)) =
                    &error
                else {
                    panic!("the refusal must preserve its typed source: {error:?}");
                };
                let Some(lash_core_execution::StoreError::Incompatible {
                    refusal:
                        lash_core_execution::compat::CompatRefusal::IncompleteStoreSet {
                            missing: reported,
                            ..
                        },
                }) = source.downcast_ref::<lash_core_execution::StoreError>()
                else {
                    panic!("expected an incomplete set refusal: {error:?}");
                };
                let expected: Vec<String> = missing
                    .iter()
                    .map(|database| database.name().to_owned())
                    .collect();
                assert_eq!(reported, &expected, "fixture version {version}");
                for database in missing {
                    assert!(error.to_string().contains(database.name()), "{error}");
                    assert!(!root.path().join(database.file_name()).exists());
                }
                for (path, before) in surviving {
                    assert_eq!(std::fs::read(path).expect("surviving bytes"), before);
                }
                assert!(!root.path().join("migration-backups").exists());
            }
        }
    }

    fn catalog_table_count(uri: &str) -> i64 {
        rusqlite::Connection::open(uri)
            .expect("open a raw connection to the catalog")
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'session_head'",
                [],
                |row| row.get(0),
            )
            .expect("read the catalog")
    }

    /// A SQLite memory store set's databases live exactly as long as the stores
    /// or a handle taken from it: data written through one handle is read
    /// through another after the writer is gone, and the databases disappear
    /// once the last handle drops.
    #[tokio::test]
    async fn a_sqlite_memory_store_lives_until_its_last_handle_drops() {
        let stores = SqliteStoreSet::memory()
            .await
            .expect("open the memory stores");
        let uri = stores.database_uri(SqliteDatabase::DurableCore);
        let store = stores.open_store().await.expect("open catalog");
        let request = lash_core_execution::testing::store_fixtures::session_store_request(
            &lash_core_execution::SessionId::from("memory-lifetime"),
            "memory-lifetime",
            lash_core_execution::SessionRelation::Root,
        );
        lash_core_execution::SessionCatalogStore::admit_session(store.as_ref(), &request)
            .await
            .expect("create a session");
        drop(store);
        let reopened = stores
            .reopen()
            .await
            .expect("reopen")
            .open_store()
            .await
            .expect("open catalog")
            .lookup_session(&request.session_id)
            .await
            .expect("look up the session");
        assert!(
            matches!(reopened, lash_core_execution::SessionLookup::Live(_)),
            "a session written through one handle is read through a fresh one"
        );
        drop(reopened);
        assert_eq!(catalog_table_count(&uri), 1, "the catalog is alive");

        drop(stores);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while catalog_table_count(&uri) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "dropping the stores and every handle must release its databases"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
