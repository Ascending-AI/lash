use std::sync::Arc;

use crate::plugin::{PluginFactory, PluginHost, PluginSession};
use crate::{
    EmbeddedRuntimeHost, LashRuntime, PluginStack, ProcessRegistry, RuntimeHostConfig,
    RuntimePersistence, RuntimeSessionState, SessionError, SessionPolicy, SessionStoreFactory,
};

enum PluginSource {
    Host(PluginHost),
    Session(Arc<PluginSession>),
}

pub struct EmbeddedRuntimeBuilder {
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    session_id: Option<String>,
    policy: Option<SessionPolicy>,
    plugin_options: crate::PluginOptions,
    initial_state: Option<RuntimeSessionState>,
    plugin_source: PluginSource,
    core: RuntimeHostConfig,
    session_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    trigger_store: Option<Arc<dyn crate::TriggerStore>>,
    store: Option<Arc<dyn RuntimePersistence>>,
    attachment_manifest_store: Option<Arc<dyn RuntimePersistence>>,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
    drivers: Box<EmbeddedRuntimeDriverBindings>,
}

/// Cold builder-only bindings live off the async build frame. Keeping this
/// optional host wiring together avoids growing every `build` caller's future
/// as new inline drivers are added.
struct EmbeddedRuntimeDriverBindings {
    process: Option<crate::ProcessWorkWiring>,
    queued: Arc<dyn crate::QueuedWorkSubstrate>,
}

impl Default for EmbeddedRuntimeDriverBindings {
    fn default() -> Self {
        Self {
            process: None,
            queued: Arc::new(crate::NoQueuedWork::new()),
        }
    }
}

