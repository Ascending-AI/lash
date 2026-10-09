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
    creation: Option<(SessionPolicy, crate::SessionToolAccess)>,
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
            creation: None,
            initial_state: None,
            plugin_source: PluginSource::Host(PluginHost::empty(
                core.control.execution_budgets.clone(),
                core.tracing.clone(),
            )),
            core,
            store: None,
            attachment_referrers_store: None,
            work: Box::new(RuntimeWork::sessions_only()),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// What a session this builder creates records: its policy and its
    /// tool authority. A builder that opens a recorded session, or runs a
    /// supplied state, reads both from there and ignores this.
    pub fn with_creation(
        mut self,
        policy: SessionPolicy,
        tool_access: crate::SessionToolAccess,
    ) -> Self {
        self.creation = Some((policy, tool_access));
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
        let host = PluginHost::new(
            factories,
            self.core.control.execution_budgets.clone(),
            self.core.tracing.clone(),
        );
        self.plugin_source = PluginSource::Host(host);
        self
    }

    pub fn with_plugin_stack(self, stack: PluginStack) -> Self {
        let budgets = self.core.control.execution_budgets.clone();
        let tracing = self.core.tracing.clone();
        self.with_plugin_host(stack.into_host(budgets, tracing))
    }

    pub fn with_trace_sink(mut self, sink: Option<Arc<dyn lash_trace::TraceSink>>) -> Self {
        self.core.tracing = self.core.tracing.clone().with_trace_sinks(sink);
        self
    }

    pub fn with_trace_level(mut self, level: lash_trace::TraceLevel) -> Self {
        self.core.tracing = self.core.tracing.clone().with_level(level);
        self
    }

    pub fn with_telemetry_content(mut self, content: lash_trace::TelemetryContent) -> Self {
        self.core.tracing = self.core.tracing.clone().with_content(content);
        self
    }

    pub fn with_trace_context(mut self, context: lash_trace::TraceContext) -> Self {
        self.core.tracing = self.core.tracing.clone().with_base_context(context);
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

    pub fn with_process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn crate::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.core.control.process_tool_visibility_filter = Some(filter);
        self
    }

    /// The state of a session this builder creates, under the policy and
    /// tool authority its creator stated. Creation resolves the session's
    /// plugin configuration once its plugins are built.
    fn resolve_created_state(&self) -> Result<RuntimeSessionState, SessionError> {
        let (policy, tool_access) = self.creation.clone().ok_or_else(|| {
            SessionError::Protocol(
                "embedded runtime creation is required to create a session; state its SessionPolicy and SessionToolAccess with `with_creation`"
                    .to_string(),
            )
        })?;
        let mut state = RuntimeSessionState::new(
            policy,
            crate::RuntimeSessionAuthority::new(
                tool_access,
                crate::PluginConfig::default(),
                crate::prompt_sections::PromptPlan::default(),
            ),
        );
        if let Some(session_id) = &self.session_id {
            state.session_id = session_id.clone();
        }
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
        }
        Ok((self.resolve_created_state()?, true))
    }

    fn resolve_plugins(
        &self,
        state: &RuntimeSessionState,
    ) -> Result<Arc<PluginSession>, SessionError> {
        match &self.plugin_source {
            PluginSource::Session(session) => Ok(Arc::clone(session)),
            PluginSource::Host(host) => {
                let authority = crate::plugin::SessionAuthorityContext {
                    tool_access: state.authority.tool_access.clone(),
                    plugin_config: state.admitted_plugin_config(),
                };
                let request = match state.plugin_state() {
                    Some(snapshot) => PluginSessionRequest::rematerialization(
                        state.session_id.clone(),
                        snapshot,
                        authority,
                    ),
                    None => PluginSessionRequest::creation(state.session_id.clone(), authority),
                };
                host.clone()
                    .with_trace_runtime(self.core.tracing.clone())
                    .with_execution_budgets(self.core.control.execution_budgets.clone())
                    .isolated_registry()
                    .defer_session(request)
                    .map_err(SessionError::Plugin)
            }
        }
    }

    pub async fn build(self) -> Result<LashRuntime, SessionError> {
        let (mut state, created) = self.resolve_state().await?;
        if created {
            crate::CoreConfigOwner::validate_charge_safety(&state.policy.charge_safety)
                .map_err(crate::CoreConfigOwner::creation_refusal)
                .map_err(SessionError::SessionConfigRefused)?;
        }
        let plugins = self.resolve_plugins(&state)?;
        if created {
            // A new session records what every installed owner resolves for
            // it, under the protocol its plugins registered (FIG-4379).
            // Creation is an adoption point of its own (FIG-4747): the
            // created head's namespaces are written in the formats the fleet
            // record permits now, and the session writes under that choice
            // until its first Run records one. Without a fleet store, each
            // plugin writes its declared native format.
            let admission = match &self.store {
                Some(store) => plugins
                    .host()
                    .admit_plugins(store.store().as_ref())
                    .await
                    .map_err(|error| SessionError::Plugin(error.into()))?,
                None => super::plugin_transition::native_plugin_admission(plugins.host())
                    .map_err(SessionError::Plugin)?,
            };
            state.authority.plugin_config = plugins.host().resolve_creation_plugin_config(
                plugins.host().protocol_plugin_id(),
                &crate::PluginOptions::default(),
                &admission,
            )?;
            plugins.adopt_plugin_admission(admission);
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
