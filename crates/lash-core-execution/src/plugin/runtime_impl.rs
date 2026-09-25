use crate::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, Weak};

use lash_sansio::sync::MutexExt;

use super::*;

pub trait ProcessEngineContributionTarget {
    fn process_engine_trace_context(&self) -> &lash_trace::TraceContext;
    fn process_observation_sink(&self) -> Option<Arc<dyn lash_trace::TraceSink>>;
    fn install_contributed_process_engine(
        &mut self,
        registration: crate::ProcessEngineRegistration,
    ) -> Result<(), crate::PluginError>;
}

#[derive(Clone)]
pub struct PluginHost {
    factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(super) export_plugin_namespaces: bool,
    extensions: PluginExtensions,
    sessions: Arc<StdMutex<BTreeMap<SessionId, Weak<PluginSession>>>>,
}

struct BuildPluginSessionRequest<'a> {
    session_id: SessionId,
    parent_session_id: Option<SessionId>,
    materialization: PluginSessionMaterializationRequest<'a>,
    tool_catalog_overlay: ToolCatalogContribution,
    tool_snapshot: Option<crate::ToolState>,
}

enum PluginSessionMaterializationRequest<'a> {
    Creation {
        config: SessionCreationConfig,
        seed_snapshot: Option<&'a PluginState>,
    },
    Rematerialization {
        snapshot: &'a PluginState,
        config: RecordedSessionConfig,
    },
}

