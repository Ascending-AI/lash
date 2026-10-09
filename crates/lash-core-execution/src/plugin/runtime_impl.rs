use crate::{RuntimeOwner, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, Weak};

use lash_sansio::sync::MutexExt;

use super::*;

#[derive(Clone)]
pub struct PluginHost {
    prompt_render_pool: Option<Arc<super::prompt::PromptRenderPool>>,
    trace_runtime: crate::trace::TraceRuntime,
    factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(super) export_plugin_namespaces: bool,
    extensions: PluginExtensions,
    sessions: Arc<StdMutex<BTreeMap<RuntimeOwner, Weak<PluginSession>>>>,
    /// Config registration is collected on first use, after format preflight.
    config_registry: Arc<
        std::sync::OnceLock<Result<Arc<super::ConfigRegistry>, super::ConfigRegistrationError>>,
    >,
    execution_budgets: crate::ExecutionBudgets,
}

/// Inputs shared by new-session creation and reconstruction from durable
/// state. `owner` names the runtime the plugin session serves: a session, or
/// a process runtime, which is never a session.
#[derive(Clone, Debug)]
pub struct PluginSessionRequest<'a> {
    pub owner: RuntimeOwner,
    pub materialization: PluginSessionMaterializationRequest<'a>,
    pub tool_catalog_overlay: ToolCatalogContribution,
    pub tool_snapshot: Option<crate::ToolState>,
}

impl<'a> PluginSessionRequest<'a> {
    pub fn creation(session_id: impl Into<SessionId>, config: SessionAuthorityContext) -> Self {
        Self {
            owner: RuntimeOwner::Session(session_id.into()),
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: None,
            },
            tool_catalog_overlay: ToolCatalogContribution::default(),
            tool_snapshot: None,
        }
    }

    /// A process runtime's plugin session, built from the process's captured
    /// execution environment. No session lookup finds it.
    pub fn process_creation(process_id: crate::ProcessId, config: SessionAuthorityContext) -> Self {
        Self {
            owner: RuntimeOwner::Process(process_id),
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: None,
            },
            tool_catalog_overlay: ToolCatalogContribution::default(),
            tool_snapshot: None,
        }
    }

    pub fn rematerialization(
        session_id: impl Into<SessionId>,
        snapshot: &'a PluginState,
        config: SessionAuthorityContext,
    ) -> Self {
        Self {
            owner: RuntimeOwner::Session(session_id.into()),
            materialization: PluginSessionMaterializationRequest::Rematerialization {
                snapshot,
                config,
            },
            tool_catalog_overlay: ToolCatalogContribution::default(),
            tool_snapshot: None,
        }
    }
}

/// Creation may seed a fork from its spawn-time capture. Rematerialization
/// requires the snapshot already recorded on disk. Both build under the
/// session's recorded authority and plugin configuration.
#[derive(Clone, Debug)]
pub enum PluginSessionMaterializationRequest<'a> {
    Creation {
        config: SessionAuthorityContext,
        seed_snapshot: Option<&'a PluginState>,
    },
    Rematerialization {
        snapshot: &'a PluginState,
        config: SessionAuthorityContext,
    },
}

struct BuiltSessionContributions {
    plugins: Vec<Arc<dyn SessionPlugin>>,
    contributions: PluginContributions,
}

/// The recorded facts a plugin session is built under: the session's tool
/// authority and its recorded plugin configuration (or a process's captured
/// one).
#[derive(Clone, Debug)]
pub struct SessionAuthorityContext {
    pub tool_access: SessionToolAccess,
    /// The recorded plugin configuration the session is built with
    /// (FIG-4379).
    pub plugin_config: super::AdmittedPluginConfig,
}

impl SessionAuthorityContext {
    /// The fixture context tests share: ambient tool access and no plugin
    /// configuration.
    #[cfg(any(test, feature = "testing"))]
    pub fn ambient_fixture() -> Self {
        Self {
            tool_access: SessionToolAccess::ambient(),
            plugin_config: super::AdmittedPluginConfig::default(),
        }
    }
}