impl EmbeddedRuntimeBuilder {
    /// Construct an embedded runtime builder with an explicit commit budget.
    pub fn new(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self {
            runtime_lease_owner,
            session_id: None,
            policy: None,
            plugin_options: crate::PluginOptions::default(),
            initial_state: None,
            plugin_source: PluginSource::Host(PluginHost::empty()),
            // `RuntimeHostConfig` has no `Default`; start from an explicitly
            // named in-memory core. Callers that need durable stores override
            // it with `with_runtime_host`.
            core: RuntimeHostConfig::in_memory(commit_budget, queued_work_batching),
            session_store_factory: None,
            trigger_store: Some(Arc::new(crate::InMemoryTriggerStore::default())),
            store: None,
            attachment_manifest_store: None,
            process_registry: None,
            drivers: Box::default(),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_plugin_options(mut self, plugin_options: crate::PluginOptions) -> Self {
        self.plugin_options = plugin_options;
        self
    }

    pub fn with_initial_state(mut self, state: RuntimeSessionState) -> Self {
        self.initial_state = Some(state);
        self
    }

    pub fn with_plugin_host(mut self, plugin_host: PluginHost) -> Self {
        self.plugin_source = PluginSource::Host(plugin_host);
        self
    }

    pub fn with_plugin_session(mut self, plugin_session: Arc<PluginSession>) -> Self {
        self.plugin_source = PluginSource::Session(plugin_session);
        self
    }

    pub fn with_plugin_factories(mut self, factories: Vec<Arc<dyn PluginFactory>>) -> Self {
        let host = PluginHost::new(factories);
        self.plugin_source = PluginSource::Host(host);
        self
    }

    pub fn with_plugin_stack(self, stack: PluginStack) -> Self {
        self.with_plugin_factories(stack.into_factories())
    }

    pub fn with_runtime_host(mut self, core: RuntimeHostConfig) -> Self {
        self.core = core;
        self
    }

    pub fn with_attachment_store(
        mut self,
        attachment_store: Arc<dyn crate::AttachmentStore>,
    ) -> Self {
        self.core.durability.attachment_store =
            Arc::new(crate::SessionAttachmentStore::ephemeral(attachment_store));
        self
    }

    pub fn with_prompt_template(mut self, prompt_template: crate::PromptTemplate) -> Self {
        self.core.prompt.prompt.template = Some(prompt_template);
        self
    }

    pub fn with_prompt_contribution(mut self, contribution: crate::PromptContribution) -> Self {
        self.core.prompt.prompt.add_contribution(contribution);
        self
    }

    pub fn with_replaced_prompt_slot(
        mut self,
        slot: crate::PromptSlot,
        contributions: impl IntoIterator<Item = crate::PromptContribution>,
    ) -> Self {
        self.core.prompt.prompt.replace_slot(slot, contributions);
        self
    }

    pub fn with_cleared_prompt_slot(mut self, slot: crate::PromptSlot) -> Self {
        self.core.prompt.prompt.clear_slot(slot);
        self
    }

    pub fn with_prompt_layer(mut self, prompt: crate::PromptLayer) -> Self {
        self.core.prompt.prompt = prompt;
        self
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn lash_trace::TraceSink>>) -> Self {
        self.core.tracing.trace_sink = sink;
        self
    }

    pub fn with_trace_level(mut self, level: lash_trace::TraceLevel) -> Self {
        self.core.tracing.trace_level = level;
        self
    }

    pub fn with_trace_context(mut self, context: lash_trace::TraceContext) -> Self {
        self.core.tracing.trace_context = context;
        self
    }

    pub fn with_provider_resolver(
        mut self,
        provider_resolver: Arc<dyn crate::RuntimeProviderResolver>,
    ) -> Self {
        self.core.providers.provider_resolver = provider_resolver;
        self
    }

    pub fn with_session_store_factory(
        mut self,
        session_store_factory: Arc<dyn SessionStoreFactory>,
    ) -> Self {
        self.session_store_factory = Some(session_store_factory);
        self
    }

    pub fn with_trigger_store(mut self, store: Arc<dyn crate::TriggerStore>) -> Self {
        self.trigger_store = Some(store);
        self
    }

    pub fn with_store(mut self, store: Arc<dyn RuntimePersistence>) -> Self {
        self.store = Some(store);
        self
    }

    pub(crate) fn with_attachment_manifest_store(
        mut self,
        store: Arc<dyn RuntimePersistence>,
    ) -> Self {
        // Runtime state still uses `self.store`; only attachment intent
        // persistence is redirected to this store.
        self.attachment_manifest_store = Some(store);
        self
    }

    pub fn with_process_registry(mut self, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        self.process_registry = Some(process_registry);
        self
    }

    pub fn with_process_work(mut self, wiring: crate::ProcessWorkWiring) -> Self {
        self.process_registry = Some(Arc::clone(wiring.registry()));
        self.drivers.process = Some(wiring);
        self
    }

    pub fn with_queued_work(mut self, queued: Arc<dyn crate::QueuedWorkSubstrate>) -> Self {
        self.drivers.queued = queued;
        self
    }

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.core.control.process_tool_visibility_filter = Some(filter);
        self
    }

    fn resolve_state_from_defaults(&self) -> Result<RuntimeSessionState, SessionError> {
        let policy = self.policy.clone().ok_or_else(|| {
            SessionError::Protocol(
                "embedded runtime policy is required; construct SessionPolicy with an explicit TurnBudget"
                    .to_string(),
            )
        })?;
        let mut state = self
            .initial_state
            .clone()
            .unwrap_or_else(|| RuntimeSessionState::new(policy.clone()));
        if let Some(session_id) = &self.session_id {
            state.session_id = session_id.clone();
        }
        state.policy = policy;
        Ok(state)
    }

    async fn resolve_state(&self) -> Result<RuntimeSessionState, SessionError> {
        if let Some(state) = &self.initial_state {
            return Ok({
                let mut state = state.clone();
                if let Some(session_id) = &self.session_id {
                    state.session_id = session_id.clone();
                }
                if let Some(policy) = &self.policy {
                    let recorded_provider_id = state.policy.recorded_provider_id().to_string();
                    state.policy.provider_id = recorded_provider_id;
                    state.policy.session_id = policy.session_id.clone();
                    if state.policy.model.id.trim().is_empty() {
                        state.policy.model = policy.model.clone();
                    }
                }
                state
            });
        }
        if let Some(store) = &self.store {
            let recovery_session_id = match self.session_id.clone() {
                Some(session_id) => Some(session_id),
                None => store
                    .load_session_meta()
                    .await
                    .map_err(|err| {
                        SessionError::Protocol(format!(
                            "failed to resolve store binding for admission: {err}"
                        ))
                    })?
                    .map(|meta| meta.session_id),
            };
            if let Some(recovery_session_id) = recovery_session_id
                && let Some(mut state) = crate::store::load_persisted_session_admitted(
                    store.as_ref(),
                    &recovery_session_id,
                    &self.runtime_lease_owner,
                    &uuid::Uuid::new_v4().to_string(),
                    self.core.control.lease_timings.ttl_ms(),
                )
                .await
                .map_err(|source| SessionError::Store {
                    context: "failed to admit and load store".to_string(),
                    source,
                })?
                .map(|loaded| loaded.state)
            {
                if let Some(session_id) = &self.session_id
                    && &state.session_id != session_id
                {
                    return Err(SessionError::Protocol(format!(
                        "store is bound to session `{}` but builder requested `{session_id}`",
                        state.session_id
                    )));
                }
                if let Some(policy) = &self.policy {
                    let recorded_provider_id = state.policy.recorded_provider_id().to_string();
                    state.policy.provider_id = recorded_provider_id;
                    state.policy.session_id = policy.session_id.clone();
                    if state.policy.model.id.trim().is_empty() {
                        state.policy.model = policy.model.clone();
                    }
                }
                return Ok(state);
            }
            let mut state = self.resolve_state_from_defaults()?;
            if let Some(policy) = &self.policy {
                state.policy = policy.clone();
            }
            return Ok(state);
        }
        self.resolve_state_from_defaults()
    }

    fn resolve_plugins(
        &self,
        state: &RuntimeSessionState,
    ) -> Result<Arc<PluginSession>, SessionError> {
        match &self.plugin_source {
            PluginSource::Session(session) => Ok(Arc::clone(session)),
            PluginSource::Host(host) => host
                .clone()
                .isolated_registry()
                .build_session_with_parent(
                    state.session_id.clone(),
                    None,
                    crate::plugin::SessionCreationConfig {
                        authority: crate::plugin::SessionAuthorityContext {
                            plugin_options: self.plugin_options.clone(),
                            ..crate::plugin::SessionAuthorityContext::default()
                        },
                        protocol_turn_options: state.protocol_turn_options.clone(),
                    },
                )
                .map_err(SessionError::Plugin),
        }
    }

    pub async fn build(self) -> Result<LashRuntime, SessionError> {
        let state = self.resolve_state().await?;
        let plugins = self.resolve_plugins(&state)?;
        let mut persistence = super::lifecycle::RuntimePersistenceBindings::new(self.store);
        if let Some(manifest_store) = self.attachment_manifest_store {
            persistence = persistence.with_attachment_manifest_store(manifest_store);
        }
        let embedded_host = EmbeddedRuntimeHost {
            core: self.core,
            session_store_factory: self.session_store_factory,
            trigger_store: self.trigger_store,
        };
        // `assemble_runtime` owns the (store, registry) wiring + residency so the
        // worker rebuild cannot drift from the live open path.
        let work = match (self.process_registry, self.drivers.process) {
            (Some(_), None) => {
                return Err(SessionError::Protocol(
                    "process registry requires a process-work port".to_string(),
                ));
            }
            (Some(_), Some(wiring)) => {
                super::host::RuntimeWork::processes(wiring, Arc::clone(&self.drivers.queued))
            }
            (None, Some(_)) => {
                return Err(SessionError::Protocol(
                    "process-work port requires a process registry".to_string(),
                ));
            }
            (None, None) => {
                super::host::RuntimeWork::sessions_only(Arc::clone(&self.drivers.queued))
            }
        };
        LashRuntime::assemble_runtime(
            state.policy.clone(),
            embedded_host,
            plugins,
            persistence,
            work,
            super::lifecycle::RuntimeSessionAssembly::new(
                state,
                crate::SessionRelation::Root,
                self.runtime_lease_owner,
            ),
        )
        .await
    }
}

impl LashRuntime {
    /// Construct an embedded runtime builder seeded with an in-memory host
    /// using `commit_budget`. A later
    /// [`with_runtime_host`](EmbeddedRuntimeBuilder::with_runtime_host) call
    /// replaces that host config wholesale, including its commit budget.
    pub fn builder(
        commit_budget: crate::CommitBudget,
        queued_work_batching: crate::QueuedWorkBatchingConfig,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> EmbeddedRuntimeBuilder {
        EmbeddedRuntimeBuilder::new(commit_budget, queued_work_batching, runtime_lease_owner)
    }
}
