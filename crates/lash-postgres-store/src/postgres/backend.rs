//! [`PostgresStoreSet`]: every persistence port of one PostgreSQL database
//! (ADR 0102, D2). PostgreSQL is storage only: a deployment journals its
//! effects on Restate, beside this store set (ADR 0104).
//!
//! PostgreSQL keeps no attachment bytes, so the store set takes an attachment
//! backend at construction: a store set with no attachment port is not one.

use std::sync::Arc;

use lash_core_execution::{AttachmentStore, Clock};

use crate::{
    PostgresLashlangArtifactStore, PostgresProcessDefinitionRegistry, PostgresProcessRegistry,
    PostgresSessionStoreFactory, PostgresStorage, PostgresTriggerStore,
};

/// Every persistence port of one PostgreSQL database: the
/// [`StoreSet`](lash_core_execution::StoreSet) a Restate backend
/// journals its effects beside.
///
/// The session-store factory declares the registry shared, because it is: the
/// registry is this database's. Cloning shares the store set.
#[derive(Clone)]
pub struct PostgresStoreSet {
    inner: Arc<StoreParts>,
}

struct StoreParts {
    storage: PostgresStorage,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<PostgresSessionStoreFactory>,
    process_registry: Arc<PostgresProcessRegistry>,
    trigger_store: Arc<PostgresTriggerStore>,
    process_definitions: Arc<PostgresProcessDefinitionRegistry>,
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
            lash_core_execution::WakeDeliveryConfig::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
    }

    /// The store set over `storage` with explicit wake-delivery bounds and
    /// clock.
    pub fn with_clock(
        storage: &PostgresStorage,
        attachment_store: Arc<dyn AttachmentStore>,
        wake_delivery: lash_core_execution::WakeDeliveryConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(StoreParts {
                storage: storage.clone(),
                session_store_factory: Arc::new(
                    storage
                        .session_store_factory_with_shared_process_registry()
                        .with_clock(Arc::clone(&clock)),
                ),
                process_registry: Arc::new(
                    storage
                        .process_registry_with_wake_delivery_config(wake_delivery)
                        .with_clock(Arc::clone(&clock)),
                ),
                trigger_store: Arc::new(storage.trigger_store().with_clock(Arc::clone(&clock))),
                process_definitions: Arc::new(
                    PostgresProcessDefinitionRegistry::with_pool(storage.pool().clone())
                        .with_clock(Arc::clone(&clock)),
                ),
                process_env_store: Arc::new(storage.process_env_store()),
                attachment_store,
                clock,
            }),
        }
    }

    /// The storage every port of this store set runs over.
    pub fn storage(&self) -> &PostgresStorage {
        &self.inner.storage
    }

    /// The factory every session of this store set is created and reopened
    /// through.
    pub fn session_store_factory(&self) -> Arc<PostgresSessionStoreFactory> {
        Arc::clone(&self.inner.session_store_factory)
    }

    /// The process registry, in the same database as the sessions.
    pub fn process_registry(&self) -> Arc<PostgresProcessRegistry> {
        Arc::clone(&self.inner.process_registry)
    }

    /// The trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<PostgresTriggerStore> {
        Arc::clone(&self.inner.trigger_store)
    }

    /// The named process-definition registry.
    pub fn process_definition_registry(&self) -> Arc<PostgresProcessDefinitionRegistry> {
        Arc::clone(&self.inner.process_definitions)
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
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::SessionStoreFactory> {
        PostgresStoreSet::session_store_factory(self)
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        PostgresStoreSet::process_registry(self)
    }

    fn process_continuations(&self) -> Arc<dyn lash_core_execution::ProcessContinuationStore> {
        PostgresStoreSet::process_registry(self)
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        PostgresStoreSet::trigger_store(self)
    }

    fn process_definition_registry(
        &self,
    ) -> Arc<dyn lash_core_execution::ProcessDefinitionRegistry> {
        PostgresStoreSet::process_definition_registry(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
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
}

impl std::fmt::Debug for PostgresStoreSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresStoreSet")
            .finish_non_exhaustive()
    }
}
