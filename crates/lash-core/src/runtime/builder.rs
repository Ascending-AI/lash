use super::host::RuntimeWork;
use crate::SessionId;
use crate::plugin::PluginSessionRequest;
use std::sync::Arc;

use crate::plugin::{PluginFactory, PluginHost, PluginSession};
use crate::{
    EmbeddedRuntimeHost, LashRuntime, PluginStack, RuntimeHostConfig, RuntimeSessionState,
    SessionError, SessionPolicy,
};

enum PluginSource {
    Host(PluginHost),
    Session(Arc<PluginSession>),
}

pub struct EmbeddedRuntimeBuilder {
    runtime_lease_owner: crate::LeaseOwnerIdentity,
    session_id: Option<SessionId>,
    policy: Option<SessionPolicy>,
    initial_state: Option<RuntimeSessionState>,
    plugin_source: PluginSource,
    core: RuntimeHostConfig,
    store: Option<crate::store::SessionStore>,
    attachment_referrers_store: Option<Arc<dyn crate::store::RuntimeStore>>,
    // Keep the work wiring off the async build frame.
    work: Box<RuntimeWork>,
}

impl EmbeddedRuntimeBuilder {
    /// A builder over `core`, whose one backend supplies every store port
    /// and the effect host the runtime runs on (ADR 0102, D2). There is no
    /// in-memory default: a runtime cannot be built without a backend.
    pub fn new(core: RuntimeHostConfig, runtime_lease_owner: crate::LeaseOwnerIdentity) -> Self {
        Self {
            runtime_lease_owner,
            session_id: None,
            policy: None,
            initial_state: None,
            plugin_source: PluginSource::Host(PluginHost::empty()),
            core,
            store: None,
            attachment_referrers_store: None,
            work: Box::new(RuntimeWork::sessions_only(Arc::new(
                crate::NoSessionWork::new(),
            ))),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = Some(policy);
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

    /// The host's models: the registry that mints model bindings and binds
    /// recorded ones to their transports.
    pub fn with_llm_profiles(mut self, models: Arc<dyn crate::LlmProfiles>) -> Self {
        self.core.providers.models = models;
        self
    }

    pub fn with_store(mut self, store: crate::store::SessionStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_attachment_referrers_store(
        mut self,
        store: Arc<dyn crate::store::RuntimeStore>,
    ) -> Self {
        // Runtime state still uses `self.store`; only attachment intent
        // persistence is redirected to this store.
        self.attachment_referrers_store = Some(store);
        self
    }

    pub fn with_process_work(mut self, wiring: crate::ProcessWorkWiring) -> Self {
        self.work = Box::new((*self.work).with_process_wiring(wiring));
        self
    }

    pub fn with_queued_work(mut self, queued: Arc<dyn crate::SessionWorkEngine>) -> Self {
        self.work = Box::new((*self.work).with_queued(queued));
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

    /// The state this builder runs, and whether it is a new session's: one
    /// neither supplied nor loaded from the store.
    async fn resolve_state(&self) -> Result<(RuntimeSessionState, bool), SessionError> {
        if let Some(state) = &self.initial_state {
            return Ok((
                {
                    let mut state = state.clone();
                    if let Some(session_id) = &self.session_id {
                        state.session_id = session_id.clone();
                    }
                    state
                },
                false,
            ));
        }
        if let Some(store) = &self.store {
            // The view names its session; a builder session id that
            // disagrees is refused below, before anything is adopted.
            if let Some(state) = crate::store::load_session_window_state(
                store,
                crate::store::WindowSelector::Current,
            )
            .await
            .map_err(|source| SessionError::Store {
                context: "failed to admit and load store".to_string(),
                source,
            })?
            .map(|loaded| loaded.state)
            {
                if let Some(session_id) = &self.session_id
                    && state.session_id != session_id
                {
                    return Err(SessionError::Protocol(format!(
                        "store is bound to session `{}` but builder requested `{session_id}`",
                        state.session_id
                    )));
                }
                return Ok((state, false));
            }
            let mut state = self.resolve_state_from_defaults()?;
            if let Some(policy) = &self.policy {
                state.policy = policy.clone();
            }
            return Ok((state, true));
        }
        Ok((self.resolve_state_from_defaults()?, true))
    }

    fn resolve_plugins(
        &self,
        state: &RuntimeSessionState,
        parent_session_id: Option<SessionId>,
    ) -> Result<Arc<PluginSession>, SessionError> {
        match &self.plugin_source {
            PluginSource::Session(session) => Ok(Arc::clone(session)),
            PluginSource::Host(host) => host
                .clone()
                .isolated_registry()
                .build_session(PluginSessionRequest {
                    parent_session_id,
                    ..PluginSessionRequest::creation(
                        state.session_id.clone(),
                        crate::plugin::SessionAuthorityContext {
                            tool_access: state.authority.tool_access.clone(),
                            subagent: state.authority.subagent.clone(),
                            plugin_config: state.admitted_plugin_config(),
                        },
                    )
                })
                .map_err(SessionError::Plugin),
        }
    }

    pub async fn build(self) -> Result<LashRuntime, SessionError> {
        let (mut state, created) = self.resolve_state().await?;
        if created {
            crate::CoreConfigOwner::validate_charge_safety(&state.policy.charge_safety)
                .map_err(crate::CoreConfigOwner::creation_refusal)
                .map_err(SessionError::SessionConfigRefused)?;
        }
        let parent_session_id =
            super::lifecycle::recorded_parent_session_id(self.store.as_ref()).await?;
        let is_root_session = parent_session_id.is_none();
        let plugins = self.resolve_plugins(&state, parent_session_id)?;
        state.authority.plugin_config = plugins
            .host()
            .decode_config(&state.authority.plugin_config)?;
        if created {
            // A new session records what every installed owner resolves for
            // it, under the protocol its plugins registered (FIG-4379).
            state.authority.plugin_config = plugins.host().resolve_creation_plugin_config(
                Some(plugins.protocol_plugin_id()),
                &crate::PluginOptions::default(),
                None,
                is_root_session,
            )?;
            plugins.publish_plugin_config(state.admitted_plugin_config())?;
        }
        let mut persistence = super::lifecycle::RuntimePersistenceBindings::new(self.store);
        if let Some(manifest_store) = self.attachment_referrers_store {
            persistence = persistence.with_attachment_referrers_store(manifest_store);
        }
        let embedded_host = EmbeddedRuntimeHost::new(self.core);
        // `assemble_runtime` owns the (store, registry) wiring + residency so the
        // worker rebuild cannot drift from the live open path.
        LashRuntime::assemble_runtime(
            state.policy.clone(),
            embedded_host,
            plugins,
            persistence,
            *self.work,
            super::lifecycle::RuntimeSessionAssembly::new(state, self.runtime_lease_owner),
        )
        .await
    }
}

impl LashRuntime {
    /// A builder over `core` and its one backend; see
    /// [`EmbeddedRuntimeBuilder::new`].
    pub fn builder(
        core: RuntimeHostConfig,
        runtime_lease_owner: crate::LeaseOwnerIdentity,
    ) -> EmbeddedRuntimeBuilder {
        EmbeddedRuntimeBuilder::new(core, runtime_lease_owner)
    }
}
