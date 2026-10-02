use crate::{RuntimeOwner, SessionId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, Weak};

use lash_sansio::sync::MutexExt;

use super::*;

#[derive(Clone)]
pub struct PluginHost {
    trace_runtime: crate::trace::TraceRuntime,
    factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) export_plugin_namespaces: bool,
    extensions: PluginExtensions,
    sessions: Arc<StdMutex<BTreeMap<RuntimeOwner, Weak<PluginSession>>>>,
    /// Config registration is collected on first use, after format preflight.
    config_registry: Arc<
        std::sync::OnceLock<Result<Arc<super::ConfigRegistry>, super::ConfigRegistrationError>>,
    >,
}

/// Inputs shared by new-session creation and reconstruction from durable
/// state. `owner` names the runtime the plugin session serves: a session, or
/// a process runtime, which is never a session.
#[derive(Clone, Debug)]
pub struct PluginSessionRequest<'a> {
    pub owner: RuntimeOwner,
    pub parent_session_id: Option<SessionId>,
    pub materialization: PluginSessionMaterializationRequest<'a>,
    pub tool_catalog_overlay: ToolCatalogContribution,
    pub tool_snapshot: Option<crate::ToolState>,
}

impl<'a> PluginSessionRequest<'a> {
    pub fn creation(session_id: impl Into<SessionId>, config: SessionAuthorityContext) -> Self {
        Self {
            owner: RuntimeOwner::Session(session_id.into()),
            parent_session_id: None,
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
            parent_session_id: None,
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
            parent_session_id: None,
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
    state: Arc<StdMutex<PluginStateRegistry>>,
    triggers: crate::TriggerEventCatalog,
}

/// The recorded facts a plugin session is built under: the session's tool
/// authority and its recorded plugin configuration (or a process's captured
/// one).
#[derive(Clone, Debug, Default)]
pub struct SessionAuthorityContext {
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    /// The recorded plugin configuration the session is built with
    /// (FIG-4379).
    pub plugin_config: super::AdmittedPluginConfig,
}

impl PluginHost {
    fn plugin_view(&self) -> Self {
        Self {
            export_plugin_namespaces: false,
            ..self.clone()
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn new(factories: Vec<Arc<dyn PluginFactory>>) -> Self {
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
            factories: Arc::new(all_factories),
            export_plugin_namespaces: true,
            extensions,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            config_registry,
            trace_runtime: crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
        }
    }

    pub fn with_trace_runtime(mut self, trace_runtime: crate::trace::TraceRuntime) -> Self {
        self.trace_runtime = trace_runtime;
        self
    }

    pub fn trace_runtime(&self) -> &crate::trace::TraceRuntime {
        &self.trace_runtime
    }

    pub fn with_extensions(mut self, extensions: PluginExtensions) -> Self {
        self.extensions = extensions;
        self
    }

    pub fn isolated_registry(&self) -> Self {
        Self {
            factories: Arc::clone(&self.factories),
            export_plugin_namespaces: self.export_plugin_namespaces,
            extensions: self.extensions.clone(),
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
            config_registry: Arc::clone(&self.config_registry),
            trace_runtime: self.trace_runtime.clone(),
        }
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    pub fn factories(&self) -> &[Arc<dyn PluginFactory>] {
        self.factories.as_ref().as_slice()
    }

    /// This host's plugins in hook order ([`super::PluginComposition`]): the
    /// builtin factories, then the embedder's, each as it declares itself.
    pub fn composition(&self) -> Result<super::PluginComposition, super::PluginDeclarationError> {
        super::PluginComposition::of(self.factories())
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
        parent: Option<&super::PluginConfig>,
        is_root_session: bool,
        writers: &crate::store::plugin_writers::PluginAdmission,
    ) -> Result<super::PluginConfig, super::CreationConfigError> {
        let options = super::PluginConfig::from_recorded_parts(None, requested.plugins.clone());
        self.decode_config(&options)?;
        if let Some(parent) = parent {
            self.decode_config(parent)?;
        }
        Ok(self.config_registry()?.resolve_creation(
            protocol_plugin_id,
            requested,
            parent,
            is_root_session,
            writers,
        )?)
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
        let ctx = super::ProcessEngineContributionContext::new(
            self,
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

    pub fn build_session(
        &self,
        request: PluginSessionRequest<'_>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let PluginSessionRequest {
            owner,
            parent_session_id,
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
        self.validate_config_formats(&authority.plugin_config.config)?;
        if let Some(snapshot) = snapshot {
            self.validate_state_formats(snapshot)?;
        }
        let mut authority = authority;
        authority.plugin_config.config =
            Arc::new(self.decode_config(&authority.plugin_config.config)?);
        let decoded_snapshot = snapshot
            .map(|snapshot| self.decode_state(snapshot))
            .transpose()?;
        let snapshot = decoded_snapshot.as_ref();
        let ctx = PluginSessionContext {
            tracing: self.trace_runtime.clone(),
            trace: None,
            owner,
            tool_access: authority.tool_access.clone(),
            subagent: authority.subagent.clone(),
            plugin_config: authority.plugin_config.clone(),
            materialization,
            extensions: self.extensions.clone(),
            parent_session_id,
        };
        let owner = ctx.owner.clone();
        let BuiltSessionContributions {
            plugins,
            contributions,
            state,
            triggers,
        } = self.build_session_contributions(&ctx, snapshot)?;
        let registry = build_tool_registry(&contributions, tool_snapshot)?;
        let tools = Arc::clone(&registry) as Arc<dyn ToolProvider>;
        let session_extensions = PluginExtensions::from_contributions(
            plugins
                .iter()
                .flat_map(|plugin| plugin.extension_contributions()),
        );

        let session = Arc::new(PluginSession {
            state,
            host: self.clone(),
            owner: ctx.owner,
            plugins,
            tools,
            tool_registry: registry,
            tool_catalog_overlay,
            authority: Arc::new(std::sync::RwLock::new(
                super::session_obj::LiveSessionAuthority {
                    tool_access: authority.tool_access,
                    subagent: authority.subagent,
                    plugin_config: authority.plugin_config,
                },
            )),
            extensions: self.extensions.clone(),
            session_extensions,
            triggers,
            retains_state: Arc::new(std::sync::atomic::AtomicBool::new(
                !contributions.state_retaining_plugins.is_empty(),
            )),
            contributions,
            forked,
            admission: Arc::new(std::sync::Mutex::new(None)),
        });
        self.register_session(&owner, &session)?;
        for plugin in &session.plugins {
            let state =
                PluginStateStore::bind(&session.owner, plugin.id(), Arc::clone(&session.state));
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
        // Registration and readiness both contribute to a cold materialization.
        // Freeze their replay log before later writes become an uncommitted tail.
        session
            .state
            .lock_recover()
            .initialize(&session.owner, snapshot)?;
        Ok(session)
    }

    fn build_session_contributions(
        &self,
        ctx: &PluginSessionContext,
        snapshot: Option<&PluginState>,
    ) -> Result<BuiltSessionContributions, PluginError> {
        let mut registry = PluginStateRegistry::registering(snapshot);
        for factory in self.factories() {
            registry
                .data
                .plugins
                .entry(factory.id().into())
                .or_insert_with(|| super::PluginNamespaceState {
                    format_version: factory.declaration().format_version,
                    ..Default::default()
                });
        }
        let state = Arc::new(StdMutex::new(registry));
        let mut plugins = Vec::new();
        let mut reg = PluginRegistrar::new();
        for factory in self.factories() {
            let plugin = factory.build(ctx)?;
            reg.registering_plugin_id = Some(plugin.id().to_string());
            reg.state = Some(PluginStateStore::bind(
                &ctx.owner,
                plugin.id(),
                Arc::clone(&state),
            ));
            plugin.register(&mut reg)?;
            if let Some(store) = reg.state.take() {
                let probe = store.retention_probe();
                drop(store);
                if Arc::strong_count(&probe) > 1 {
                    reg.contributions
                        .state_retaining_plugins
                        .push(plugin.id().to_string());
                }
            }
            reg.registering_plugin_id = None;
            plugins.push(plugin);
        }
        let mut contributions = reg.contributions;
        let protocol_session = contributions.protocol_session.take().ok_or_else(|| {
            PluginError::Registration("missing protocol session capability".to_string())
        })?;
        let protocol_driver = contributions.protocol_driver.take().ok_or_else(|| {
            PluginError::Registration("missing protocol driver capability".to_string())
        })?;
        contributions.protocol_session = Some(protocol_session);
        contributions.protocol_driver = Some(protocol_driver);
        contributions
            .turn_context_transforms
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        contributions
            .context_compactors
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        contributions
            .context_pressure_hooks
            .sort_by_key(|entry| std::cmp::Reverse(entry.0));
        let triggers = crate::TriggerEventCatalog::from_events(contributions.triggers.clone())
            .map_err(|message| {
                PluginError::Registration(format!("invalid trigger event catalog: {message}"))
            })?;
        Ok(BuiltSessionContributions {
            state,
            plugins,
            contributions,
            triggers,
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
            .entry(registered.plugin_id.clone())
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
