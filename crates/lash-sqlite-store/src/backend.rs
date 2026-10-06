//! [`SqliteStoreSet`]: every storage port of one SQLite substrate, from one
//! typed [`SqliteLocation`] (ADR 0102).
//!
//! A SQLite deployment is one database (ADR 0132 §12): a file store set
//! keeps it in the one database file its host configures, a memory store set
//! as one named `memdb` database pinned by an anchor connection. Every
//! component shares the store set's one writer connection, so a transaction
//! that writes a process-registry row, a trigger row and a session row
//! commits all of them or none. Every component is opened once and handed
//! out as a shared handle.

use std::path::Path;
use std::sync::Arc;

use lash_core_execution::Clock;

use crate::location::{DatabaseLocation, MemoryAnchors, SqliteLocation};
use crate::{
    BuiltinBlobProfile, SqliteAttachmentStore, SqliteProcessRegistry, SqliteStore,
    SqliteTriggerStore, StoreOptions,
};

/// Construction-time choices for a [`SqliteStoreSet`], which opens no effect
/// journal and so has no effect-replay options.
#[derive(Clone, Debug, Default)]
pub struct SqliteStoreSetOptions {
    /// Physical store observations; SQLite has no connection-pool wait to report.
    pub observer: lash_core_execution::facade_support::StoreObserver,
    /// Blob and connection policy for the database.
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
    /// In-store pauses, installed on the store set's process registry and on
    /// every session store its factory opens.
    #[cfg(feature = "testing")]
    pub pauses: Option<crate::testing::SqlitePauses>,
    /// Observes, pauses, crashes or fails the open-time migration at each of
    /// its steps.
    #[cfg(feature = "testing")]
    pub migration_hook: Option<crate::testing::SqliteMigrationHook>,
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

fn system_clock() -> Arc<dyn Clock> {
    Arc::new(lash_core_execution::facade_support::SystemClock)
}

impl SqliteStoreSet {
    /// The file store set in the database file at `path`, created with
    /// every table of the deployment if it is absent. A directory in the
    /// retired three-file layout refuses with
    /// [`CompatRefusal::RetiredSqliteLayout`](lash_core_execution::compat::CompatRefusal::RetiredSqliteLayout)
    /// in the error's source chain.
    pub async fn open(path: impl AsRef<Path>) -> tokio_rusqlite::Result<Self> {
        Self::open_with_clock(path, system_clock()).await
    }

    /// The file store set at `path` on `clock`.
    pub async fn open_with_clock(
        path: impl AsRef<Path>,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(path, SqliteStoreSetOptions::default(), clock).await
    }

    /// The file store set at `path` with explicit options and clock.
    pub async fn open_with_options_and_clock(
        path: impl AsRef<Path>,
        options: SqliteStoreSetOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let location = crate::location::file_location(path.as_ref(), "SqliteStoreSet")?;
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
        // A store older than this build is backed up whole and migrated
        // before any component opens it.
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
        let database = DatabaseLocation::in_backend(&location, anchors.as_ref());
        // The store's installer provisions or admits every table of the
        // database and arms the writer fence every component shares.
        let process_env_store = Arc::new(
            SqliteStore::open_at(
                &database,
                options.store,
                Arc::clone(&clock),
                lash_core_execution::FleetFormat::writable(),
                #[cfg(feature = "testing")]
                options.pauses.clone(),
            )
            .await?,
        );
        let process_registry = Arc::new(
            SqliteProcessRegistry::on_connection(
                process_env_store.conn.clone(),
                database.clone(),
                Arc::clone(&clock),
            )
            .with_wake_delivery_config(options.wake_delivery)
            .with_process_id_mint_for_testing(options.process_id_mint.clone()),
        );
        let trigger_store = Arc::new(SqliteTriggerStore::on_connection(
            process_env_store.conn.clone(),
            database,
            Arc::clone(&clock),
        ));
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

    /// Where this store set's database is.
    pub fn location(&self) -> &SqliteLocation {
        &self.inner.location
    }

    /// `sqlite:<canonical database path>` or `sqlite-memory:<id>`.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// The options this store set was opened with.
    pub fn options(&self) -> &SqliteStoreSetOptions {
        &self.inner.options
    }

    /// The URI a raw SQLite connection opens the database through. An
    /// inspection affordance; see [`SqliteLocation::database_uri`].
    pub fn database_uri(&self) -> String {
        self.inner.location.database_uri()
    }

    /// The factory every session of this store set is created and reopened
    /// through.
    pub fn session_store_factory(&self) -> Arc<SqliteStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The process registry, on the store set's writer connection.
    pub fn process_registry(&self) -> Arc<SqliteProcessRegistry> {
        Arc::clone(&self.inner.process_registry)
    }

    /// The trigger subscriptions and occurrences, on the store set's writer
    /// connection.
    pub fn trigger_store(&self) -> Arc<SqliteTriggerStore> {
        Arc::clone(&self.inner.trigger_store)
    }

    /// The [`SqliteStore`] that serves process execution environments and
    /// Lashlang artifacts. Unbound to any session.
    pub fn process_env_store(&self) -> Arc<SqliteStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The durability engine's store, on the set's writer connection and
    /// clock.
    pub fn durable_store(&self) -> crate::SqliteDurableStore {
        crate::SqliteDurableStore::new(
            self.inner.process_env_store.conn.clone(),
            Arc::clone(&self.inner.clock),
        )
    }

    /// The attachment byte store, beside the manifest its garbage collection
    /// reads.
    pub fn attachment_store(&self) -> Arc<SqliteAttachmentStore> {
        Arc::clone(&self.inner.attachment_store)
    }

    /// The unbound [`SqliteStore`] of this store set.
    pub async fn open_store(&self) -> tokio_rusqlite::Result<Arc<SqliteStore>> {
        Ok(Arc::clone(&self.inner.process_env_store))
    }
}

impl lash_core_execution::StoreSet for SqliteStoreSet {
    fn durable_store(&self) -> Arc<dyn lash_durable::DurableStore> {
        Arc::new(SqliteStoreSet::durable_store(self))
    }

