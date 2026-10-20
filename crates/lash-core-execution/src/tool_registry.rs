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
/// (e.g. the production `tools.search` path in agent-workbench): its
/// manifest's identity and its contract as data. How a tool reads to a model
/// is the execution mode's tool presenter's, never this record's.
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
            let mut projected = serde_json::json!({
                "id": manifest.id,
                "name": manifest.tool(),
                "description": manifest.description,
                "bindings": manifest.bindings,
                "inline": manifest.inline,
            });
            projected
                .as_object_mut()
                .expect("projected tool catalog entry is an object")
                .insert("contract".to_string(), serde_json::json!(entry.contract));
            projected
        })
        .collect()
}

/// The tool providers of one session or process env, by declared key, and
/// its deferred provider, if any.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    providers: BTreeMap<ProviderKey, Arc<dyn ToolProvider>>,
    deferred: Option<(ProviderKey, Arc<dyn crate::DeferredToolProvider>)>,
    /// The transforms of the tools the deferred provider's registration
    /// grants dispatch, by tool.
    dispatch: BTreeMap<ToolId, crate::DispatchTransform>,
    /// Each provider's reader, when it registered one.
    reads: BTreeMap<ProviderKey, Arc<dyn crate::ToolReads>>,
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
        Self::from_tool_registrations(
            vec![(identity, provider)],
            None,
            BTreeMap::new(),
            Vec::new(),
        )
    }

    /// A registry of every registered provider, keyed by the identity its
    /// plugin or component declared it under, with each provider's reader
    /// and the dispatch grants of the deferred provider's registration. A
    /// key registered twice, or a reader of no registered provider, is
    /// refused.
    pub(crate) fn from_tool_registrations(
        providers: Vec<(ProviderKey, Arc<dyn ToolProvider>)>,
        deferred: Option<(ProviderKey, Arc<dyn crate::DeferredToolProvider>)>,
        dispatch: BTreeMap<ToolId, crate::DispatchTransform>,
        reads: Vec<(ProviderKey, Arc<dyn crate::ToolReads>)>,
    ) -> Result<Self, ReconfigureError> {
        let mut registry = Self {
            providers: BTreeMap::new(),
            deferred,
            dispatch,
            reads: BTreeMap::new(),
        };
        for (key, provider) in providers {
            if registry.providers.contains_key(&key) {
                return Err(ReconfigureError::Validation(format!(
                    "tool provider `{key}` is registered twice"
                )));
            }
            registry.providers.insert(key, provider);
        }
        for (key, reads) in reads {
            if !registry.providers.contains_key(&key) || registry.reads.contains_key(&key) {
                return Err(ReconfigureError::Validation(format!(
                    "the reader of tool provider `{key}` is registered without its provider or twice"
                )));
            }
            registry.reads.insert(key, reads);
        }
        Ok(registry)
    }

    /// The provider registered under `key`.
    pub fn provider(&self, key: &ProviderKey) -> Option<&Arc<dyn ToolProvider>> {
        self.providers.get(key)
    }

    /// The reader the provider registered under `key` registered beside
    /// itself: what answers a cell's reads of its `Read` tools.
    pub fn reads(&self, key: &ProviderKey) -> Option<&Arc<dyn crate::ToolReads>> {
        self.reads.get(key)
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

    /// The transform of `tool`, when the deferred provider registered under
    /// `key` grants it dispatch: what a granted call's target is computed
    /// by, wherever the call runs.
    pub fn dispatch_transform(
        &self,
        key: &ProviderKey,
        tool: &ToolId,
    ) -> Option<&crate::DispatchTransform> {
        self.deferred_provider(key)?;
        self.dispatch.get(tool)
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
    ) -> Result<Vec<SurfaceEntry>, lash_execution_contracts::plugin_error::PluginError> {
        let mut entries = Vec::new();
        for (key, provider) in &self.providers {
            config.validate_provider(key)?;
            let mut ids = std::collections::BTreeSet::new();
            for manifest in provider.tool_manifests(config)? {
                if !ids.insert(manifest.id.clone()) {
                    return Err(lash_execution_contracts::plugin_error::PluginError::ResidentToolDuplicateId {
                        tool_id: manifest.id,
                    });
                }
                // A read is a valid one, and its provider reads it: it never
                // falls back to the provider's recorded execution.
                if let Some(refusal) = manifest.read_refusal() {
                    return Err(lash_execution_contracts::plugin_error::PluginError::ToolRegistrationRefused {
                        source: Box::new(refusal),
                    });
                }
                if manifest.execution_policy == crate::ExecutionPolicy::Read
                    && !self.reads.contains_key(key)
                {
                    return Err(lash_execution_contracts::plugin_error::PluginError::ToolRegistrationRefused {
                        source: Box::new(
                            lash_protocol_types::RegistrationRefused::ReadWithoutReader {
                                tool: manifest.tool(),
                                provider: key.to_string(),
                            },
                        ),
                    });
                }
                let payload = provider
                    .link_payload(&manifest, config)
                    .map(|(format, payload)| crate::ExecutionBinding {
                        codec: key.clone(),
                        format,
                        payload,
                    });
                let definition = served_definition(provider.as_ref(), manifest, config)?;
                entries.push(SurfaceEntry::advertised(key.clone(), definition, payload));
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
    manifest: ToolManifest,
    config: &OwnerConfigView,
) -> Result<ToolDefinition, lash_execution_contracts::plugin_error::PluginError> {
    let contract = provider
        .resolve_contract_by_id(&manifest.id, config)
        .ok_or_else(|| {
            lash_execution_contracts::plugin_error::PluginError::ResidentToolContractUnavailable {
                tool_id: manifest.id.clone(),
                operation: manifest.tool(),
            }
        })?;
    Ok(ToolDefinition {
        manifest,
        contract: (*contract).clone(),
    })
}

/// How a pinned call's linked tool is served on this node, when it is not
/// served under the pinned contract ([`crate::judge`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolSurfaceDriftKind {
    /// The recorded provider is not registered here, a catalogue tool is
    /// absent under the run's config, or a deferred grant's recorded
    /// deferred provider or payload codec is not registered here. The call
    /// parks.
    Missing,
    /// The provider serves the tool under another contract id than the
    /// call is pinned to. The call's `perform` raises
    /// `operation_contract_changed`, and no body runs.
    ContractChanged {
        pinned: lash_kernel_doc::ContractId,
        live: lash_kernel_doc::ContractId,
    },
}

impl ToolSurfaceDriftKind {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Missing => "missing from",
            Self::ContractChanged { .. } => "under another contract in",
        }
    }
}