impl PluginHost {
    pub fn with_prompt_render_pool(
        mut self,
        pool: Option<Arc<super::prompt::PromptRenderPool>>,
    ) -> Self {
        self.prompt_render_pool = pool;
        self
    }
    pub fn prompt_render_pool(&self) -> &super::prompt::PromptRenderPool {
        self.prompt_render_pool
            .as_deref()
            .unwrap_or_else(|| super::prompt::PromptRenderPool::shared())
    }

    fn plugin_view(&self) -> Self {
        Self {
            export_plugin_namespaces: false,
            ..self.clone()
        }
    }

    /// A host of the builtin plugins under the caller's budgets and trace runtime.
    pub fn empty(
        execution_budgets: crate::ExecutionBudgets,
        trace_runtime: crate::trace::TraceRuntime,
    ) -> Self {
        Self::new(Vec::new(), execution_budgets, trace_runtime)
    }

    /// A host of `factories` over the builtin plugins. Every catalog its
    /// sessions build admits its tools against `execution_budgets`, the
    /// budgets its runtime's host config records. The trace runtime carries the
    /// caller's clock and observers from construction onward.
    pub fn new(
        factories: Vec<Arc<dyn PluginFactory>>,
        execution_budgets: crate::ExecutionBudgets,
        trace_runtime: crate::trace::TraceRuntime,
    ) -> Self {
        let override_ids: BTreeSet<&'static str> =
            factories.iter().map(|factory| factory.id()).collect();
        let mut all_factories = super::builtin_plugin_factories();
        if !override_ids.is_empty() {
            all_factories.retain(|factory| !override_ids.contains(factory.id()));
        }
        all_factories.extend(factories);
        let extensions = PluginExtensions::from_contributions(
            all_factories
                .iter()
                .flat_map(|factory| factory.extension_contributions()),
        );
        let config_registry = Arc::new(std::sync::OnceLock::new());
        Self {
            prompt_render_pool: None,
            factories: Arc::new(all_factories),
            protocol_factory: None,
            export_plugin_namespaces: true,
            extensions,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            config_registry,
            trace_runtime,
            execution_budgets,
        }
    }

    /// Select the protocol factory without constructing a session.
    pub fn with_protocol_plugin(mut self, protocol: Arc<dyn PluginFactory>) -> Self {
        let factories = Arc::make_mut(&mut self.factories);
        if let Some(existing) = factories
            .iter_mut()
            .find(|factory| factory.id() == protocol.id())
        {
            *existing = Arc::clone(&protocol);
        } else {
            factories.push(Arc::clone(&protocol));
        }
        self.protocol_factory = Some(protocol);
        self
    }

    pub fn protocol_plugin_id(&self) -> Option<&str> {
        self.protocol_factory.as_ref().map(|factory| factory.id())
    }

