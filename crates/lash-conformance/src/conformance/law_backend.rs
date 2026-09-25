//! The backend a conformance law runs its runtime over.
//!
//! A runtime takes every port from one [`crate::Backend`] (ADR 0102, D2). A
//! law is handed the backend under test by the embedder and runs its runtime
//! over that backend's ports. [`LawBackend`] is that backend with the ports a
//! law substitutes: a testing layer over its effect host, or its own handle on
//! the same substrate's session catalog or process registry.

use std::sync::Arc;

use lash_core::engine::BuildGeneration;

/// See the module documentation.
pub(crate) struct LawBackend {
    layered: crate::testing::runtime_helpers::LayeredBackend,
}

impl LawBackend {
    /// Every port of `backend`.
    pub(crate) fn over(backend: &crate::Backend) -> Self {
        Self {
            layered: crate::testing::runtime_helpers::LayeredBackend::over(backend.clone()),
        }
    }

    /// The backend an engine host and one store set make together: every
    /// storage port is `stores`', the effects journal on `effect_host`, and
    /// the law supplies its own process work, so the backend runs no process
    /// or queued work of its own.
    pub(crate) fn over_stores(
        stores: Arc<dyn crate::StoreSet>,
        effect_host: Arc<dyn crate::EffectHost>,
    ) -> Self {
        Self::over(&crate::Backend::new(Arc::new(HostOverStores {
            stores,
            effect_host,
        })))
    }

    /// The law's effect host in place of the backend's own: a testing layer
    /// over it, or another handle on the same substrate.
    pub(crate) fn with_effect_host(self, effect_host: Arc<dyn crate::EffectHost>) -> Self {
        Self {
            layered: self.layered.map_effect_host(|_| effect_host),
        }
    }

    /// The law's session catalog in place of the backend's own.
    pub(crate) fn with_session_store_factory(
        self,
        session_store_factory: Arc<dyn crate::SessionStoreFactory>,
    ) -> Self {
        Self {
            layered: self
                .layered
                .map_session_store_factory(|_| session_store_factory),
        }
    }

    /// The law's process registry in place of the backend's own.
    pub(crate) fn with_process_registry(
        self,
        process_registry: Arc<dyn crate::ProcessRegistry>,
    ) -> Self {
        Self {
            layered: self.layered.map_process_registry(|_| process_registry),
        }
    }

    pub(crate) fn into_backend(self) -> crate::Backend {
        self.layered.into_backend()
    }

    /// A runtime host config over this backend.
    pub(crate) fn host_config(
        self,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> crate::RuntimeHostConfig {
        crate::RuntimeHostConfig::new(self.into_backend(), commit_budget, queued_work_batching)
    }
}

/// An effect host over one store set, running no process or queued work of
/// its own.
struct HostOverStores {
    stores: Arc<dyn crate::StoreSet>,
    effect_host: Arc<dyn crate::EffectHost>,
}

impl crate::EffectEngine for HostOverStores {
    fn stores(&self) -> Arc<dyn crate::StoreSet> {
        Arc::clone(&self.stores)
    }

    fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn build_generation(&self) -> &BuildGeneration {
        // A law engine serves no Restate journals, so nothing routes it by
        // generation; a fixed value keeps the trait honest.
        static GENERATION: std::sync::OnceLock<BuildGeneration> = std::sync::OnceLock::new();
        GENERATION.get_or_init(|| BuildGeneration::for_test("host-over-stores"))
    }

    fn process_work(&self) -> Option<crate::ProcessWorkWiring> {
        None
    }

