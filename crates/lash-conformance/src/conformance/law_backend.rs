//! The backend a conformance law runs its runtime over.
//!
//! A runtime takes every port from one [`crate::Backend`] (ADR 0102, D2). A
//! law is handed the backend under test by the embedder and runs its runtime
//! over that backend's ports. [`LawBackend`] is that backend with the ports a
//! law substitutes: a testing layer over its effect host, or its own handle on
//! the same substrate's session catalog or process registry.

use std::sync::Arc;

/// See the module documentation.
pub(crate) struct LawBackend {
    binding_identity: String,
    clock: Arc<dyn crate::Clock>,
    session_store_factory: Arc<dyn crate::SessionStoreFactory>,
    effect_host: Arc<dyn crate::EffectHost>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_definitions: Arc<dyn crate::ProcessDefinitionRegistry>,
    process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn crate::AttachmentStore>,
    module_artifacts: Arc<dyn crate::ModuleArtifactStore>,
    process_work: Option<crate::ProcessWorkWiring>,
    queued_work: crate::BackendQueuedWork,
}

impl LawBackend {
    /// Every port of `backend`.
    pub(crate) fn over(backend: &dyn crate::Backend) -> Self {
        Self {
            binding_identity: backend.binding_identity().to_string(),
            clock: backend.clock(),
            session_store_factory: backend.session_store_factory(),
            effect_host: backend.effect_host(),
            process_registry: backend.process_registry(),
            trigger_store: backend.trigger_store(),
            process_definitions: backend.process_definition_registry(),
            process_env_store: backend.process_env_store(),
            attachment_store: backend.attachment_store(),
            module_artifacts: backend.module_artifacts(),
            process_work: backend.process_work(),
            queued_work: backend.queued_work(),
        }
    }

    /// The backend an engine host and one store set make together: every
    /// storage port is `stores`', the effects journal on `effect_host`, and
    /// the law supplies its own process work, so the backend runs no process
    /// or queued work of its own.
    pub(crate) fn over_stores(
        stores: &dyn crate::StoreSet,
        effect_host: Arc<dyn crate::EffectHost>,
    ) -> Self {
        Self {
            binding_identity: effect_host.turn_control_binding_id(),
            clock: stores.clock(),
            session_store_factory: stores.session_store_factory(),
            effect_host,
            process_registry: stores.process_registry(),
            trigger_store: stores.trigger_store(),
            process_definitions: stores.process_definition_registry(),
            process_env_store: stores.process_env_store(),
            attachment_store: stores.attachment_store(),
            module_artifacts: stores.module_artifacts(),
            process_work: None,
            queued_work: crate::BackendQueuedWork::Disabled,
        }
    }

    /// The law's effect host in place of the backend's own: a testing layer
    /// over it, or another handle on the same substrate. The binding is that
    /// host's.
    pub(crate) fn with_effect_host(self, effect_host: Arc<dyn crate::EffectHost>) -> Self {
        Self {
            binding_identity: effect_host.turn_control_binding_id(),
            effect_host,
            ..self
        }
    }

    /// The law's session catalog in place of the backend's own.
    pub(crate) fn with_session_store_factory(
        self,
        session_store_factory: Arc<dyn crate::SessionStoreFactory>,
    ) -> Self {
        Self {
            session_store_factory,
            ..self
        }
    }

    /// The law's process registry in place of the backend's own.
    pub(crate) fn with_process_registry(
        self,
        process_registry: Arc<dyn crate::ProcessRegistry>,
    ) -> Self {
        Self {
            process_registry,
            ..self
        }
    }

    pub(crate) fn into_backend(self) -> Arc<dyn crate::Backend> {
        Arc::new(self)
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

impl crate::Backend for LawBackend {
    fn binding_identity(&self) -> &str {
        &self.binding_identity
    }

    fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn crate::SessionStoreFactory> {
        Arc::clone(&self.session_store_factory)
    }

    fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_registry(&self) -> Arc<dyn crate::ProcessRegistry> {
        Arc::clone(&self.process_registry)
    }

    fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        Arc::clone(&self.trigger_store)
    }

    fn process_definition_registry(&self) -> Arc<dyn crate::ProcessDefinitionRegistry> {
        Arc::clone(&self.process_definitions)
    }

    fn process_env_store(&self) -> Arc<dyn crate::ProcessExecutionEnvStore> {
        Arc::clone(&self.process_env_store)
    }

    fn attachment_store(&self) -> Arc<dyn crate::AttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    fn module_artifacts(&self) -> Arc<dyn crate::ModuleArtifactStore> {
        Arc::clone(&self.module_artifacts)
    }

    fn process_work(&self) -> Option<crate::ProcessWorkWiring> {
        self.process_work.clone()
    }

    fn queued_work(&self) -> crate::BackendQueuedWork {
        self.queued_work.clone()
    }
}

/// A backend over `stores` whose effects journal on `effect_host`, for an
/// embedder whose substrate is storage only: the law's runtime reaches every
/// storage port of `stores`, drives its own queued work in process, and runs
/// its effects on the host the embedder supplies.
pub fn backend_over(
    stores: &dyn crate::StoreSet,
    effect_host: Arc<dyn crate::EffectHost>,
) -> Arc<dyn crate::Backend> {
    Arc::new(LawBackend {
        queued_work: crate::BackendQueuedWork::InProcess,
        ..LawBackend::over_stores(stores, effect_host)
    })
}

/// A backend over `stores` whose effect host is the recording double: for an
/// embedder's storage law that reaches a backend's storage ports and runs no
/// effect, over a substrate that is storage only.
pub fn recording_backend_over(stores: &dyn crate::StoreSet) -> Arc<dyn crate::Backend> {
    Arc::new(LawBackend::over_stores(
        stores,
        Arc::new(crate::RecordingEffectHost::default()),
    ))
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
    clock: Arc<dyn crate::Clock>,
}

impl StoreLawBackend {
    pub(crate) fn new() -> Self {
        Self {
            effect_host: Arc::new(crate::RecordingEffectHost::default()),
            clock: Arc::new(crate::facade_support::SystemClock),
        }
    }

    pub(crate) fn into_backend(self) -> Arc<dyn crate::Backend> {
        Arc::new(self)
    }

    /// A runtime host config over this backend.
    pub(crate) fn host_config(
        self,
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
    ) -> crate::RuntimeHostConfig {
        crate::RuntimeHostConfig::new(self.into_backend(), commit_budget, queued_work_batching)
    }

    fn no_second_substrate(port: &str) -> ! {
        panic!("a store law's runtime reaches no {port}: its substrate is the store it was handed")
    }
}

impl crate::Backend for StoreLawBackend {
    fn binding_identity(&self) -> &str {
        "conformance-recording-effect-host"
    }

    fn clock(&self) -> Arc<dyn crate::Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn crate::SessionStoreFactory> {
        Self::no_second_substrate("session catalog")
    }

    fn effect_host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_registry(&self) -> Arc<dyn crate::ProcessRegistry> {
        Self::no_second_substrate("process registry")
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

    fn process_work(&self) -> Option<crate::ProcessWorkWiring> {
        None
    }

    fn queued_work(&self) -> crate::BackendQueuedWork {
        crate::BackendQueuedWork::Disabled
    }
}
