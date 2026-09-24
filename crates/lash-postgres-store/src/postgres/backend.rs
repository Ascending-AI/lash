//! [`PostgresBackend`] and [`PostgresStoreSet`]: every persistence port of
//! one PostgreSQL database, with or without its effect host (ADR 0102, D2).
//!
//! PostgreSQL keeps no attachment bytes, so both take an attachment backend at
//! construction: a backend with no attachment port is not a backend.

use std::sync::Arc;

use lash_core_execution::{AttachmentStore, Clock};

use crate::{
    PostgresEffectHost, PostgresEffectReplayOptions, PostgresLashlangArtifactStore,
    PostgresProcessDefinitionRegistry, PostgresProcessRegistry, PostgresSessionStoreFactory,
    PostgresStorage, PostgresTriggerStore,
};

/// Construction-time choices for a [`PostgresBackend`].
#[derive(Clone, Debug, Default)]
pub struct PostgresBackendOptions {
    /// Lease timing and drain budget of the effect host.
    pub effect_replay: PostgresEffectReplayOptions,
    /// Retention and staleness bounds of the process registry's wake
    /// deliveries.
    pub wake_delivery: lash_core_execution::WakeDeliveryConfig,
    /// Testing seam: admit session-store transactions on the backend's
    /// clock rather than the database's, so a test clock can lapse leases.
    #[cfg(feature = "testing")]
    pub lease_time_from_clock_for_testing: bool,
}

/// Every persistence port of one PostgreSQL database without an effect host:
/// the [`StoreSet`](lash_core_execution::StoreSet) a Restate backend
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

    /// This store set with its session-store factory admitting transactions
    /// on the store set's clock (see
    /// [`PostgresBackendOptions::lease_time_from_clock_for_testing`]).
    #[cfg(feature = "testing")]
    fn with_lease_clock_for_testing(self) -> Self {
        let parts = &self.inner;
        let session_store_factory = Arc::new(
            parts
                .storage
                .session_store_factory_with_shared_process_registry()
                .with_clock(Arc::clone(&parts.clock))
                .with_lease_clock_for_testing(Arc::clone(&parts.clock)),
        );
        Self {
            inner: Arc::new(StoreParts {
                storage: parts.storage.clone(),
                clock: Arc::clone(&parts.clock),
                session_store_factory,
                process_registry: Arc::clone(&parts.process_registry),
                trigger_store: Arc::clone(&parts.trigger_store),
                process_definitions: Arc::clone(&parts.process_definitions),
                process_env_store: Arc::clone(&parts.process_env_store),
                attachment_store: Arc::clone(&parts.attachment_store),
            }),
        }
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

/// One PostgreSQL substrate: the [`PostgresStoreSet`] and the
/// [`PostgresEffectHost`] that journals its effects in the same database.
///
/// Its binding identity is the effect host's, which is keyed on the
/// database's await-event signing secret. Cloning shares the backend.
#[derive(Clone)]
pub struct PostgresBackend {
    stores: PostgresStoreSet,
    effect_host: Arc<PostgresEffectHost>,
    identity: Arc<str>,
}

impl PostgresBackend {
    /// The backend over `storage`, writing attachment bytes to
    /// `attachment_store`, with default options on the system clock.
    pub fn new(storage: &PostgresStorage, attachment_store: Arc<dyn AttachmentStore>) -> Self {
        Self::with_options_and_clock(
            storage,
            attachment_store,
            PostgresBackendOptions::default(),
            Arc::new(lash_core_execution::facade_support::SystemClock),
        )
    }

    /// The backend over `storage` with explicit options and clock.
    ///
    /// PostgreSQL stays authoritative for lease timestamps and comparisons;
    /// the clock drives effect sleeps, busy backoff, renewal cadence and
    /// store-side stamps.
    pub fn with_options_and_clock(
        storage: &PostgresStorage,
        attachment_store: Arc<dyn AttachmentStore>,
        options: PostgresBackendOptions,
        clock: Arc<dyn Clock>,
    ) -> Self {
        #[cfg_attr(not(feature = "testing"), allow(unused_mut))]
        let mut stores = PostgresStoreSet::with_clock(
            storage,
            attachment_store,
            options.wake_delivery,
            Arc::clone(&clock),
        );
        #[cfg(feature = "testing")]
        if options.lease_time_from_clock_for_testing {
            stores = stores.with_lease_clock_for_testing();
        }
        let effect_host = Arc::new(PostgresEffectHost::with_options_and_clock(
            storage,
            options.effect_replay,
            clock,
        ));
        let identity = Arc::from(lash_core_execution::EffectHost::turn_control_binding_id(
            effect_host.as_ref(),
        ));
        Self {
            stores,
            effect_host,
            identity,
        }
    }

    /// The store set this backend's effect host journals beside.
    pub fn stores(&self) -> &PostgresStoreSet {
        &self.stores
    }

    /// The storage every port of this backend runs over.
    pub fn storage(&self) -> &PostgresStorage {
        self.stores.storage()
    }

    /// The host that journals this backend's effects.
    pub fn effect_host(&self) -> Arc<PostgresEffectHost> {
        Arc::clone(&self.effect_host)
    }

    /// The factory every session of this backend is created and reopened
    /// through.
    pub fn session_store_factory(&self) -> Arc<PostgresSessionStoreFactory> {
        self.stores.session_store_factory()
    }

    /// The process registry.
    pub fn process_registry(&self) -> Arc<PostgresProcessRegistry> {
        self.stores.process_registry()
    }

    /// The trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<PostgresTriggerStore> {
        self.stores.trigger_store()
    }

    /// The named process-definition registry.
    pub fn process_definition_registry(&self) -> Arc<PostgresProcessDefinitionRegistry> {
        self.stores.process_definition_registry()
    }

    /// The store that serves process execution environments and Lashlang
    /// artifacts.
    pub fn process_env_store(&self) -> Arc<PostgresLashlangArtifactStore> {
        self.stores.process_env_store()
    }

    /// The attachment backend this backend was built with.
    pub fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.stores.attachment_store()
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
}

