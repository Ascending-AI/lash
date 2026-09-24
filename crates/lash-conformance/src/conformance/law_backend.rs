//! The backend a conformance law runs its runtime over.
//!
//! A runtime takes every port from one [`crate::Backend`] (ADR 0102, D2). A
//! law here is handed the ports it certifies — a session store, a process
//! registry, an effect host — by the embedder, and runs the rest of the
//! runtime in process around them. [`LawBackend`] is that one backend: the
//! law's ports, and the in-process ports for everything the law does not
//! certify.

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
}

impl LawBackend {
    /// Every port in process.
    pub(crate) fn in_process() -> Self {
        let clock: Arc<dyn crate::Clock> = Arc::new(crate::facade_support::SystemClock);
        Self::with_effect_host_ports(
            Arc::new(crate::facade_support::NativeEffectHost::default()),
            clock,
        )
    }

    fn with_effect_host_ports(
        effect_host: Arc<dyn crate::EffectHost>,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            binding_identity: effect_host.turn_control_binding_id(),
            session_store_factory: Arc::new(
                crate::facade_support::InMemorySessionStoreFactory::new(),
            ),
            process_registry: Arc::new(crate::TestLocalProcessRegistry::default()),
            trigger_store: Arc::new(crate::facade_support::InMemoryTriggerStore::with_clock(
                Arc::clone(&clock),
            )),
            process_definitions: Arc::new(crate::InMemoryProcessDefinitionRegistry::with_clock(
                Arc::clone(&clock),
            )),
            process_env_store: Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
            attachment_store: Arc::new(crate::facade_support::InMemoryAttachmentStore::new()),
            effect_host,
            clock,
        }
    }

    /// The law's effect host in place of the in-process one: the backend's
    /// binding is that host's.
    pub(crate) fn with_effect_host(self, effect_host: Arc<dyn crate::EffectHost>) -> Self {
        Self {
            binding_identity: effect_host.turn_control_binding_id(),
            effect_host,
            ..self
        }
    }

    /// The law's session catalog in place of the in-process one.
    pub(crate) fn with_session_store_factory(
        self,
        session_store_factory: Arc<dyn crate::SessionStoreFactory>,
    ) -> Self {
        Self {
            session_store_factory,
            ..self
        }
    }

    /// The law's process registry in place of the in-process one.
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

    /// The in-process worker drives the law's registry.
    fn process_work(&self) -> Option<crate::ProcessWorkWiring> {
        None
    }

    fn queued_work(&self) -> crate::BackendQueuedWork {
        crate::BackendQueuedWork::InProcess
    }
}
