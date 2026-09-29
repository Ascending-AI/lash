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
    BuiltinBlobProfile, SqliteAttachmentStore, SqliteDatabase, SqliteProcessDefinitionRegistry,
    SqliteProcessRegistry, SqliteStore, SqliteTriggerStore, StoreOptions,
};

/// Construction-time choices for a [`SqliteStoreSet`], which opens no effect
/// journal and so has no effect-replay options.
#[derive(Clone, Debug, Default)]
pub struct SqliteStoreSetOptions {
    /// Blob and connection policy for the durable-core catalog.
    pub store: StoreOptions,
    /// Retention and staleness bounds of the process registry's wake
    /// deliveries.
    pub wake_delivery: lash_core_execution::WakeDeliveryConfig,
    /// Where the process registry mints process ids (ADR 0107): at random,
    /// unless a test that replays committed bytes needs them deterministic.
    #[doc(hidden)]
    pub process_id_mint: lash_core_execution::ProcessIdMint,
    /// Deterministic transaction faults, installed on every session store the
    /// store set's factory opens.
    #[cfg(feature = "testing")]
    pub fault_injector: Option<crate::testing::SqliteFaultInjector>,
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
    process_definitions: Arc<SqliteProcessDefinitionRegistry>,
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
    /// The file store set under `root`, created if absent.
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

    /// A fresh named in-memory store set. SQLite caps each memdb at 1 GiB.
    pub async fn memory() -> tokio_rusqlite::Result<Self> {
        Self::memory_with_clock(system_clock()).await
    }

    /// A fresh named in-memory store set on `clock`.
    pub async fn memory_with_clock(clock: Arc<dyn Clock>) -> tokio_rusqlite::Result<Self> {
        Self::memory_with_options_and_clock(SqliteStoreSetOptions::memory(), clock).await
    }

    /// A fresh named in-memory store set with explicit options and clock.
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
            lash_core_execution::FleetFormat::writable_range(),
            #[cfg(feature = "testing")]
            None,
        )
        .await?;
        // The durable-core store opens first so its admitted `F` stamps the
        // process registry's durable payloads too (FIG-3796): one fleet row
        // governs both databases.
        let process_registry = Arc::new(
            SqliteProcessRegistry::open_at(
                &registry,
                Arc::clone(&clock),
                core.clone(),
                lash_core_execution::FleetFormatStore::fleet_format(&process_env_store),
                #[cfg(feature = "testing")]
                None,
            )
            .await?
            .with_wake_delivery_config(options.wake_delivery)
            .with_process_id_mint_for_testing(options.process_id_mint.clone()),
        );
        crate::lifecycle::attach_process_registry(
            &process_env_store.conn,
            registry.target(),
            options.store.connection_policy,
        )
        .await?;
        process_env_store.process_registry = Some(registry.target().clone());
        process_env_store.process_registry_attached = true;
        let process_env_store = Arc::new(process_env_store);
        let trigger_store =
            Arc::new(SqliteTriggerStore::open_at(&triggers, Arc::clone(&clock)).await?);
        let process_definitions =
            Arc::new(SqliteProcessDefinitionRegistry::open_at(&core, Arc::clone(&clock)).await?);
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
                process_definitions,
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

    /// The named process-definition registry, in the durable-core catalog.
    pub fn process_definition_registry(&self) -> Arc<SqliteProcessDefinitionRegistry> {
        Arc::clone(&self.inner.process_definitions)
    }

    /// The durable-core [`Store`] that serves process execution environments
    /// and Lashlang artifacts. Unbound to any session.
    pub fn process_env_store(&self) -> Arc<SqliteStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The attachment byte store over the durable-core catalog, beside the
    /// manifest its garbage collection reads.
    pub fn attachment_store(&self) -> Arc<SqliteAttachmentStore> {
        Arc::clone(&self.inner.attachment_store)
    }

    /// A new unbound [`Store`] on this store set's durable-core catalog, on
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

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        SqliteStoreSet::process_registry(self)
    }

    fn process_continuations(&self) -> Arc<dyn lash_core_execution::ProcessContinuationStore> {
        SqliteStoreSet::process_registry(self)
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        SqliteStoreSet::trigger_store(self)
    }

    fn process_definition_registry(
        &self,
    ) -> Arc<dyn lash_core_execution::ProcessDefinitionRegistry> {
        SqliteStoreSet::process_definition_registry(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
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
        // holds plans and processes, the durable core everything else.
        // Ingress spans two tables of the durable core.
        if kind == lash_core_execution::store::ObligationKind::Ingress {
            return crate::ingress_obligation::ingress_ledger(&self.inner.process_env_store.conn);
        }
        let conn = if crate::obligation_ledger::in_process_registry(kind) {
            self.inner.process_registry.conn.clone()
        } else {
            self.inner.process_env_store.conn.clone()
        };
        Arc::new(crate::obligation_ledger::SqliteObligationLedger::new(
            kind, conn,
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

    /// A memory stores's databases live exactly as long as the stores
    /// or a handle taken from it: data written through one handle is read
    /// through another after the writer is gone, and the databases disappear
    /// once the last handle drops.
    #[tokio::test]
    async fn a_memory_stores_lives_until_its_last_handle_drops() {
        let stores = SqliteStoreSet::memory()
            .await
            .expect("open the memory stores");
        let uri = stores.database_uri(SqliteDatabase::DurableCore);
        let factory = stores.session_store_factory();
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
            stores
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

    #[tokio::test]
    async fn a_store_set_opens_only_its_three_storage_databases() {
        let dir = tempfile::tempdir().expect("store-set root");
        let stores = SqliteStoreSet::open(dir.path())
            .await
            .expect("open store set");
        let root = crate::location::canonical_path(dir.path());
        for database in SqliteDatabase::ALL {
            assert!(
                root.join(database.file_name()).exists(),
                "{database:?} is created"
            );
        }
        assert!(
            !root.join("effects.db").exists(),
            "the store set must not recreate the deleted effect journal"
        );
        assert_eq!(
            lash_core_execution::StoreSet::binding_identity(&stores).as_str(),
            stores.identity()
        );
    }
}
