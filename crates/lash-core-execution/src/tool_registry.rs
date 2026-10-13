//! The tool providers a session or a process env registers, each under the
//! key its plugin or component declared it under (FIG-6073), and the routed
//! view of a run's recorded surface every call executes through (FIG-6074).
//!
//! Which tools exist is each provider's own configuration: a provider
//! advertises under the run's resolved config, and nothing here selects,
//! curates or remembers tools. A run's surface is built once from those
//! advertisements and recorded; a call executes on exactly the entry it was
//! admitted under, by that entry's provider, and on nothing else.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::plugin::OwnerConfigView;
use crate::{
    PreparedToolCall, ProviderKey, RecordedSurface, SurfaceEntry, ToolCall, ToolContract,
    ToolDefinition, ToolId, ToolManifest, ToolOutcome, ToolPrepareCall, ToolProvider,
};

pub use lash_core_store::tool_surface::ReconfigureError;

/// Project every surface tool to a JSON record for host-owned discovery
/// (e.g. the production `tools.search` path in agent-workbench).
#[expect(
    clippy::expect_used,
    reason = "`projected` is built one line above by a `json!` object literal, so it is always a JSON object"
)]
pub(crate) fn project_tool_catalog<I>(entries: I) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = crate::ToolCatalogEntry>,
{
    entries
        .into_iter()
        .map(|entry| {
            let manifest = entry.manifest;
            let compact_contract = entry.contract.compact_contract_shared(&manifest);
            let mut projected = serde_json::json!({
                "id": manifest.id,
                "name": manifest.name,
                "description": manifest.description,
                "bindings": manifest.bindings,
                "inline": manifest.inline,
            });
            projected
                .as_object_mut()
                .expect("projected tool catalog entry is an object")
                .insert("contract".to_string(), serde_json::json!(compact_contract));
            projected
        })
        .collect()
}

pub use lash_core_store::tool_surface::effect_path_of;

/// The tool providers of one session or process env, by declared key, and
/// its deferred provider, if any.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    providers: BTreeMap<ProviderKey, Arc<dyn ToolProvider>>,
    deferred: Option<(ProviderKey, Arc<dyn crate::DeferredToolProvider>)>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_list()
            .entries(self.providers.keys())
            .finish()
    }
}

impl ToolRegistry {
    /// A registry of the one provider declared as `identity`.
    pub fn from_tool_provider(
        identity: ProviderKey,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<Self, ReconfigureError> {
        Self::from_tool_registrations(vec![(identity, provider)], None)
    }

    /// A registry of every registered provider, keyed by the identity its
    /// plugin or component declared it under. A key registered twice is
    /// refused.
    pub(crate) fn from_tool_registrations(
        providers: Vec<(ProviderKey, Arc<dyn ToolProvider>)>,
        deferred: Option<(ProviderKey, Arc<dyn crate::DeferredToolProvider>)>,
    ) -> Result<Self, ReconfigureError> {
        let mut registry = Self {
            providers: BTreeMap::new(),
            deferred,
        };
        for (key, provider) in providers {
            if registry.providers.contains_key(&key) {
                return Err(ReconfigureError::Validation(format!(
                    "tool provider `{key}` is registered twice"
                )));
            }
            registry.providers.insert(key, provider);
        }
        Ok(registry)
    }

    /// The provider registered under `key`.
    pub fn provider(&self, key: &ProviderKey) -> Option<&Arc<dyn ToolProvider>> {
        self.providers.get(key)
    }

    /// The deferred provider's key, when one is registered.
    pub fn deferred_key(&self) -> Option<&ProviderKey> {
        self.deferred.as_ref().map(|(key, _)| key)
    }

    /// The deferred provider registered under `key`: what a surface that
    /// recorded `key` as its deferred provider resolves through.
    pub fn deferred_provider(
        &self,
        key: &ProviderKey,
    ) -> Option<&Arc<dyn crate::DeferredToolProvider>> {
        self.deferred
            .as_ref()
            .filter(|(registered, _)| registered == key)
            .map(|(_, provider)| provider)
    }

    /// Whether a provider is registered under `key`.
    pub fn serves(&self, key: &ProviderKey) -> bool {
        self.providers.contains_key(key)
    }

    /// The registered keys, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &ProviderKey> {
        self.providers.keys()
    }