/// One pinned tool of a recorded surface that this node does not serve
/// under its pin.
///
/// Drift is judged per call by the one judge ([`crate::judge`]): the live
/// tool's contract id must equal the pinned one, and nothing else about it
/// is compared. A reworded description, a new transport or a changed retry
/// bound is not drift; the call runs under the policy its link recorded.
#[derive(Clone, Debug)]
pub struct ToolSurfaceDrift {
    pub kind: ToolSurfaceDriftKind,
    pub recorded: ToolDefinition,
}

impl ToolSurfaceDrift {
    /// The drift `judgement` found for the call of `recorded`: `None` when
    /// the tool is served under its pin.
    pub fn of(recorded: &ToolDefinition, judgement: crate::Judgement) -> Option<Self> {
        let kind = match judgement {
            crate::Judgement::Served => return None,
            crate::Judgement::Missing => ToolSurfaceDriftKind::Missing,
            crate::Judgement::ContractChanged { pinned, live } => {
                ToolSurfaceDriftKind::ContractChanged { pinned, live }
            }
        };
        Some(Self {
            kind,
            recorded: recorded.clone(),
        })
    }

    /// Whether the linked tool is missing here: the call parks.
    pub fn is_missing(&self) -> bool {
        self.kind == ToolSurfaceDriftKind::Missing
    }

    /// The refusal a call on the missing tool meets: nothing was
    /// dispatched, and the call parks, typed, as a code cell's missing
    /// binding does.
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

    /// The failure the call's `perform` of `effect` raises when its tool is
    /// served under another contract; `None` for a missing tool.
    pub fn failure(&self, effect: &str) -> Option<crate::ToolFailure> {
        let ToolSurfaceDriftKind::ContractChanged { pinned, live } = &self.kind else {
            return None;
        };
        Some(operation_contract_changed(
            effect,
            &self.recorded.manifest.id,
            pinned,
            live,
        ))
    }
}

