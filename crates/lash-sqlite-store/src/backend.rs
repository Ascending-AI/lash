//! [`SqliteBackend`]: every persistence port and the effect host of one
//! SQLite substrate, from one typed [`SqliteLocation`] (ADR 0102).
//!
//! A file backend keeps its four databases under one root directory; a
//! memory backend keeps them as named `memdb` databases pinned by anchor
//! connections. Both run the same store, the same replay driver and the same
//! locking rules: the location decides only where each database is and what
//! the backend is called. Every component is opened once, bound to the
//! others by the location rather than by a later path exchange, and handed out
//! as a shared handle.

use std::path::Path;
use std::sync::Arc;

use lash_core_execution::{Clock, ExecutionScope};

use crate::location::{DatabaseLocation, MemoryAnchors, SqliteLocation};
use crate::{
    BuiltinBlobProfile, SqliteAttachmentStore, SqliteDatabase, SqliteEffectHost,
    SqliteEffectReplayOptions, SqliteProcessDefinitionRegistry, SqliteProcessRegistry,
    SqliteRuntimeEffectController, SqliteSessionStoreFactory, SqliteTriggerStore, Store,
    StoreOptions,
};

/// Construction-time choices for a [`SqliteBackend`].
#[derive(Clone, Debug, Default)]
pub struct SqliteBackendOptions {
    /// Blob and connection policy for the durable-core catalog.
    pub store: StoreOptions,
    /// Lease timing and drain budget of the effect host.
    pub effect_replay: SqliteEffectReplayOptions,
    /// Retention and staleness bounds of the process registry's wake
    /// deliveries.
    pub wake_delivery: lash_core_execution::WakeDeliveryConfig,
    /// Deterministic transaction faults, installed on every session store the
    /// backend's factory opens (FIG-2971: production builds do not compile
    /// the hook).
    #[cfg(feature = "testing")]
    pub fault_injector: Option<crate::testing::SqliteFaultInjector>,
}