    /// Every provider's advertisement under `config`, in key order: each
    /// advertised tool with its whole definition and its provider. A tool a
    /// provider advertises twice, or advertises without a contract, is
    /// refused.
    ///
    /// # Errors
    ///
    /// The first refused advertisement.
    pub fn advertise(
        &self,
        config: &OwnerConfigView,
    ) -> Result<Vec<SurfaceEntry>, crate::PluginError> {
        let mut entries = Vec::new();
        for (key, provider) in &self.providers {
            config.validate_provider(key)?;
            let mut ids = std::collections::BTreeSet::new();
            for manifest in provider.tool_manifests(config)? {
                if !ids.insert(manifest.id.clone()) {
                    return Err(crate::PluginError::ResidentToolDuplicateId {
                        tool_id: manifest.id,
                    });
                }
                let definition = served_definition(provider.as_ref(), manifest, config)?;
                entries.push(SurfaceEntry {
                    provider: key.clone(),
                    definition,
                });
            }
        }
        Ok(entries)
    }

    /// The definition `provider` serves `tool` as under `config`, or `None`
    /// when no provider is registered under that key or it does not serve
    /// the tool: what a recorded call is judged against before it runs.
    pub fn served(
        &self,
        provider: &ProviderKey,
        tool: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolDefinition> {
        let provider = self.providers.get(provider)?;
        let manifest = provider
            .resolve_manifest_by_id(tool, config)
            .filter(|manifest| manifest.id == *tool)?;
        served_definition(provider.as_ref(), manifest, config).ok()
    }
}

/// `manifest` as its provider serves it under `config`, with its contract.
fn served_definition(
    provider: &dyn ToolProvider,
    mut manifest: ToolManifest,
    config: &OwnerConfigView,
) -> Result<ToolDefinition, crate::PluginError> {
    let contract = provider
        .resolve_contract_by_id(&manifest.id, config)
        .ok_or_else(|| crate::PluginError::ResidentToolContractUnavailable {
            tool_id: manifest.id.clone(),
            name: manifest.name.clone(),
        })?;
    if manifest.compact_contract.is_none() {
        manifest.compact_contract = Some(contract.compact_contract_shared(&manifest));
    }
    Ok(ToolDefinition {
        manifest,
        contract: (*contract).clone(),
    })
}

/// How a tool of a run's recorded surface differs from what its provider
/// serves on this node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSurfaceDriftKind {
    /// The recorded provider does not serve the tool here: it is not
    /// registered, or it does not advertise the tool under the run's config.
    Missing,
    /// The provider serves the tool with another dispatch surface.
    Changed,
}

impl ToolSurfaceDriftKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Missing => "missing from",
            Self::Changed => "changed in",
        }
    }
}

/// One tool of a recorded surface whose served definition drifted on this
/// node.
///
/// Drift is judged per call, structurally, on what decides how a call links
/// and dispatches ([`crate::tool_dispatch_surface`]); a reworded description
/// or new examples are not drift, since the prompt is served from the
/// recorded surface. A node executes a recorded match only if it serves the
/// exact retained surface; outcomes may differ between two builds while
/// they coexist.
#[derive(Clone, Debug)]
pub struct ToolSurfaceDrift {
    pub kind: ToolSurfaceDriftKind,
    pub recorded: ToolDefinition,
}

impl ToolSurfaceDrift {
    /// Judges `recorded` against `served`, what its provider serves it as on
    /// this node: `None` when it serves it undrifted.
    pub fn judge(recorded: &ToolDefinition, served: Option<&ToolDefinition>) -> Option<Self> {
        let kind = match served {
            None => ToolSurfaceDriftKind::Missing,
            Some(served)
                if crate::tool_dispatch_surface(&served.manifest, &served.contract)
                    != crate::tool_dispatch_surface(&recorded.manifest, &recorded.contract) =>
            {
                ToolSurfaceDriftKind::Changed
            }
            Some(_) => return None,
        };
        Some(Self {
            kind,
            recorded: recorded.clone(),
        })
    }

    /// The refusal a call on the drifted tool meets: nothing was dispatched,
    /// and the call parks, typed, as a code cell's drifted binding does.
    pub fn refusal(&self, call_id: &crate::ToolCallId) -> crate::RuntimeEffectControllerError {
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::LashVmCellBindingDrift,
            format!(
                "tool call `{call_id}` names tool `{}`, which is {} what this node's provider \
                 serves since the run's tool surface was recorded; nothing was dispatched",
                self.recorded.manifest.id,
                self.kind.describe(),
            ),
        )
    }
}

/// The calls a run's recorded surface or a process's links admit, routed:
/// each tool to the provider its entry records, and a granted deferred match
/// to the provider the match names. It is the one provider a dispatch
/// executes through.
#[derive(Clone, Default)]
pub struct SurfaceTools {
    routes: Arc<BTreeMap<ToolId, SurfaceEntry>>,
    deferred: Option<ProviderKey>,
    registry: Arc<ToolRegistry>,
}

