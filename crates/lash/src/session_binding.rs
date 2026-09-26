use crate::support::{
    Arc, EffectHost, ProcessWorkWiring, RuntimeEnvironment, RuntimePersistence,
    SessionStoreFactory, SessionWorkEngine,
};
use lash_sansio::SessionId;

/// Immutable owner-issued capabilities for one successfully opened session.
///
/// Construction stays inside the facade open/materialize paths. The store,
/// effect host, process/queue ports, backend, and catalog are captured
/// together from the core's one backend so later per-session operations
/// cannot independently consult a core override.
#[derive(Clone)]
pub(crate) struct BoundSession {
    session_id: SessionId,
    store: Arc<dyn RuntimePersistence>,
    effect_host: Arc<dyn EffectHost>,
    process: ProcessWorkWiring,
    work: crate::core::HeldWork,
    /// The owner core's open sessions: the core whose driver serves `work`
    /// runs a drive on the runtime registered here.
    residents: Arc<crate::core::residents::ResidentSessions>,
    backend: lash_core::Backend,
    attachment_store: Arc<lash_core::facade_support::SessionAttachmentStore>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    process_engines: lash_core::ProcessEngineRegistry,
    catalog: Arc<dyn SessionStoreFactory>,
    /// The core's tool-child context source (FIG-3712), held for as long as
    /// the session is: the backend's host holds it weakly, and a session
    /// whose core was dropped still has children to rebuild.
    tool_child_context_source: Option<Arc<dyn lash_core::facade_support::ToolChildContextSource>>,
}

impl BoundSession {
    pub(crate) fn new(
        session_id: SessionId,
        store: Arc<dyn RuntimePersistence>,
        env: &RuntimeEnvironment,
        process: ProcessWorkWiring,
        work: crate::core::HeldWork,
        residents: Arc<crate::core::residents::ResidentSessions>,
        catalog: Arc<dyn SessionStoreFactory>,
    ) -> Self {
        Self {
            session_id,
            store,
            effect_host: Arc::clone(&env.core.control.effect_host),
            process,
            work,
            residents,
            backend: env.core.backend().clone(),
            attachment_store: Arc::clone(&env.core.durability.attachment_store),
            process_env_store: Arc::clone(&env.core.durability.process_env_store),
            process_engines: env.core.process_engines.clone(),
            catalog,
            tool_child_context_source: None,
        }
    }

    /// Keeps `source` alive for as long as this binding is.
    pub(crate) fn holding_tool_child_context_source(
        mut self,
        source: Arc<dyn lash_core::facade_support::ToolChildContextSource>,
    ) -> Self {
        self.tool_child_context_source = Some(source);
        self
    }

    /// Whether this binding keeps a tool-child context source alive.
    #[cfg(test)]
    pub(crate) fn holds_tool_child_context_source(&self) -> bool {
        self.tool_child_context_source.is_some()
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) fn store(&self) -> Arc<dyn RuntimePersistence> {
        Arc::clone(&self.store)
    }

    pub(crate) fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    pub(crate) fn process(&self) -> &ProcessWorkWiring {
        &self.process
    }

    /// The owner-issued queued-work port. The binding-derived Durable Session
    /// wakes this port, never a core-level override.
    pub(crate) fn queued(&self) -> Arc<dyn SessionWorkEngine> {
        self.work.engine() as Arc<dyn SessionWorkEngine>
    }

    /// The same port as [`queued`](Self::queued), with how a send waits on
    /// its drive.
    pub(crate) fn work(&self) -> crate::core::HeldWork {
        self.work.clone()
    }

    /// Record `handle` as this session's open runtime with the owner core,
    /// whose driver then runs the session's drives on it (FIG-3600 S5b). A
    /// resumed session keeps its owner, so a resume registers here too.
    pub(crate) fn register_resident(&self, handle: &lash_core::facade_support::RuntimeHandle) {
        self.residents
            .register(&self.session_id, handle, self.effect_host());
    }

    /// Withdraw `handle` from the owner core's open sessions and let a drive
    /// running on it stop: a close or park then owns the runtime alone.
    pub(crate) async fn release_resident(
        &self,
        handle: &lash_core::facade_support::RuntimeHandle,
    ) -> bool {
        self.residents.release(&self.session_id, handle).await
    }

    pub(crate) fn catalog(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.catalog)
    }

    pub(crate) fn administration(&self) -> lash_core::SessionAdministration {
        lash_core::SessionAdministration::new(
            self.catalog(),
            self.effect_host(),
            Some(self.process.clone()),
            Some(self.backend.trigger_store()),
            Arc::clone(&self.process_env_store),
            self.process_engines.clone(),
        )
    }

    /// Apply only lifecycle-owner services to a destination core environment.
    /// Provider, plugin, prompt, tracing, and policy configuration continue to
    /// come from the core performing resume.
    pub(crate) fn apply_owner(&self, mut env: RuntimeEnvironment) -> RuntimeEnvironment {
        env.core = env.core.with_backend(self.backend.clone());
        env.core.control.effect_host = self.effect_host();
        env.core.durability.attachment_store = Arc::clone(&self.attachment_store);
        env.core.durability.process_env_store = Arc::clone(&self.process_env_store);
        env.with_work_ports(Some(self.process.clone()), self.queued())
    }
}