impl SqliteBackendOptions {
    /// The options [`SqliteBackend::memory`] uses: uncompressed blobs,
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

/// One SQLite substrate: the session-store factory, the effect host, the
/// process registry, the trigger store, the process-definition registry, the
/// process-exec-env store and the attachment store, all bound to one
/// [`SqliteLocation`] and keyed on its one identity.
///
/// Cloning shares the backend. A memory backend's databases live until
/// the backend and every handle taken from it have dropped.
#[derive(Clone)]
pub struct SqliteBackend {
    inner: Arc<BackendParts>,
}

struct BackendParts {
    location: SqliteLocation,
    /// The backend's binding identity: the one value every component's
    /// identity is taken from, the effect host's turn-control binding
    /// included.
    identity: Arc<str>,
    anchors: Option<Arc<MemoryAnchors>>,
    options: SqliteBackendOptions,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<SqliteSessionStoreFactory>,
    effect_host: Arc<SqliteEffectHost>,
    process_registry: Arc<SqliteProcessRegistry>,
    trigger_store: Arc<SqliteTriggerStore>,
    process_definitions: Arc<SqliteProcessDefinitionRegistry>,
    process_env_store: Arc<Store>,
    attachment_store: Arc<SqliteAttachmentStore>,
}

impl SqliteBackend {
    /// The file backend under `root`, created if absent.
    pub async fn open(root: impl AsRef<Path>) -> tokio_rusqlite::Result<Self> {
        Self::open_with_options_and_clock(
            root,
            SqliteBackendOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    /// The file backend under `root` with explicit options and clock.
    #[expect(
        clippy::disallowed_methods,
        reason = "a file backend creates the host-supplied root before naming its databases (FIG-2971)"
    )]
    pub async fn open_with_options_and_clock(
        root: impl AsRef<Path>,
        options: SqliteBackendOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let root = root.as_ref();
        crate::location::validate_file_database_path(root, "SqliteBackend")?;
        std::fs::create_dir_all(root).map_err(|error| {
            tokio_rusqlite::Error::Error(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!(
                    "SqliteBackend could not create its root {}: {error}",
                    root.display()
                )),
            ))
        })?;
        let location = SqliteLocation::File {
            root: crate::location::canonical_path(root),
        };
        Self::assemble(location, None, options, clock).await
    }

    /// A fresh named in-memory backend.
    ///
    /// Each of its four databases is a `memdb` database, which SQLite caps at
    /// 1 GiB (`SQLITE_MEMDB_DEFAULT_MAXSIZE` in the bundled build); a write
    /// past the cap fails with `SQLITE_FULL`. Attachment bytes live in the
    /// durable-core database beside the sessions, so a memory backend's
    /// sessions, checkpoints and attachments share that one gibibyte. A
    /// workload that needs more belongs on a file backend.
    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(
            SqliteBackendOptions::memory(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
        .await
    }

    /// A fresh named in-memory backend on `clock`.
    pub async fn memory_with_clock(clock: Arc<dyn Clock>) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(SqliteBackendOptions::memory(), clock).await
    }

    /// A fresh named in-memory backend with explicit options and clock.
    pub async fn memory_with_options_and_clock(
        options: SqliteBackendOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let location = SqliteLocation::fresh_memory();
        let anchors = MemoryAnchors::pin(&location).map_err(tokio_rusqlite::Error::Error)?;
        Self::assemble(location, Some(anchors), options, clock).await
    }

    /// Fresh handles on this backend's databases: every component opened
    /// again, over the same location, identity, options and clock — what a
    /// second process over a file root is, and what a second runtime over the
    /// same memory backend is.
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
        options: SqliteBackendOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        Self::assemble(
            self.inner.location.clone(),
            self.inner.anchors.clone(),
            options,
            clock,
        )
        .await
    }

    async fn assemble(
        location: SqliteLocation,
        anchors: Option<Arc<MemoryAnchors>>,
        options: SqliteBackendOptions,
        clock: Arc<dyn Clock>,
    ) -> tokio_rusqlite::Result<Self> {
        let identity: Arc<str> = Arc::from(location.identity());
        let database = |database| {
            DatabaseLocation::in_backend(&location, &identity, database, anchors.as_ref())
        };
        let core = database(SqliteDatabase::DurableCore);
        let registry = database(SqliteDatabase::ProcessRegistry);
        let triggers = database(SqliteDatabase::Triggers);
        let journal = database(SqliteDatabase::EffectReplay);

        let process_registry = Arc::new(
            SqliteProcessRegistry::open_at(
                &registry,
                Arc::clone(&clock),
                core.clone(),
                #[cfg(feature = "testing")]
                None,
            )
            .await?
            .with_wake_delivery_config(options.wake_delivery),
        );
        let trigger_store =
            Arc::new(SqliteTriggerStore::open_at(&triggers, Arc::clone(&clock)).await?);
        let process_definitions =
            Arc::new(SqliteProcessDefinitionRegistry::open_at(&core, Arc::clone(&clock)).await?);
        let process_env_store = Arc::new(
            Store::open_at(
                &core,
                options.store,
                Arc::clone(&clock),
                None,
                None,
                #[cfg(feature = "testing")]
                None,
            )
            .await?,
        );
        let attachment_store = Arc::new(SqliteAttachmentStore::for_store(&process_env_store));
        let effect_host = Arc::new(
            SqliteEffectHost::open_at(&journal, options.effect_replay.clone(), Arc::clone(&clock))
                .await?,
        );
        effect_host.attach_process_registry(registry.target().clone());
        let factory = SqliteSessionStoreFactory::at(
            core,
            Some(registry.target().clone()),
            Some(journal.clone()),
            options.store,
            Arc::clone(&clock),
        );
        #[cfg(feature = "testing")]
        let factory = match options.fault_injector.clone() {
            Some(injector) => factory.with_fault_injector(injector),
            None => factory,
        };
        Ok(Self {
            inner: Arc::new(BackendParts {
                identity,
                location,
                anchors,
                options,
                clock,
                session_store_factory: Arc::new(factory),
                effect_host,
                process_registry,
                trigger_store,
                process_definitions,
                process_env_store,
                attachment_store,
            }),
        })
    }

    /// Where this backend's databases are.
    pub fn location(&self) -> &SqliteLocation {
        &self.inner.location
    }

    /// The options this backend was opened with.
    pub fn options(&self) -> &SqliteBackendOptions {
        &self.inner.options
    }

    /// `sqlite:<canonical effect-replay.db path>` or `sqlite-memory:<id>`;
    /// see [`SqliteLocation::identity`].
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// The URI a raw SQLite connection opens `database` through. An
    /// inspection affordance; see [`SqliteLocation::database_uri`].
    pub fn database_uri(&self, database: SqliteDatabase) -> String {
        self.inner.location.database_uri(database)
    }

    /// The factory every session of this backend is created and reopened
    /// through, over the durable-core catalog.
    pub fn session_store_factory(&self) -> Arc<SqliteSessionStoreFactory> {
        Arc::clone(&self.inner.session_store_factory)
    }

    /// The host that journals this backend's effects, with its process
    /// scope fences kept in the backend's registry.
    pub fn effect_host(&self) -> Arc<SqliteEffectHost> {
        Arc::clone(&self.inner.effect_host)
    }

    /// The process registry, pruning process-owned sessions out of the
    /// backend's own catalog.
    pub fn process_registry(&self) -> Arc<SqliteProcessRegistry> {
        Arc::clone(&self.inner.process_registry)
    }

    /// The trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<SqliteTriggerStore> {
        Arc::clone(&self.inner.trigger_store)
    }

    /// The named process-definition registry, in the durable-core catalog.
    pub fn process_definition_registry(&self) -> Arc<SqliteProcessDefinitionRegistry> {
        Arc::clone(&self.inner.process_definitions)
    }

    /// The durable-core [`Store`] that serves process execution environments
    /// and Lashlang artifacts. Unbound to any session.
    pub fn process_env_store(&self) -> Arc<Store> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The attachment byte store over the durable-core catalog, beside the
    /// manifest its garbage collection reads.
    pub fn attachment_store(&self) -> Arc<SqliteAttachmentStore> {
        Arc::clone(&self.inner.attachment_store)
    }

    /// A new unbound [`Store`] on this backend's durable-core catalog, on
    /// a connection of its own.
    pub async fn open_store(&self) -> tokio_rusqlite::Result<Store> {
        Store::open_at(
            &self.core(),
            self.inner.options.store,
            Arc::clone(&self.inner.clock),
            None,
            None,
            #[cfg(feature = "testing")]
            None,
        )
        .await
    }

    /// A controller scoped to `scope` over this backend's effect journal,
    /// on a replay driver of its own, keyed on this backend's identity.
    pub async fn open_effect_controller(
        &self,
        scope: ExecutionScope,
    ) -> tokio_rusqlite::Result<SqliteRuntimeEffectController> {
        self.inner
            .effect_host
            .open_scoped_controller(
                scope,
                self.inner.options.effect_replay.clone(),
                Arc::clone(&self.inner.clock),
            )
            .await
    }

    fn core(&self) -> DatabaseLocation {
        DatabaseLocation::in_backend(
            &self.inner.location,
            &self.inner.identity,
            SqliteDatabase::DurableCore,
            self.inner.anchors.as_ref(),
        )
    }
}

