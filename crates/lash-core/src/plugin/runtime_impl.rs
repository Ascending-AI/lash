use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::{Mutex as StdMutex, Weak};

use super::*;

#[derive(Clone)]
pub struct PluginHost {
    factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    extensions: PluginExtensions,
    sessions: Arc<StdMutex<BTreeMap<String, Weak<PluginSession>>>>,
}

struct BuildPluginSessionRequest<'a> {
    session_id: String,
    parent_session_id: Option<String>,
    snapshot: Option<&'a PluginSessionSnapshot>,
    tool_catalog_overlay: ToolCatalogContribution,
    tool_snapshot: Option<crate::ToolState>,
    authority: SessionAuthorityContext,
}

#[derive(Clone, Debug, Default)]
pub struct SessionAuthorityContext {
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub plugin_options: PluginOptions,
}

impl PluginHost {
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
    pub fn install_process_engine_contributions(
        &self,
        mut runtime_host: crate::runtime::RuntimeHostConfig,
        process_lifecycle_available: bool,
    ) -> Result<crate::runtime::RuntimeHostConfig, PluginError> {
        let trace_context = runtime_host.tracing.trace_context.clone();
        let ctx = super::ProcessEngineContributionContext::new(
            &self.extensions,
            &trace_context,
            process_lifecycle_available,
        );
        for factory in self.factories() {
            for engine in factory.process_engine_contributions(&ctx)? {
                runtime_host.process_engines = runtime_host
                    .process_engines
                    .clone()
                    .try_with_engine(engine)?;
            }
        }
        Ok(runtime_host)
    }

    pub fn build_session(
        &self,
        session_id: impl Into<String>,
        snapshot: Option<&PluginSessionSnapshot>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_with_overlay(
            session_id,
            snapshot,
            ToolCatalogContribution::default(),
            None,
        )
    }

    /// Variant of [`build_session`](Self::build_session) that records the caller as the
    /// parent of the new session. Plugin factories read
    /// [`PluginSessionContext::is_root_session`] to gate root-only
    /// behavior; anything that goes through the plain `build_session`
    /// is treated as a root session by default.
    pub fn build_session_with_parent(
        &self,
        session_id: impl Into<String>,
        parent_session_id: Option<String>,
        snapshot: Option<&PluginSessionSnapshot>,
        authority: SessionAuthorityContext,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_with_parent_and_overlay(
            session_id,
            parent_session_id,
            snapshot,
            ToolCatalogContribution::default(),
            None,
            authority,
        )
    }

    pub fn build_session_with_parent_and_overlay(
        &self,
        session_id: impl Into<String>,
        parent_session_id: Option<String>,
        snapshot: Option<&PluginSessionSnapshot>,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
        authority: SessionAuthorityContext,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id,
            snapshot,
            tool_catalog_overlay,
            tool_snapshot,
            authority,
        })
    }

    pub fn build_session_with_overlay(
        &self,
        session_id: impl Into<String>,
        snapshot: Option<&PluginSessionSnapshot>,
        tool_catalog_overlay: ToolCatalogContribution,
        tool_snapshot: Option<crate::ToolState>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        self.build_session_inner(BuildPluginSessionRequest {
            session_id: session_id.into(),
            parent_session_id: None,
            snapshot,
            tool_catalog_overlay,
            tool_snapshot,
            authority: SessionAuthorityContext::default(),
        })
    }

    fn build_session_inner(
        &self,
        request: BuildPluginSessionRequest<'_>,
    ) -> Result<Arc<PluginSession>, PluginError> {
        let BuildPluginSessionRequest {
            session_id,
            parent_session_id,
            snapshot,
            tool_catalog_overlay,
            tool_snapshot,
            authority,
        } = request;
        let ctx = PluginSessionContext {
            session_id,
            tool_access: authority.tool_access.clone(),
            subagent: authority.subagent.clone(),
            plugin_options: authority.plugin_options.clone(),
            extensions: self.extensions.clone(),
            parent_session_id,
        };
        let session_id = ctx.session_id.clone();
        let mut plugins = Vec::new();
        let mut reg = PluginRegistrar::new();
        for factory in self.factories() {
            let plugin = factory.build(&ctx)?;
            reg.registering_plugin_id = Some(plugin.id().to_string());
            plugin.register(&mut reg)?;
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
        let registry = match tool_snapshot {
            Some(snapshot) => Arc::new(
                crate::ToolRegistry::from_tool_providers_with_hidden_tools(
                    contributions.tool_providers.clone(),
                    authority.tool_access.hidden_tools.clone(),
                )
                .map_err(|err| {
                    PluginError::Registration(format!("failed to build tool registry: {err}"))
                })?
                .fork_with_state(snapshot)
                .map_err(|err| {
                    PluginError::Session(format!(
                        "tool state cannot be applied to this plugin host session: {err}"
                    ))
                })?,
            ),
            None => Arc::new(
                crate::ToolRegistry::from_tool_providers_with_hidden_tools(
                    contributions.tool_providers.clone(),
                    authority.tool_access.hidden_tools.clone(),
                )
                .map_err(|err| {
                    PluginError::Registration(format!("failed to build tool registry: {err}"))
                })?,
            ),
        };
        let tools = Arc::clone(&registry) as Arc<dyn ToolProvider>;

        let session = Arc::new(PluginSession {
            host: self.clone(),
            session_id: ctx.session_id,
            plugins,
            tools,
            tool_registry: registry,
            tool_catalog_overlay,
            tool_access: authority.tool_access,
            subagent: authority.subagent,
            extensions: self.extensions.clone(),
            triggers,
            contributions,
        });
        self.register_session(&session_id, &session)?;
        let ready = SessionReadyContext {
            session_id: session.session_id.clone(),
            host: self.clone(),
        };
        for plugin in &session.plugins {
            plugin.session_ready(ready.clone())?;
        }
        if let Some(snapshot) = snapshot {
            session.restore(snapshot)?;
        }
        Ok(session)
    }

    fn register_session(
        &self,
        session_id: &str,
        session: &Arc<PluginSession>,
    ) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            PluginError::Session("plugin host session registry poisoned".to_string())
        })?;
        if let Some(existing) = sessions.get(session_id).and_then(Weak::upgrade) {
            if !Arc::ptr_eq(&existing, session) {
                return Err(PluginError::Session(format!(
                    "session `{session_id}` is already registered on this plugin host"
                )));
            }
            return Ok(());
        }
        sessions.insert(session_id.to_string(), Arc::downgrade(session));
        Ok(())
    }

    pub fn unregister_session(&self, session_id: &str) -> Result<(), PluginError> {
        let mut sessions = self.sessions.lock().map_err(|_| {
            PluginError::Session("plugin host session registry poisoned".to_string())
        })?;
        sessions.remove(session_id);
        Ok(())
    }

    pub fn session(
        &self,
        session_id: &str,
    ) -> Result<Arc<PluginSession>, PluginOperationInvokeError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| PluginOperationInvokeError::SessionRegistryPoisoned)?;
        let Some(weak) = sessions.get(session_id).cloned() else {
            return Err(PluginOperationInvokeError::UnknownSession(
                session_id.to_string(),
            ));
        };
        match weak.upgrade() {
            Some(session) => Ok(session),
            None => {
                sessions.remove(session_id);
                Err(PluginOperationInvokeError::UnknownSession(
                    session_id.to_string(),
                ))
            }
        }
    }
}