impl lash_core_execution::Backend for PostgresBackend {
    fn binding_identity(&self) -> &str {
        &self.identity
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.stores.inner.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core_execution::SessionStoreFactory> {
        PostgresBackend::session_store_factory(self)
    }

    fn effect_host(&self) -> Arc<dyn lash_core_execution::EffectHost> {
        PostgresBackend::effect_host(self)
    }

    fn process_registry(&self) -> Arc<dyn lash_core_execution::ProcessRegistry> {
        PostgresBackend::process_registry(self)
    }

    fn trigger_store(&self) -> Arc<dyn lash_core_execution::TriggerStore> {
        PostgresBackend::trigger_store(self)
    }

    fn process_definition_registry(
        &self,
    ) -> Arc<dyn lash_core_execution::ProcessDefinitionRegistry> {
        PostgresBackend::process_definition_registry(self)
    }

    fn process_env_store(&self) -> Arc<dyn lash_core_execution::ProcessExecutionEnvStore> {
        PostgresBackend::process_env_store(self)
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        PostgresBackend::attachment_store(self)
    }

    /// The runtime's in-process worker drives this backend's registry.
    fn process_work(&self) -> Option<lash_core_execution::ProcessWorkWiring> {
        None
    }

    fn queued_work(&self) -> lash_core_execution::BackendQueuedWork {
        lash_core_execution::BackendQueuedWork::InProcess
    }
}

/// The store that keeps this backend's process execution environments keeps
/// its Lashlang module artifacts too.
#[cfg(feature = "lashlang")]
impl lashlang::LashlangArtifactBackend for PostgresBackend {
    fn lashlang_artifact_store(&self) -> Arc<dyn lashlang::LashlangArtifactStore> {
        PostgresBackend::process_env_store(self)
    }
}

#[cfg(feature = "lashlang")]
impl lashlang::LashlangArtifactStoreSet for PostgresStoreSet {
    fn lashlang_artifact_store(&self) -> Arc<dyn lashlang::LashlangArtifactStore> {
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

impl std::fmt::Debug for PostgresBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresBackend")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