    pub fn with_trace_runtime(mut self, trace_runtime: crate::trace::TraceRuntime) -> Self {
        self.trace_runtime = trace_runtime;
        self
    }

    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        &self.trace_runtime
    }

    /// Replace the budgets this host was constructed with by the ones the
    /// runtime adopting it records in its host config.
    pub fn with_execution_budgets(mut self, execution_budgets: crate::ExecutionBudgets) -> Self {
        self.execution_budgets = execution_budgets;
        self
    }

    pub fn execution_budgets(&self) -> crate::ExecutionBudgets {
        self.execution_budgets.clone()
    }

    pub fn with_extensions(mut self, extensions: PluginExtensions) -> Self {
        self.extensions = extensions;
        self
    }

    pub fn isolated_registry(&self) -> Self {
        Self {
            prompt_render_pool: self.prompt_render_pool.clone(),
            factories: Arc::clone(&self.factories),
            protocol_factory: self.protocol_factory.clone(),
            export_plugin_namespaces: self.export_plugin_namespaces,
            extensions: self.extensions.clone(),
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            config_registry: Arc::clone(&self.config_registry),
            trace_runtime: self.trace_runtime.clone(),
            execution_budgets: self.execution_budgets.clone(),
        }
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    pub fn factories(&self) -> &[Arc<dyn PluginFactory>] {
        self.factories.as_ref().as_slice()
    }

    /// Every factory's transcript decoder, the protocol's first: a pure
    /// extension that needs no materialized plugin.
    pub fn transcript_decoders(&self) -> lash_core_store::transcript::TranscriptDecoders {
        self.protocol_factory
            .iter()
            .chain(self.factories.iter())
            .filter_map(|factory| factory.transcript_decoder())
            .fold(Default::default(), |decoders, decoder| {
                decoders.with_decoder(decoder)
            })
    }

    /// This host's plugins in hook order ([`super::PluginComposition`]): the
    /// builtin factories, then the embedder's, each as it declares itself.
    pub fn composition(&self) -> Result<super::PluginComposition, super::PluginDeclarationError> {
        let mut declarations = Vec::with_capacity(self.factories().len());
        for factory in self.factories() {
            let declaration = factory.plugin_declaration();
            if declaration.id.as_str() != factory.id() {
                return Err(super::PluginDeclarationError::IdMismatch {
                    factory: factory.id().to_owned(),
                    declared: declaration.id.as_str().to_owned(),
                });
            }
            declarations.push(declaration);
        }
        super::PluginComposition::new(declarations)
    }

    /// Admit this host's composition against the fleet record `store`
    /// carries (FIG-4747): the one place a writer format is chosen. A
    /// segment admission calls it once and records the answer; nothing that
    /// runs under the admission reads the fleet record again.
    ///
    /// A plugin the record does not name is provisioned from its
    /// declaration first.
    ///
    /// # Errors
    /// The store's typed refusal for a plugin that writes no format the
    /// fleet record permits, and any fault reading the record.
    pub async fn admit_plugins(
        &self,
        store: &(impl crate::store::FleetFormatStore + ?Sized),
    ) -> Result<crate::store::plugin_writers::PluginAdmission, crate::StoreError> {
        // A core validates its composition when it is built, so a
        // declaration refused here is a host assembled without one.
        let composition = self
            .composition()
            .map_err(|error| crate::StoreError::Backend(error.to_string()))?;
        let registrations = composition.writer_registrations();
        let mut ranges = store.plugin_writers().await?;
        if registrations
            .iter()
            .any(|registration| ranges.permitted_writer(&registration.plugin).is_err())
        {
            ranges = store.provision_plugin_writers(&registrations).await?;
        }
        composition
            .admission(&ranges)
            .map_err(|refusal| crate::StoreError::Incompatible { refusal })
    }

    /// Every config registration of this host's plugins, and the core
    /// owner's (FIG-4379): the one list config creation, command ingress,
    /// resolution and the command catalog are generated from. A factory's
    /// invalid registration refuses here, on every use.
    pub fn config_registry(
        &self,
    ) -> Result<Arc<super::ConfigRegistry>, super::ConfigRegistrationError> {
        self.config_registry
            .get_or_init(|| super::ConfigRegistry::build(self.factories()).map(Arc::new))
            .clone()
    }

    /// The recorded plugin configuration of a session created on this host
    /// (FIG-4379): every registered owner creates its namespace from
    /// `requested`, and `protocol_plugin_id` names the protocol owner. Each
    /// namespace is written in the format `writers` records for its plugin
    /// ([`Self::admit_plugins`]).
    pub fn resolve_creation_plugin_config(
        &self,
        protocol_plugin_id: Option<&str>,
        requested: &crate::PluginOptions,
        writers: &crate::store::plugin_writers::PluginAdmission,
    ) -> Result<super::PluginConfig, super::CreationConfigError> {
        let options = super::PluginConfig::from_recorded_parts(None, requested.plugins.clone());
        self.decode_config(&options)?;
        Ok(self
            .config_registry()?
            .resolve_creation(protocol_plugin_id, requested, writers)?)
    }

    /// Ask every factory for its process-engine contributions and register them
    /// on `runtime_host`, enforcing unique [`ProcessEngine::kind`](crate::ProcessEngine::kind)
    /// across all engines (directly wired or plugin-contributed).
    ///
    /// This is the core-owned installation step that replaces facade-level
    /// out-of-band wiring: engine construction that needs the fully-built plugin
    /// host's extensions runs here, after the host is built. The trace context
    /// handed to factories is the one already on `runtime_host`.
    pub fn install_process_engine_contributions(
        &self,
        mut runtime_host: crate::RuntimeHostConfig,
        process_lifecycle_available: bool,
    ) -> Result<crate::RuntimeHostConfig, PluginError> {
        let trace_runtime = runtime_host.tracing.clone();
        let backend = runtime_host.backend().clone();
        let ctx = super::ProcessEngineContributionContext::new(
            self,
            &backend,
            &trace_runtime,
            process_lifecycle_available,
        );
        for factory in self.factories() {
            for engine in factory.process_engine_contributions(&ctx)? {
                runtime_host.install_contributed_process_engine(engine)?;
            }
        }
        Ok(runtime_host)
    }

    /// The registered executable composition, in declaration order.
    pub fn plugin_revisions(&self) -> Vec<PluginRevision> {
        self.factories()
            .iter()
            .map(|factory| {
                PluginRevision::new(factory.id(), factory.plugin_declaration().behavior_revision)
            })
            .collect()
    }

    /// A recorded segment never adopts this build's different code.
    pub fn validate_plugin_admission(
        &self,
        admission: &crate::store::plugin_writers::PluginAdmission,
    ) -> Result<(), PluginError> {
        let recorded: Vec<_> = admission
            .plugins()
            .iter()
            .map(|plugin| PluginRevision::new(&plugin.plugin, plugin.behavior_revision))
            .collect();
        let available = self.plugin_revisions();
        if recorded != available {
            return Err(PluginError::Runtime(
                PluginExecutionRefusal {
                    recorded,
                    available,
                    callback: None,
                }
                .into_runtime_error(),
            ));
        }
        Ok(())
    }

    /// Retain raw state and recorded config without constructing capabilities.
    pub fn defer_session(
        &self,
        request: PluginSessionRequest<'_>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let PluginSessionRequest {
            owner,
            materialization,
            tool_catalog_overlay,
            tool_snapshot,
        } = request;
        let (authority, materialization, snapshot, forked) = match materialization {
            PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot,
            } => (
                config,
                PluginSessionMaterialization::Creation,
                seed_snapshot,
                seed_snapshot.is_some(),
            ),
            PluginSessionMaterializationRequest::Rematerialization { snapshot, config } => (
                config,
                PluginSessionMaterialization::Rematerialization,
                Some(snapshot),
                false,
            ),
        };
        self.composition()?;
        Ok(Arc::new(PluginSession {
            state: Arc::new(StdMutex::new({
                let mut registry = if forked {
                    PluginStateRegistry::from_fork(snapshot)
                } else {
                    PluginStateRegistry::from_snapshot(snapshot)
                };
                registry.trace_limits = self.trace_runtime.limits();
                registry
            })),
            native_view: Arc::new(StdMutex::new(None)),
            host: self.clone(),
            owner,
            materialization,
            tool_snapshot,
            capabilities: Arc::new(std::sync::OnceLock::new()),
            tool_catalog_overlay,
            authority: Arc::new(std::sync::RwLock::new(
                super::session_obj::LiveSessionAuthority {
                    tool_access: authority.tool_access,
                    plugin_config: authority.plugin_config,
                },
            )),
            extensions: self.extensions.clone(),
            materialized: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            materialization_lock: Arc::new(StdMutex::new(())),
            retains_state: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            forked,
            admission: Arc::new(StdMutex::new(None)),
        }))
    }

    pub fn build_session(
        &self,
        request: PluginSessionRequest<'_>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let session = self.defer_session(request)?;
        // Callers of this constructor supply an admitted native snapshot.
        self.materialize_session(&session)?;
        Ok(session)
    }

    pub(super) fn materialize_session(
        &self,
        session: &Arc<PluginSession>,
    ) -> Result<(), PluginError> {
        let _materialization = session.materialization_lock.lock_recover();
        session.validate_recorded_admission()?;
        if session.is_materialized() {
            return Ok(());
        }
        if session.capabilities.get().is_none() {
            let authority = session.live_authority();
            let snapshot = session.capture_state();
            for factory in self.factories() {
                if let Some(namespace) = snapshot.plugins.get(factory.id()) {
                    super::state::validate_namespace(&namespace.values)?;
                }
            }
            self.validate_native_formats(&snapshot, &authority.plugin_config.config)?;
            let ctx = PluginSessionContext {
                tracing: self.trace_runtime.clone(),
                trace: None,
                owner: session.owner.clone(),
                tool_access: authority.tool_access,
                plugin_config: authority.plugin_config,
                materialization: session.materialization,
                extensions: self.extensions.clone(),
            };
            let BuiltSessionContributions {
                plugins,
                contributions,
            } = self.build_session_contributions(&ctx, Arc::clone(&session.state))?;
            let registry = build_tool_registry(&contributions, session.tool_snapshot.clone())?;
            let tools = Arc::clone(&registry) as Arc<dyn ToolProvider>;
            let session_extensions = PluginExtensions::from_contributions(
                plugins
                    .iter()
                    .flat_map(|plugin| plugin.extension_contributions()),
            );
            session.retains_state.store(
                !contributions.state_retaining_plugins.is_empty(),
                std::sync::atomic::Ordering::SeqCst,
            );
            let _ = session
                .capabilities
                .set(super::session_obj::PluginSessionCapabilities {
                    plugins,
                    contributions,
                    tool_registry: registry,
                    tools,
                    session_extensions,
                });
        }
        self.register_session(&session.owner, session)?;
        for plugin in &session.capabilities().plugins {
            let state =
                PluginStateView::bind(&session.owner, plugin.id(), Arc::clone(&session.state));
            let probe = state.retention_probe();
            plugin.session_ready(SessionReadyContext {
                tracing: self.trace_runtime.clone(),
                trace: None,
                owner: session.owner.clone(),
                host: self.plugin_view(),
                state,
            })?;
            if Arc::strong_count(&probe) > 1 {
                session
                    .retains_state
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        session
            .materialized
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// The prompt sections, families and wrappers the installed plugins
    /// register for a session recorded under `plugin_config`, built for a
    /// host to inspect: the plugins are built over empty state, and no
    /// session is materialized or registered.
    ///
    /// # Errors
    ///
    /// A plugin's build or registration error.
    pub fn inspect_prompt_catalog(
        &self,
        owner: RuntimeOwner,
        tool_access: crate::SessionToolAccess,
        plugin_config: super::AdmittedPluginConfig,
    ) -> Result<super::prompt::PromptCatalog, PluginError> {
        let ctx = PluginSessionContext {
            tracing: self.trace_runtime.clone(),
            trace: None,
            owner,
            tool_access,
            plugin_config,
            materialization: PluginSessionMaterialization::Rematerialization,
            extensions: self.extensions.clone(),
        };
        let built = self.build_session_contributions(
            &ctx,
            Arc::new(StdMutex::new(PluginStateRegistry::from_snapshot(None))),
        )?;
        Ok(super::prompt::PromptCatalog::new(
            built.contributions.prompt,
        ))
    }

    fn build_session_contributions(
        &self,
        ctx: &PluginSessionContext,
        state: Arc<StdMutex<PluginStateRegistry>>,
    ) -> Result<BuiltSessionContributions, PluginError> {
        let mut registry = state.lock_recover();
        for factory in self.factories() {
            registry
                .data
                .plugins
                .entry(factory.id().into())
                .or_insert_with(|| super::PluginNamespaceState {
                    format_version: factory.plugin_declaration().format_version,
                    ..Default::default()
                });
            registry.declare_fork(factory.id(), factory.plugin_declaration().state_fork);
        }
        drop(registry);
        let mut plugins = Vec::new();
        let mut contributions = PluginContributions::default();
        let mut tool_names = Default::default();
        for factory in self.factories() {
            let plugin = factory.build(ctx)?;
            if plugin.id() != factory.id() {
                return Err(PluginError::Registration(format!(
                    "factory `{}` built plugin `{}`",
                    factory.id(),
                    plugin.id()
                )));
            }
            let mut reg = PluginRegistrar::new(PluginRevision::new(
                factory.id(),
                factory.plugin_declaration().behavior_revision,
            ));
            reg.contributions = contributions;
            reg.tool_names = tool_names;
            reg.state = Some(PluginStateView::bind(
                &ctx.owner,
                plugin.id(),
                Arc::clone(&state),
            ));
            plugin.register(&mut reg)?;
            reg.validate_stream_state_pairs()?;
            if let Some(store) = reg.state.take() {
                let probe = store.retention_probe();
                drop(store);
                if Arc::strong_count(&probe) > 1 {
                    reg.contributions
                        .state_retaining_plugins
                        .push(plugin.id().to_string());
                }
            }
            contributions = reg.contributions;
            tool_names = reg.tool_names;
            plugins.push(plugin);
        }

        let protocol_session = contributions.protocol_session.take().ok_or_else(|| {
            PluginError::Registration("missing protocol session capability".to_string())
        })?;
        let protocol_driver = contributions.protocol_driver.take().ok_or_else(|| {
            PluginError::Registration("missing protocol driver capability".to_string())
        })?;
        contributions.protocol_session = Some(protocol_session);
        contributions.protocol_driver = Some(protocol_driver);
        contributions
            .attachment_omission_policies
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        contributions
            .context_compactors
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        contributions
            .context_pressure_hooks
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        Ok(BuiltSessionContributions {
            plugins,
            contributions,
        })
    }

    fn register_session(
        &self,
        owner: &RuntimeOwner,
        session: &Arc<PluginSession>,
    ) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock_recover();
        if let Some(existing) = sessions.get(owner).and_then(Weak::upgrade) {
            if !Arc::ptr_eq(&existing, session) {
                return Err(PluginError::Session(format!(
                    "plugin session for `{owner}` is already registered on this plugin host"
                )));
            }
            return Ok(());
        }
        sessions.insert(owner.clone(), Arc::downgrade(session));
        Ok(())
    }

    pub fn unregister_session(&self, session_id: &SessionId) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock_recover();
        sessions.remove(&RuntimeOwner::Session(session_id.clone()));
        Ok(())
    }

    pub fn session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<PluginSession>, PluginOperationInvokeError> {
        let mut sessions = self.sessions.lock_recover();
        let owner = RuntimeOwner::Session(session_id.clone());
        let Some(weak) = sessions.get(&owner).cloned() else {
            return Err(PluginOperationInvokeError::UnknownSession(
                session_id.to_string(),
            ));
        };
        match weak.upgrade() {
            Some(session) => {
                if self.export_plugin_namespaces == session.host.export_plugin_namespaces {
                    Ok(session)
                } else {
                    let mut view = (*session).clone();
                    view.host = self.clone();
                    Ok(Arc::new(view))
                }
            }
            None => {
                sessions.remove(&owner);
                Err(PluginOperationInvokeError::UnknownSession(
                    session_id.to_string(),
                ))
            }
        }
    }
}

fn build_tool_registry(
    contributions: &PluginContributions,
    tool_snapshot: Option<crate::ToolState>,
) -> Result<Arc<crate::ToolRegistry>, PluginError> {
    let mut providers_by_source = BTreeMap::<String, Vec<Arc<dyn crate::ToolProvider>>>::new();
    for registered in &contributions.tool_providers {
        providers_by_source
            .entry(registered.identity.owner.plugin.clone())
            .or_default()
            .push(Arc::clone(&registered.hook));
    }
    let registry =
        crate::ToolRegistry::from_tool_registrations(providers_by_source.into_iter().collect())
            .map_err(|err| {
                PluginError::Registration(format!("failed to build tool registry: {err}"))
            })?;
    match tool_snapshot {
        Some(snapshot) => registry
            .fork_with_state(snapshot)
            .map(Arc::new)
            .map_err(|err| {
                PluginError::Session(format!(
                    "tool state cannot be applied to this plugin host session: {err}"
                ))
            }),
        None => Ok(Arc::new(registry)),
    }
}