/// The failure a `perform` of `effect` raises when its linked `tool` is
/// served under the contract `live`, not the `pinned` one its call is
/// pinned to: `operation_contract_changed`, which the program may catch.
/// No body ran.
pub fn operation_contract_changed(
    effect: &str,
    tool: &ToolId,
    pinned: &lash_kernel_doc::ContractId,
    live: &lash_kernel_doc::ContractId,
) -> crate::ToolFailure {
    crate::ToolFailure::runtime(
        crate::ToolFailureClass::InvalidRequest,
        "operation_contract_changed",
        format!(
            "`{effect}` is linked to tool `{tool}`, which this node's provider serves under \
             contract `{live}`, not the contract `{pinned}` the call is pinned to; nothing ran"
        ),
    )
    .with_cause(crate::ToolFailureCause::OperationContractChanged {
        effect: effect.to_owned(),
        tool: tool.clone(),
        pinned: pinned.clone(),
        live: live.clone(),
    })
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
    /// it did, for a call pinned to `pinned`: the contract the performing
    /// code requires, or, for a model round's call, which no code performs,
    /// the one the entry's recorded definition serves. `None` for an
    /// undrifted tool or one no entry offers.
    pub fn drift(
        &self,
        tool: &ToolId,
        pinned: Option<&lash_kernel_doc::ContractId>,
        config: &OwnerConfigView,
    ) -> Option<ToolSurfaceDrift> {
        let entry = self.routes.get(tool)?;
        self.drift_entry(entry, pinned, config)
    }

    /// Where a read of `tool` goes on this node under `config`: the
    /// provider its entry retained, with that provider's reader.
    ///
    /// # Errors
    ///
    /// [`RuntimeErrorCode::LashVmCellBindingDrift`](crate::RuntimeErrorCode::LashVmCellBindingDrift):
    /// no entry offers `tool`, its provider serves it otherwise here, or
    /// registers no reader here. Nothing was read.
    pub fn read_route(
        &self,
        tool: &ToolId,
        pinned: &lash_kernel_doc::ContractId,
        config: &OwnerConfigView,
    ) -> Result<crate::ReadRoute, crate::RuntimeEffectControllerError> {
        let refused = |why: String| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::LashVmCellBindingDrift,
                format!("a read of tool `{tool}` {why}; nothing was read"),
            )
        };
        let entry = self
            .routes
            .get(tool)
            .ok_or_else(|| refused("names no tool of the run's surface".to_owned()))?;
        let drift = self.drift_entry(entry, Some(pinned), config);
        if let Some(drift) = drift.as_ref().filter(|drift| drift.is_missing()) {
            return Err(refused(format!(
                "names a tool {} what this node's provider serves since the run's tool surface \
                 was recorded",
                drift.kind.describe()
            )));
        }
        let reads = self.registry.reads(entry.provider()).ok_or_else(|| {
            refused(format!(
                "goes to provider `{}`, which registers no reader on this node",
                entry.provider()
            ))
        })?;
        Ok(crate::ReadRoute::new(
            entry.provider().clone(),
            entry.definition.clone(),
            Arc::clone(reads),
        )
        .with_drift(drift.and_then(|drift| drift.failure(&entry.definition.manifest.tool()))))
    }

    /// A catalogue entry is judged by the one judge against what its
    /// provider serves now, pinned to `pinned`, else to the contract of the
    /// definition the entry recorded. Deferred grants never take this path.
    fn drift_entry(
        &self,
        entry: &SurfaceEntry,
        pinned: Option<&lash_kernel_doc::ContractId>,
        config: &OwnerConfigView,
    ) -> Option<ToolSurfaceDrift> {
        let served = self.registry.served(entry.provider(), entry.tool(), config);
        let pinned = pinned
            .cloned()
            .unwrap_or_else(|| entry.definition.contract_id());
        ToolSurfaceDrift::of(
            &entry.definition,
            crate::judge(&pinned, &entry.link, served.as_ref()),
        )
    }

    /// A deferred grant's committed definition is its authority, never a
    /// second catalogue lookup. Only registration and binding readability
    /// can make this node unable to run it, including on cell restoration.
    pub(crate) fn require_deferred_grant(
        &self,
        grant: &crate::ToolExecutionGrant,
        call: &crate::ToolCallId,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let missing = || {
            ToolSurfaceDrift {
                kind: ToolSurfaceDriftKind::Missing,
                recorded: grant.definition().clone(),
            }
            .refusal(call)
        };
        if !self.registry.serves(grant.provider())
            || !matches!(self.deferred_provider(), Ok(Some(_)))
        {
            return Err(missing());
        }
        if let Some(payload) = grant.payload() {
            let codec = self.registry.provider(&payload.codec).ok_or_else(missing)?;
            payload
                .check_readable(codec.readable_binding_formats())
                .map_err(|source| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::ExecutionBindingUnreadable,
                        source.to_string(),
                    )
                })?;
        }
        Ok(())
    }

    fn by_name(&self, name: &str) -> Option<&SurfaceEntry> {
        self.routes
            .values()
            .find(|entry| entry.definition.manifest.tool() == name)
    }

    /// The handle of the provider `tool` routes to under `route`.
    fn route_handle<'a>(
        &'a self,
        tool: &ToolId,
        route: &'a crate::tool_provider::ToolExecutionRoute,
    ) -> Option<&'a crate::plugin::ToolProviderHandle> {
        match route {
            crate::tool_provider::ToolExecutionRoute::Catalog => {
                self.routes.get(tool).map(SurfaceEntry::provider)
            }
            crate::tool_provider::ToolExecutionRoute::Granted { provider } => Some(provider),
        }
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
                    .link
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
    ) -> std::result::Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
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
        // The body's capability is bound from the registered provider the
        // call routes to, never from anything the call names.
        if let Some(handle) = self.route_handle(call.tool_id(), call.context.execution_route())
            && let Some(callback) = crate::trace::BoundCallback::of(&handle.callback_identity())
        {
            call.context.trace().bind(callback);
        }
        provider.execute(call).await
    }
}
