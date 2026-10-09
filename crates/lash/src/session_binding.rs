use crate::support::{Arc, DeploymentStore, ProcessWorkWiring, RuntimeEnvironment};
use lash_core::ActorContext;

/// Immutable owner-issued capabilities for one successfully opened session.
///
/// Construction stays inside the facade open/materialize paths. The store,
/// effect host, process port, backend, and catalog are captured
/// together from the core's one backend so later per-session operations
/// cannot independently consult a core override.
#[derive(Clone)]
pub(crate) struct BoundSession {
    pub(crate) observer_pacing: Arc<crate::ObserverPacing>,
    observation_work_limits: lash_trace::ObservationWorkLimits,
    /// The commits the owner core's node is publishing, which the session's
    /// feed waits for before it judges a gap.
    published_heads: Arc<lash_core::runtime::durable::services::PublishedHeads>,
    store: lash_core::store::SessionStore,
    effect_host: ActorContext,
    process: ProcessWorkWiring,
    backend: lash_core::Backend,
    attachment_store: Arc<lash_core::facade_support::RuntimeAttachmentStore>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    process_engines: lash_core::ProcessEngineRegistry,
    catalog: Arc<dyn DeploymentStore>,
    models: Arc<dyn lash_core::LlmProfiles>,
    /// The owner core's telemetry adapter: what a send through this
    /// binding captures the caller's trace context from.
    tracing: lash_core::facade_support::TraceRuntime,
}

impl BoundSession {
    pub(crate) fn new(
        store: lash_core::store::SessionStore,
        env: &RuntimeEnvironment,
        process: ProcessWorkWiring,
        catalog: Arc<dyn DeploymentStore>,
        observer_pacing: Arc<crate::ObserverPacing>,
        published_heads: Arc<lash_core::runtime::durable::services::PublishedHeads>,
    ) -> Self {
        Self {
            observer_pacing,
            published_heads,
            store,
            effect_host: env.core.control.effect_host.clone(),
            observation_work_limits: env.core.observation_work_limits,
            process,
            backend: env.core.backend().clone(),
            attachment_store: Arc::clone(&env.core.durability.attachment_store),
            process_env_store: Arc::clone(&env.core.durability.process_env_store),
            process_engines: env.core.process_engines.clone(),
            catalog,
            models: Arc::clone(&env.core.providers.models),
            tracing: env.core.tracing.clone(),
        }
    }

    pub(crate) fn store(&self) -> lash_core::store::SessionStore {
        self.store.clone()
    }

    pub(crate) fn effect_host(&self) -> ActorContext {
        self.effect_host.clone()
    }

    pub(crate) fn process(&self) -> &ProcessWorkWiring {
        &self.process
    }

    pub(crate) fn catalog(&self) -> Arc<dyn DeploymentStore> {
        Arc::clone(&self.catalog)
    }

    pub(crate) fn llm_profiles(&self) -> Arc<dyn lash_core::LlmProfiles> {
        Arc::clone(&self.models)
    }

    pub(crate) fn observation_work_limits(&self) -> lash_trace::ObservationWorkLimits {
        self.observation_work_limits
    }

    pub(crate) fn published_heads(
        &self,
    ) -> Arc<lash_core::runtime::durable::services::PublishedHeads> {
        Arc::clone(&self.published_heads)
    }

    pub(crate) fn trace_scopes(&self) -> Arc<dyn lash_core::TraceScopeFactory> {
        Arc::clone(self.tracing.scopes())
    }

    pub(crate) fn administration(&self) -> lash_core::SessionAdministration {
        lash_core::SessionAdministration::new(
            self.catalog(),
            self.effect_host(),
            Some(self.process.clone()),
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
        env.with_work_ports(self.process.clone())
    }
}