impl lash_core_execution::Backend for SqliteBackend {
    fn binding_identity(&self) -> &str {
        self.identity()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::SessionStoreFactory> {
        SqliteBackend::session_store_factory(self)
    }

    fn effect_host(&self) -> Arc<dyn lash_core_execution::EffectHost> {
        SqliteBackend::effect_host(self)
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        SqliteBackend::process_registry(self)
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        SqliteBackend::trigger_store(self)
    }

    fn process_definition_registry(
        &self,
    ) -> Arc<dyn lash_core_execution::ProcessDefinitionRegistry> {
        SqliteBackend::process_definition_registry(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        SqliteBackend::process_env_store(self)
    }

    fn attachment_store(&self) -> Arc<dyn lash_core_execution::AttachmentStore> {
        SqliteBackend::attachment_store(self)
    }
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteBackend")
            .field("location", &self.inner.location)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A memory backend's databases live exactly as long as the backend
    /// or a handle taken from it: data written through one handle is read
    /// through another after the writer is gone, and the databases disappear
    /// once the last handle drops.
    #[tokio::test]
    async fn a_memory_backend_lives_until_its_last_handle_drops() {
        let backend = SqliteBackend::memory()
            .await
            .expect("open the memory backend");
        let uri = backend.database_uri(SqliteDatabase::DurableCore);
        let factory = backend.session_store_factory();
        let request = lash_core_execution::testing::store_fixtures::session_store_request(
            &lash_core_execution::SessionId::from("memory-lifetime"),
            "memory-lifetime",
            lash_core_execution::SessionRelation::Root,
        );
        drop(
            lash_core_execution::SessionStoreFactory::create_store(factory.as_ref(), &request)
                .await
                .expect("create a session"),
        );
        let reopened = lash_core_execution::SessionStoreFactory::open_existing_store(
            backend
                .reopen()
                .await
                .expect("reopen")
                .session_store_factory()
                .as_ref(),
            &request,
        )
        .await
        .expect("reopen the session");
        assert!(
            reopened.is_some(),
            "a session written through one handle is read through a fresh one"
        );
        drop(reopened);
        assert_eq!(catalog_table_count(&uri), 1, "the catalog is alive");

        drop(factory);
        drop(backend);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while catalog_table_count(&uri) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "dropping the backend and every handle must release its databases"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A file backend keeps its four databases under its root and answers
    /// to its journal's canonical path; a memory backend answers to its id.
    #[tokio::test]
    async fn each_location_names_its_databases_and_its_identity() {
        let dir = tempfile::tempdir().expect("backend root");
        let file = SqliteBackend::open(dir.path())
            .await
            .expect("open the file backend");
        let root = crate::location::canonical_path(dir.path());
        assert_eq!(
            file.location(),
            &SqliteLocation::File { root: root.clone() }
        );
        assert_eq!(
            file.identity(),
            format!(
                "sqlite:{}",
                root.join(SqliteDatabase::EffectReplay.file_name())
                    .display()
            ),
            "a file backend answers to its journal's canonical path"
        );
        for database in SqliteDatabase::ALL {
            assert!(
                root.join(database.file_name()).exists(),
                "{database:?} is created under the root"
            );
        }
        assert_eq!(
            lash_core_execution::Backend::binding_identity(&file),
            file.identity()
        );
        assert_eq!(
            lash_core_execution::EffectHost::turn_control_binding_id(file.effect_host().as_ref()),
            file.identity(),
            "the effect host binds turn control to the backend"
        );

        let memory = SqliteBackend::memory()
            .await
            .expect("open the memory backend");
        let SqliteLocation::Memory { id } = memory.location() else {
            panic!("a memory backend has a memory location");
        };
        assert_eq!(memory.identity(), format!("sqlite-memory:{id}"));
        assert_eq!(
            lash_core_execution::EffectHost::turn_control_binding_id(memory.effect_host().as_ref()),
            memory.identity()
        );
        assert_ne!(
            memory.identity(),
            SqliteBackend::memory()
                .await
                .expect("open another memory backend")
                .identity(),
            "two memory backends are two substrates"
        );
    }
}
