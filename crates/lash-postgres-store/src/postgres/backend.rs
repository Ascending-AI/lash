//! [`PostgresStoreSet`]: every persistence port of one PostgreSQL database
//! (ADR 0102, D2). The durable engine persists its state through this store
//! set (ADR 0132 §1).
//!
//! PostgreSQL keeps no attachment bytes, so the store set takes an attachment
//! backend at construction: a store set with no attachment port is not one.

use std::sync::Arc;

use lash_core_execution::{AttachmentStore, Clock};

use crate::{
    PostgresLashlangArtifactStore, PostgresProcessRegistry, PostgresStorage, PostgresStore,
};

/// Every persistence port of one PostgreSQL database: the
/// [`StoreSet`](lash_core_execution::StoreSet) the durable engine persists
/// its state through.
///
/// The session-store factory declares the registry shared, because it is: the
/// registry is this database's. Cloning shares the store set.
#[derive(Clone)]
pub struct PostgresStoreSet {
    inner: Arc<StoreParts>,
}

struct StoreParts {
    storage: PostgresStorage,
    /// `postgres:<database>.<schema>`, the catalog this store set is over.
    binding: lash_core_execution::StoreBindingId,
    clock: Arc<dyn Clock>,
    /// The clock the durable store reads in place of the database's: a
    /// test's virtual clock, never set outside tests.
    #[cfg(any(test, feature = "testing"))]
    durable_clock: Option<Arc<dyn Clock>>,
    session_store_factory: Arc<PostgresStore>,
    process_registry: Arc<PostgresProcessRegistry>,
    process_env_store: Arc<PostgresLashlangArtifactStore>,
    attachment_store: Arc<dyn AttachmentStore>,
}

impl PostgresStoreSet {
    /// The store set over `storage`, writing attachment bytes to
    /// `attachment_store`, on the system clock.
    pub fn new(storage: &PostgresStorage, attachment_store: Arc<dyn AttachmentStore>) -> Self {
        Self::with_clock(
            storage,
            attachment_store,
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
    }

    /// The store set over `storage` on `clock`.
    pub fn with_clock(
        storage: &PostgresStorage,
        attachment_store: Arc<dyn AttachmentStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(StoreParts {
                storage: storage.clone(),
                binding: lash_core_execution::StoreBindingId::new(format!(
                    "postgres:{}",
                    storage.catalog_id()
                )),
                session_store_factory: Arc::new(storage.store().with_clock(Arc::clone(&clock))),
                process_registry: Arc::new(
                    storage.process_registry().with_clock(Arc::clone(&clock)),
                ),
                process_env_store: Arc::new(
                    storage.process_env_store().with_clock(Arc::clone(&clock)),
                ),
                attachment_store,
                clock,
                #[cfg(any(test, feature = "testing"))]
                durable_clock: None,
            }),
        }
    }

    /// The store set over `storage` with every port on `clock`, its durable
    /// store included: a test moves time instead of waiting for it.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_clock_for_testing(
        storage: &PostgresStorage,
        attachment_store: Arc<dyn AttachmentStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let mut set = Self::with_clock(storage, attachment_store, Arc::clone(&clock));
        if let Some(parts) = Arc::get_mut(&mut set.inner) {
            parts.durable_clock = Some(clock);
        }
        set
    }

    /// The storage every port of this store set runs over.
    pub fn storage(&self) -> &PostgresStorage {
        &self.inner.storage
    }

    /// The factory every session of this store set is created and reopened
    /// through.
    pub fn session_store_factory(&self) -> Arc<PostgresStore> {
        Arc::clone(&self.inner.session_store_factory)
    }

    /// The process registry, in the same database as the sessions.
    pub fn process_registry(&self) -> Arc<PostgresProcessRegistry> {
        Arc::clone(&self.inner.process_registry)
    }

    /// The store that serves process execution environments and Lashlang
    /// artifacts.
    pub fn process_env_store(&self) -> Arc<PostgresLashlangArtifactStore> {
        Arc::clone(&self.inner.process_env_store)
    }

    /// The attachment backend this store set was built with.
    pub fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        Arc::clone(&self.inner.attachment_store)
    }
}

impl lash_core_execution::StoreSet for PostgresStoreSet {
    fn durable_store(&self) -> Arc<dyn lash_durable::DurableStore> {
        let store = self.inner.storage.durable_store();
        #[cfg(any(test, feature = "testing"))]
        if let Some(clock) = &self.inner.durable_clock {
            return Arc::new(store.with_clock_for_testing(Arc::clone(clock)));
        }
        Arc::new(store)
    }

    fn node_wakes(&self) -> Option<Arc<dyn lash_durable::NodeWakes>> {
        Some(Arc::new(self.inner.storage.node_wakes()))
    }

    fn binding_identity(&self) -> &lash_core_execution::StoreBindingId {
        &self.inner.binding
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::DeploymentStore> {
        PostgresStoreSet::session_store_factory(self)
    }
    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::AttachmentReferrers> {
        PostgresStoreSet::session_store_factory(self)
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        PostgresStoreSet::process_registry(self)
    }

    fn tool_material_store(&self) -> Arc<dyn lash_core_execution::store::ToolMaterialStore> {
        PostgresStoreSet::process_env_store(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        PostgresStoreSet::process_env_store(self)
    }

    fn turn_prelude_store(&self) -> Arc<dyn lash_core_execution::TurnPreludeStore> {
        PostgresStoreSet::process_env_store(self)
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        PostgresStoreSet::attachment_store(self)
    }

    /// The store that keeps the process execution environments keeps the
    /// Lashlang module artifacts too.
    fn module_artifacts(&self) -> Arc<dyn lash_core_execution::ModuleArtifactStore> {
        PostgresStoreSet::process_env_store(self)
    }

    /// Definition descriptors live beside the modules and environments their
    /// manifests name, so one transaction holds a whole closure.
    fn definition_store(&self) -> Arc<dyn lash_core_execution::ProcessDefinitionStore> {
        PostgresStoreSet::process_env_store(self)
    }

    fn recovery_leader(&self) -> Arc<dyn lash_core_execution::store::RecoveryLeaderStore> {
        Arc::new(crate::recovery_leader::PostgresRecoveryLeader::new(
            self.inner.storage.pool().clone(),
            self.inner.storage.fence.clone(),
        ))
    }

    fn obligation_ledger(
        &self,
        kind: lash_core_execution::store::ObligationKind,
    ) -> Arc<dyn lash_core_execution::store::ObligationLedger> {
        self.inner.storage.obligation_ledger(kind)
    }

    fn artifact_cleanup(&self) -> Arc<dyn lash_core_execution::store::ArtifactCleanupLedger> {
        self.inner.storage.artifact_cleanup()
    }
}

impl std::fmt::Debug for PostgresStoreSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresStoreSet")
            .finish_non_exhaustive()
    }
}
