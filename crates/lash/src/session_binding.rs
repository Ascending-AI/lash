use crate::support::{
    Arc, EffectHost, ProcessWorkSubstrate, ProcessWorkWiring, QueuedWorkSubstrate,
    RuntimeEnvironment, RuntimePersistence, SessionStoreFactory,
};

/// Immutable owner-issued capabilities for one successfully opened session.
///
/// Construction stays inside the facade open/materialize paths. The store,
/// effect host, process/queue ports, trigger store, and optional catalog are
/// captured together so later per-session operations cannot independently
/// consult a core override. Backend adapters remain responsible for supplying
/// a truthful deployment composition when they wire these capabilities.
#[derive(Clone)]
pub(crate) struct BoundSession {
    session_id: String,
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn EffectHost>,
    process: Option<ProcessWorkWiring>,
    queued: Arc<dyn QueuedWorkSubstrate>,
    trigger_store: Option<Arc<dyn lash_core::TriggerStore>>,
    child_store_provider: Option<Arc<dyn SessionStoreFactory>>,
    attachment_store: Arc<lash_core::SessionAttachmentStore>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    catalog: Option<Arc<dyn SessionStoreFactory>>,
}

impl BoundSession {
    pub(crate) fn new(
        session_id: String,
        store: Arc<dyn RuntimePersistence>,
        env: &RuntimeEnvironment,
        process: Option<ProcessWorkWiring>,
        queued: Arc<dyn QueuedWorkSubstrate>,
        catalog: Option<Arc<dyn SessionStoreFactory>>,
    ) -> Self {
        Self {
            session_id,
            store,
            effect_host: Arc::clone(&env.core.control.effect_host),
            process,
            queued,
            trigger_store: env.trigger_store.clone(),
            child_store_provider: env.session_store_factory.clone(),
            attachment_store: Arc::clone(&env.core.durability.attachment_store),
            process_env_store: Arc::clone(&env.core.durability.process_env_store),
            catalog,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn store(&self) -> Arc<dyn RuntimePersistence> {
        Arc::clone(&self.store)
    }

    pub(crate) fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub(crate) fn process_work(&self) -> Option<Arc<dyn ProcessWorkSubstrate>> {
        self.process
            .as_ref()
            .map(|wiring| Arc::clone(wiring.port()))
    }

    pub(crate) fn catalog(&self) -> Option<Arc<dyn SessionStoreFactory>> {
        self.catalog.clone()
    }

    /// Apply only lifecycle-owner services to a destination core environment.
    /// Provider, plugin, prompt, tracing, and policy configuration continue to
    /// come from the core performing resume.
    pub(crate) fn apply_owner(&self, mut env: RuntimeEnvironment) -> RuntimeEnvironment {
        env.core.control.effect_host = self.effect_host();
        env.trigger_store = self.trigger_store.clone();
        env.session_store_factory = self.child_store_provider.clone();
        env.core.durability.attachment_store = Arc::clone(&self.attachment_store);
        env.core.durability.process_env_store = Arc::clone(&self.process_env_store);
        env.with_work_ports(self.process.clone(), Arc::clone(&self.queued))
    }
}