impl SurfaceTools {
    /// The tools `surface` offers, routed through `registry`, and the
    /// deferred provider it recorded.
    pub fn of_surface(surface: &RecordedSurface, registry: Arc<ToolRegistry>) -> Self {
        let mut tools = Self::of_entries(surface.tools().values().cloned(), registry);
        tools.deferred = surface.deferred().cloned();
        tools
    }

    /// `entries`, each routed through `registry` to its provider.
    pub fn of_entries(
        entries: impl IntoIterator<Item = SurfaceEntry>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            routes: Arc::new(
                entries
                    .into_iter()
                    .map(|entry| (entry.tool().clone(), entry))
                    .collect(),
            ),
            deferred: None,
            registry,
        }
    }

    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// The deferred provider the surface recorded, as this node registers
    /// it: `Ok(None)` when the surface recorded none.
    ///
    /// # Errors
    ///
    /// The key the surface recorded, when this node does not register it as
    /// its deferred provider: an unserved provider, never "no provider".
    #[expect(
        clippy::type_complexity,
        reason = "the recorded key and its provider travel together"
    )]
    pub fn deferred_provider(
        &self,
    ) -> Result<Option<(&ProviderKey, &Arc<dyn crate::DeferredToolProvider>)>, ProviderKey> {
        let Some(key) = self.deferred.as_ref() else {
            return Ok(None);
        };
        self.registry
            .deferred_provider(key)
            .map(|provider| Some((key, provider)))
            .ok_or_else(|| key.clone())
    }

    /// Every routed entry, in tool id order.
    pub fn entries(&self) -> impl Iterator<Item = &SurfaceEntry> {
        self.routes.values()
    }

    /// The entry a call of `tool` is admitted under.
    pub fn entry(&self, tool: &ToolId) -> Option<&SurfaceEntry> {
        self.routes.get(tool)
    }

    /// How `tool`'s recorded entry drifted on this node under `config`, if
    /// it did; `None` for an undrifted tool or one no entry offers.
    pub fn drift(&self, tool: &ToolId, config: &OwnerConfigView) -> Option<ToolSurfaceDrift> {
        let entry = self.routes.get(tool)?;
        self.drift_match(&entry.provider, &entry.definition, config)
    }

    /// Judges a retained match, including a deferred match outside the
    /// surface, against the definition its recorded provider serves here.
    pub(crate) fn drift_match(
        &self,
        provider: &ProviderKey,
        recorded: &ToolDefinition,
        config: &OwnerConfigView,
    ) -> Option<ToolSurfaceDrift> {
        let served = self
            .registry
            .served(provider, &recorded.manifest.id, config);
        ToolSurfaceDrift::judge(recorded, served.as_ref())
    }

    fn by_name(&self, name: &str) -> Option<&SurfaceEntry> {
        self.routes
            .values()
            .find(|entry| entry.definition.manifest.name == name)
    }

    fn route(
        &self,
        tool: &ToolId,
        route: &crate::tool_provider::ToolExecutionRoute,
    ) -> Result<Arc<dyn ToolProvider>, ToolOutcome> {
        let key = match route {
            crate::tool_provider::ToolExecutionRoute::Catalog => {
                &self
                    .routes
                    .get(tool)
                    .ok_or_else(|| ToolOutcome::err_fmt(format_args!("Unknown tool id: {tool}")))?
                    .provider
            }
            crate::tool_provider::ToolExecutionRoute::Granted { provider } => provider,
        };
        self.registry.provider(key).cloned().ok_or_else(|| {
            ToolOutcome::err_fmt(format_args!(
                "Tool provider `{key}` of tool id `{tool}` is not registered"
            ))
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for SurfaceTools {
    fn tool_manifests(
        &self,
        _config: &OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, crate::PluginError> {
        Ok({
            self.routes
                .values()
                .map(|entry| entry.definition.manifest.clone())
                .collect()
        })
    }

    fn resolve_manifest(&self, name: &str, _config: &OwnerConfigView) -> Option<ToolManifest> {
        self.by_name(name)
            .map(|entry| entry.definition.manifest.clone())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        _config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.routes
            .get(id)
            .map(|entry| entry.definition.manifest.clone())
    }

    fn resolve_contract(&self, name: &str, _config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.by_name(name)
            .map(|entry| Arc::new(entry.definition.contract.clone()))
    }

    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        _config: &OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.routes
            .get(id)
            .map(|entry| Arc::new(entry.definition.contract.clone()))
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        let provider = self.route(&call.tool_id, call.context.execution_route())?;
        provider.prepare_tool_call(call).await
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let provider = match self.route(call.tool_id(), call.context.execution_route()) {
            Ok(provider) => provider,
            Err(outcome) => return outcome.into(),
        };
        provider.execute(call).await
    }
}