    fn durable_signals(&self) -> Option<Arc<dyn lash_durable::Signals>> {
        None
    }

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

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        SqliteStoreSet::process_env_store(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        SqliteStoreSet::process_env_store(self)
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
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

    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        let conn = self.inner.process_env_store.conn.clone();
        match kind {
            // Ingress spans two tables.
            lash_core_execution::store::ObligationKind::Ingress => {
                crate::ingress_obligation::ingress_ledger(&conn)
            }
            lash_core_execution::store::ObligationKind::ArtifactCleanup => Arc::new(
                crate::obligation_ledger::SqliteArtifactCleanupLedger::new(conn),
            ),
            kind => Arc::new(crate::obligation_ledger::SqliteObligationLedger::new(
                kind, conn,
            )),
        }
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        Arc::new(crate::obligation_ledger::SqliteArtifactCleanupLedger::new(
            self.inner.process_env_store.conn.clone(),
        ))
    }

    fn session_delete_ledger(
        &self,
    ) -> Arc<dyn lash_core_execution::store::session_delete::SessionDeleteLedger> {
        Arc::new(
            crate::session_delete_ledger::SqliteSessionDeleteLedger::new(
                self.inner.process_env_store.conn.clone(),
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

    /// A SQLite deployment is one database file (FIG-5195): a directory in
    /// the retired three-file layout is refused typed, naming the layout's
    /// files it holds, and the open changes nothing in it.
    #[tokio::test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture: compare host database bytes before and after a refused open"
    )]
    async fn a_directory_in_the_retired_layout_is_refused_unchanged() {
        let root = tempfile::tempdir().expect("store root");
        let mut held = Vec::new();
        for file in ["durable-core.db", "process-registry.db"] {
            let path = root.path().join(file);
            rusqlite::Connection::open(&path)
                .and_then(|connection| connection.execute_batch("CREATE TABLE old (id INTEGER)"))
                .expect("a retired-layout database");
            held.push((path.clone(), std::fs::read(&path).expect("fixture bytes")));
        }

        let error = SqliteStoreSet::open(root.path())
            .await
            .expect_err("a retired-layout directory must refuse");
        let tokio_rusqlite::Error::Error(rusqlite::Error::ToSqlConversionFailure(source)) = &error
        else {
            panic!("the refusal must preserve its typed source: {error:?}");
        };
        let Some(lash_core_execution::StoreError::Incompatible {
            refusal:
                lash_core_execution::compat::CompatRefusal::RetiredSqliteLayout { location, files },
        }) = source.downcast_ref::<lash_core_execution::StoreError>()
        else {
            panic!("expected a retired-layout refusal: {error:?}");
        };
        assert_eq!(location, &root.path().display().to_string());
        assert_eq!(files, &["durable-core.db", "process-registry.db"]);
        for (path, before) in held {
            assert_eq!(std::fs::read(path).expect("held bytes"), before);
        }
        let mut entries: Vec<_> = std::fs::read_dir(root.path())
            .expect("list the root")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, ["durable-core.db", "process-registry.db"]);
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
        let uri = stores.database_uri();
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