struct BuiltSessionContributions {
    plugins: Vec<Arc<dyn SessionPlugin>>,
    contributions: PluginContributions,
    state: Arc<StdMutex<PluginStateRegistry>>,
    triggers: crate::TriggerEventCatalog,
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuthorityContext {
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub plugin_options: PluginOptions,
}

/// Configuration used while constructing a genuinely new plugin session.
///
/// Protocol options may be empty here because protocol materialization owns
/// applying create-time defaults after factories have selected their surface.
#[derive(Clone, Debug, Default)]
pub struct SessionCreationConfig {
    pub authority: SessionAuthorityContext,
    pub protocol_turn_options: crate::ProtocolTurnOptions,
}

/// Durable configuration required to reconstruct an existing plugin session.
///
/// This deliberately has no [`Default`] implementation: every restore-shaped
/// construction site must name the recorded protocol options explicitly.
#[derive(Clone, Debug)]
pub struct RecordedSessionConfig {
    pub authority: SessionAuthorityContext,
    pub protocol_turn_options: crate::ProtocolTurnOptions,
}

impl RecordedSessionConfig {
    pub fn new(protocol_turn_options: crate::ProtocolTurnOptions) -> Self {
        Self {
            authority: SessionAuthorityContext::default(),
            protocol_turn_options,
        }
    }
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
        Self {
            factories: Arc::new(all_factories),
            export_plugin_namespaces: true,
            extensions,
            sessions: Arc::new(StdMutex::new(BTreeMap::new())),
        }
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
        }
    }

    pub fn extensions(&self) -> &PluginExtensions {
        &self.extensions
    }

    pub fn factories(&self) -> &[Arc<dyn PluginFactory>] {
        self.factories.as_ref().as_slice()
    }

    /// Ask every factory for its process-engine contributions and register them
    /// on `runtime_host`, enforcing unique [`ProcessEngine::kind`](crate::ProcessEngine::kind)
    /// across all engines (directly wired or plugin-contributed).
    ///
    /// This is the core-owned installation step that replaces facade-level
    /// out-of-band wiring: engine construction that needs the fully-built plugin
    /// host's extensions runs here, after the host is built. The trace context
    /// handed to factories is the one already on `runtime_host`.
    pub fn install_process_engine_contributions<T: ProcessEngineContributionTarget>(
        &self,
        mut runtime_host: T,
        process_lifecycle_available: bool,
    ) -> Result<T, PluginError> {
        let trace_context = runtime_host.process_engine_trace_context().clone();
        let observation_sink = runtime_host.process_observation_sink();
        let ctx = super::ProcessEngineContributionContext::new(
            &self.extensions,
            &trace_context,
            process_lifecycle_available,
        )
        .with_process_observation_sink(observation_sink);
        for factory in self.factories() {
            for engine in factory.process_engine_contributions(&ctx)? {
                runtime_host.install_contributed_process_engine(engine)?;
            }
        }
        Ok(runtime_host)
    }

    pub fn build_session(
        &self,
        session_id: impl Into<SessionId>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_with_overlay(
            session_id,
            ToolCatalogContribution::default(),
            None,
            SessionCreationConfig::default(),
        )
    }

    pub fn rematerialize_session(
        &self,
        session_id: impl Into<SessionId>,
        snapshot: &PluginState,
        config: RecordedSessionConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.rematerialize_session_with_overlay(
            session_id,
            snapshot,
            ToolCatalogContribution::default(),
            None,
            config,
        )
    }

    /// Variant of [`build_session`](Self::build_session) that records the caller as the
    /// parent of the new session. Plugin factories read
    /// [`PluginSessionContext::is_root_session`] to gate root-only
    /// behavior; anything that goes through the plain `build_session`
    /// is treated as a root session by default.
    pub fn build_session_with_parent(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        config: SessionCreationConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_with_parent_and_overlay(
            session_id,
            parent_session_id,
            ToolCatalogContribution::default(),
            None,
            config,
        )
    }

    pub fn rematerialize_session_with_parent(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        snapshot: &PluginState,
        config: RecordedSessionConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.rematerialize_session_with_parent_and_overlay(
            session_id,
            parent_session_id,
            snapshot,
            ToolCatalogContribution::default(),
            None,
            config,
        )
    }

    pub fn build_session_with_parent_and_overlay(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        config: SessionCreationConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id,
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: None,
            },
            tool_catalog_overlay,
            tool_snapshot,
        })
    }

    pub fn build_session_with_overlay(
        &self,
        session_id: impl Into<SessionId>,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        config: SessionCreationConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id: None,
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: None,
            },
            tool_catalog_overlay,
            tool_snapshot,
        })
    }

    /// Materialize a forked peer session from the spawn-time
    /// [`SessionPluginInit`] capture. The payload is the only input — this
    /// path never opens or reads a live parent session, so a worker restart
    /// between spawn and execution initializes identically.
    pub fn build_session_from_init(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        init: &SessionPluginInit,
        config: SessionCreationConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_forked_session_with_parent_and_overlay(
            session_id,
            parent_session_id,
            &init.plugin_state,
            init.tool_catalog_overlay.clone(),
            Some(init.tool_state.clone()),
            config,
        )
    }

    pub(super) fn build_forked_session_with_parent_and_overlay(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        seed_snapshot: &PluginState,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        config: SessionCreationConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id,
            materialization: PluginSessionMaterializationRequest::Creation {
                config,
                seed_snapshot: Some(seed_snapshot),
            },
            tool_catalog_overlay,
            tool_snapshot,
        })
    }

    pub fn rematerialize_session_with_parent_and_overlay(
        &self,
        session_id: impl Into<SessionId>,
        parent_session_id: Option<SessionId>,
        snapshot: &PluginState,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        config: RecordedSessionConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id,
            materialization: PluginSessionMaterializationRequest::Rematerialization {
                snapshot,
                config,
            },
            tool_catalog_overlay,
            tool_snapshot,
        })
    }

    pub fn rematerialize_session_with_overlay(
        &self,
        session_id: impl Into<SessionId>,
        snapshot: &PluginState,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        config: RecordedSessionConfig,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.rematerialize_session_with_parent_and_overlay(
            session_id,
            None,
            snapshot,
            tool_catalog_overlay,
            tool_snapshot,
            config,
        )
    }

    fn build_session_inner(
        &self,
        request: BuildPluginSessionRequest<'_>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let BuildPluginSessionRequest {
            session_id,
            parent_session_id,
            materialization,
            tool_catalog_overlay,
            tool_snapshot,
        } = request;
        let (authority, protocol_turn_options, materialization, snapshot, forked) =
            match materialization {
                PluginSessionMaterializationRequest::Creation {
                    config,
                    seed_snapshot,
                } => (
                    config.authority,
                    config.protocol_turn_options,
                    PluginSessionMaterialization::Creation,
                    seed_snapshot,
                    seed_snapshot.is_some(),
                ),
                PluginSessionMaterializationRequest::Rematerialization { snapshot, config } => (
                    config.authority,
                    config.protocol_turn_options,
                    PluginSessionMaterialization::Rematerialization,
                    Some(snapshot),
                    false,
                ),
            };
        let ctx = PluginSessionContext {
            session_id,
            tool_access: authority.tool_access.clone(),
            subagent: authority.subagent.clone(),
            plugin_options: authority.plugin_options.clone(),
            protocol_turn_options,
            materialization,
            extensions: self.extensions.clone(),
            parent_session_id,
        };
        let session_id = ctx.session_id.clone();
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
            session_id: ctx.session_id,
            plugins,
            tools,
            tool_registry: registry,
            tool_catalog_overlay,
            tool_access: Arc::new(std::sync::RwLock::new(authority.tool_access)),
            subagent: authority.subagent,
            extensions: self.extensions.clone(),
            session_extensions,
            triggers,
            retains_state: Arc::new(std::sync::atomic::AtomicBool::new(
                !contributions.state_retaining_plugins.is_empty(),
            )),
            contributions,
            forked,
        });
        self.register_session(&session_id, &session)?;
        session.state.lock_recover().initialize(snapshot)?;
        for plugin in &session.plugins {
            let state = PluginStateStore::bind(
                &session.session_id,
                plugin.id(),
                Arc::clone(&session.state),
            );
            let probe = state.retention_probe();
            plugin.session_ready(SessionReadyContext {
                session_id: session.session_id.clone(),
                host: self.plugin_view(),
                state,
            })?;
            if Arc::strong_count(&probe) > 1 {
                session
                    .retains_state
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Ok(session)
    }

    fn build_session_contributions(
        &self,
        ctx: &PluginSessionContext,
        snapshot: Option<&PluginState>,
    ) -> Result<BuiltSessionContributions, PluginError> {
        let state = Arc::new(StdMutex::new(PluginStateRegistry::registering(snapshot)));
        let mut plugins = Vec::new();
        let mut reg = PluginRegistrar::new();
        for factory in self.factories() {
            let plugin = factory.build(ctx)?;
            reg.registering_plugin_id = Some(plugin.id().to_string());
            reg.state = Some(PluginStateStore::bind(
                &ctx.session_id,
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

    pub fn build_core_tool_registry(&self) -> Result<Arc<crate::ToolRegistry>, PluginError> {
        let ctx = PluginSessionContext {
            session_id: SessionId::from("lash-core-tool-catalog"),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            plugin_options: PluginOptions::default(),
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
            materialization: PluginSessionMaterialization::Creation,
            extensions: self.extensions.clone(),
            parent_session_id: None,
        };
        let built = self.build_session_contributions(&ctx, None)?;
        build_tool_registry(&built.contributions, None)
    }

    fn register_session(
        &self,
        session_id: &SessionId,
        session: &Arc<PluginSession>,
    ) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock_recover();
        if let Some(existing) = sessions.get(session_id).and_then(Weak::upgrade) {
            if !Arc::ptr_eq(&existing, session) {
                return Err(PluginError::Session(format!(
                    "session `{session_id}` is already registered on this plugin host"
                )));
            }
            return Ok(());
        }
        sessions.insert(
            SessionId::from(session_id.to_string()),
            Arc::downgrade(session),
        );
        Ok(())
    }

    pub fn unregister_session(&self, session_id: &SessionId) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock_recover();
        sessions.remove(session_id);
        Ok(())
    }

    pub fn session(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<PluginSession>, PluginOperationInvokeError> {
        let mut sessions = self.sessions.lock_recover();
        let Some(weak) = sessions.get(session_id).cloned() else {
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
                sessions.remove(session_id);
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
    let registry = crate::ToolRegistry::from_tool_registrations(
        providers_by_source.into_iter().collect(),
        contributions.internal_tools.clone(),
        contributions.orchestrating_tools.clone(),
    )
    .map_err(|err| PluginError::Registration(format!("failed to build tool registry: {err}")))?;
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