    fn queued_work(&self) -> crate::BackendQueuedWork {
        crate::BackendQueuedWork::Disabled
    }
}

/// A fresh root session store on `stores`' session catalog: where a law's
/// runtime commits, on the substrate under test.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a fresh catalog creates a fresh session"
)]
pub(crate) async fn law_session_store(
    stores: &dyn crate::StoreSet,
    session_id: &crate::SessionId,
) -> Arc<dyn crate::RuntimePersistence> {
    stores
        .session_store_factory()
        .create_store(&crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create the law's session store on the backend under test")
}

/// A backend over `stores` whose effects journal on `effect_host`, for an
/// embedder whose substrate is storage only: the law's runtime reaches every
/// storage port of `stores`, drives its own queued work in process, and runs
/// its effects on the host the embedder supplies.
pub fn backend_over(
    stores: Arc<dyn crate::StoreSet>,
    effect_host: Arc<dyn crate::EffectHost>,
) -> crate::Backend {
    LawBackend {
        layered: LawBackend::over_stores(stores, effect_host)
            .layered
            .with_queued_work(crate::BackendQueuedWork::InProcess),
    }
    .into_backend()
}

/// [`backend_over`] with the recording double as its effect host: for a
/// storage law that reaches a backend's storage ports and runs no effect.
pub fn recording_backend_over(stores: Arc<dyn crate::StoreSet>) -> crate::Backend {
    backend_over(stores, Arc::new(crate::RecordingEffectHost::default()))
}

/// The backend of a store law's runtime: a law over one session store that
/// builds a runtime only to reach the store through its facade (append, park,
/// rematerialize) and runs no effect, writes no attachment and publishes no
/// execution environment.
///
/// The law's substrate is the store it was handed, so the runtime is given no
/// second one. Its effect host is the recording double, whose controllers
/// journal nothing a store answers from; its attachment and process-exec-env
/// ports refuse every write; and a port that would name a second substrate
/// (a session catalog, a process registry, a trigger store, a
/// process-definition registry) is refused outright, so a law whose runtime
/// reaches one fails loudly instead of certifying the wrong store.
pub(crate) struct StoreLawBackend {
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<StoreLawStores>,
}

impl StoreLawBackend {
    pub(crate) fn new() -> Self {
        Self {
            effect_host: Arc::new(crate::RecordingEffectHost::default()),
            stores: Arc::new(StoreLawStores {
                binding: crate::StoreBindingId::new("conformance-store-law"),
                clock: Arc::new(crate::facade_support::SystemClock),
            }),
        }
    }

    pub(crate) fn into_backend(self) -> crate::Backend {
        crate::Backend::new(Arc::new(self))
    }

    /// A runtime host config over this backend.
    pub(crate) fn host_config(
        self,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> crate::RuntimeHostConfig {
        crate::RuntimeHostConfig::new(self.into_backend(), commit_budget, queued_work_batching)
    }
}

impl crate::EffectEngine for StoreLawBackend {
    fn stores(&self) -> Arc<dyn crate::StoreSet> {
        Arc::clone(&self.stores) as Arc<dyn crate::StoreSet>
    }

    fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn build_generation(&self) -> &BuildGeneration {
        // A store law runs no engine-served journals; a fixed value keeps the
        // trait honest.
        static GENERATION: std::sync::OnceLock<BuildGeneration> = std::sync::OnceLock::new();
        GENERATION.get_or_init(|| BuildGeneration::for_test("store-law-backend"))
    }

    fn process_work(&self) -> Option<crate::ProcessWorkWiring> {
        None
    }

    fn queued_work(&self) -> crate::BackendQueuedWork {
        crate::BackendQueuedWork::Disabled
    }
}

/// The store set of a store law's runtime: its attachment and
/// process-exec-env ports refuse every write, and a port that would name a
/// second substrate is refused outright.
struct StoreLawStores {
    binding: crate::StoreBindingId,
    clock: Arc<dyn crate::Clock>,
}

impl StoreLawStores {
    fn no_second_substrate(port: &str) -> ! {
        panic!("a store law's runtime reaches no {port}: its substrate is the store it was handed")
    }
}

impl crate::StoreSet for StoreLawStores {
    fn binding_identity(&self) -> &crate::StoreBindingId {
        &self.binding
    }

    fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn crate::SessionStoreFactory> {
        Self::no_second_substrate("session catalog")
    }

    fn process_registry(&self) -> Arc<dyn crate::ProcessRegistry> {
        Self::no_second_substrate("process registry")
    }

    fn process_continuations(&self) -> Arc<dyn crate::ProcessContinuationStore> {
        Self::no_second_substrate("process continuation store")
    }

    fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        Self::no_second_substrate("trigger store")
    }

    fn process_definition_registry(&self) -> Arc<dyn crate::ProcessDefinitionRegistry> {
        Self::no_second_substrate("process-definition registry")
    }

    fn process_env_store(&self) -> Arc<dyn crate::ProcessExecutionEnvStore> {
        Arc::new(crate::testing::UnavailableProcessExecutionEnvStore)
    }

    fn attachment_store(&self) -> Arc<dyn crate::AttachmentStore> {
        Arc::new(crate::attachments::UnavailableAttachmentStore)
    }

    fn module_artifacts(&self) -> Arc<dyn crate::ModuleArtifactStore> {
        Self::no_second_substrate("Lashlang artifact store")
    }
}
