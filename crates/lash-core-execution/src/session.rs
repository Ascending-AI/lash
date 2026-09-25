use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, OnceLock};

use crate::PluginMessage;
use crate::tool_dispatch::ToolDispatchContext;
use crate::{PromptContribution, RuntimeServices, ToolProvider};

mod execution_context;
mod opener_groups;
mod process_handles;
mod settlement_incorporation;
#[cfg(test)]
mod settlement_incorporation_tests;
pub(crate) mod tool_execution;

pub use execution_context::RuntimeExecutionContext;
pub use execution_context::{RuntimeExecutionProcessEventContext, RuntimeExecutionTracing};
pub(crate) use execution_context::{
    attach_process_invocation_correlation, clear_process_invocation_correlation,
};
pub use opener_groups::{OpenerGroupRegistry, OpenerGroupsClosed, OpenerState, OpenerWorkBound};
pub use settlement_incorporation::{
    ContextFinalizationSteps, Incorporated, IncorporationLedger, SettlementSource, UsageChargeSink,
    UsageDeltaIdentity,
};
/// Runtime tool invocation requests and their collected replies.
pub use tool_execution::{
    ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateLeafReply, ToolAggregateOutcome,
    ToolAggregateRequest, ToolBatchReplies, ToolInvocation, ToolInvocationReply,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolCatalogCacheKey {
    context_overlay_revision: u64,
    tool_generation: u64,
    plugin_generations: std::collections::BTreeMap<String, u64>,
    authority_fingerprint: [u8; 32],
}

#[derive(Debug, Default)]
struct ToolCatalogDerived {
    catalog: OnceLock<Arc<Vec<serde_json::Value>>>,
}

struct ToolCatalogArtifact {
    tool_registry: Arc<crate::ToolRegistry>,
    tool_catalog: Arc<crate::ToolCatalog>,
    /// The catalog the live registry resolves to now. It is `tool_catalog`
    /// for a live surface; for a recorded surface it is what the recorded
    /// definitions are judged against, tool by tool.
    live_tool_catalog: Arc<crate::ToolCatalog>,
    preamble: Arc<crate::TurnDriverPreamble>,
    /// The recorded tools whose live definition is missing or dispatches
    /// differently; empty for a live surface.
    drift: std::collections::BTreeMap<crate::ToolId, ToolSurfaceDrift>,
    derived: ToolCatalogDerived,
}

/// How a tool of a turn's recorded surface differs from the live registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSurfaceDriftKind {
    /// The live registry holds no tool with the recorded tool's id.
    Missing,
    /// The live tool with the recorded id links or dispatches differently.
    Changed,
    /// A group tool child whose recorded surface holds no definition for its
    /// own tool: nothing recorded to judge the live tool by, so it is served
    /// only from the journal (FIG-3725).
    Unrecorded,
}

impl ToolSurfaceDriftKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Missing => "missing from",
            Self::Changed => "changed in",
            Self::Unrecorded => "unjudgeable against",
        }
    }
}

/// One tool of a turn's recorded surface whose live definition drifted.
///
/// Drift is judged per tool on what decides how a call links and dispatches
/// ([`tool_dispatch_surface`]); a reworded description or new examples are
/// not drift, since the prompt is served from the journaled sync.
#[derive(Clone, Debug)]
pub struct ToolSurfaceDrift {
    pub kind: ToolSurfaceDriftKind,
    pub recorded: crate::ToolDefinition,
}

impl ToolSurfaceDrift {
    /// Judges one recorded tool against `live`, the catalog the live registry
    /// resolves to, on what decides how a call links and dispatches
    /// ([`tool_dispatch_surface`]): `None` when the live catalog holds it
    /// undrifted. The one rule a turn's recorded surface and a group tool
    /// child's recorded admission are both judged by (FIG-3587, FIG-3725).
    pub fn judge(recorded: &crate::ToolDefinition, live: &crate::ToolCatalog) -> Option<Self> {
        let kind = match live
            .tools
            .iter()
            .find(|entry| entry.manifest.id == recorded.manifest.id)
        {
            None => ToolSurfaceDriftKind::Missing,
            Some(entry)
                if tool_dispatch_surface(&entry.manifest, &entry.contract)
                    != tool_dispatch_surface(&recorded.manifest, &recorded.contract) =>
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

    /// The grant a call on the drifted tool is authorized under: the recorded
    /// definition, so its envelope is the one the journal recorded.
    pub fn recorded_binding(&self) -> crate::ToolExecutionGrant {
        crate::ToolExecutionGrant::from_definition(self.recorded.clone())
    }

    /// The refusal a call on the drifted tool meets when the journal does not
    /// hold its result. It is the binding-drift refusal a code cell's drifted
    /// binding meets (FIG-3587), so the turn parks the same way.
    pub fn refusal(&self, call_id: &str) -> crate::RuntimeEffectControllerError {
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::LashlangCellBindingDrift,
            format!(
                "tool call `{call_id}` (tool `{}`) names a tool {} the live tool registry since \
                 the turn's tool surface was recorded; its journal serves only recorded \
                 results, and this call would reach the tool live, so nothing was dispatched",
                self.recorded.manifest.id,
                self.kind.describe(),
            ),
        )
    }
}

/// The part of a tool definition that decides how a call links and
/// dispatches: its identity, binding, activation, argument projection, retry
/// policy and schemas. The description and examples reach only the model's
/// prompt, which a redrive serves from the journaled environment sync.
#[derive(serde::Serialize, PartialEq)]
pub struct ToolDispatchSurface<'a> {
    id: &'a crate::ToolId,
    name: &'a str,
    bindings: &'a std::collections::BTreeMap<String, serde_json::Value>,
    activation: &'a crate::ToolActivation,
    argument_projection: &'a crate::ToolArgumentProjectionPolicy,
    retry_policy: &'a crate::ToolRetryPolicy,
    input_schema: &'a crate::SchemaContract,
    output_schema: &'a crate::SchemaContract,
    output_contract: &'a crate::ToolOutputContract,
}

/// The dispatch surface of `manifest` under `contract`.
pub fn tool_dispatch_surface<'a>(
    manifest: &'a crate::ToolManifest,
    contract: &'a crate::ToolContract,
) -> ToolDispatchSurface<'a> {
    ToolDispatchSurface {
        id: &manifest.id,
        name: &manifest.name,
        bindings: &manifest.bindings,
        activation: &manifest.activation,
        argument_projection: &manifest.argument_projection,
        retry_policy: &manifest.retry_policy,
        input_schema: &contract.input_schema,
        output_schema: &contract.output_schema,
        output_contract: &contract.output_contract,
    }
}

#[cfg(feature = "testing")]
#[derive(Clone)]
pub struct ToolCatalogHandle(Arc<ToolCatalogArtifact>);
#[cfg(not(feature = "testing"))]
#[derive(Clone)]
pub struct ToolCatalogHandle(Arc<ToolCatalogArtifact>);

type ToolContractFingerprints = Arc<Vec<[u8; 32]>>;
type CompositionToolFingerprintCache = Arc<
    std::sync::Mutex<
        Vec<(
            Arc<Vec<crate::llm::types::LlmToolSpec>>,
            ToolContractFingerprints,
        )>,
    >,
>;

impl ToolCatalogHandle {
    pub(crate) fn tool_registry(&self) -> Arc<crate::ToolRegistry> {
        Arc::clone(&self.0.tool_registry)
    }

    pub fn tools(&self) -> Arc<dyn ToolProvider> {
        Arc::clone(&self.0.tool_registry) as Arc<dyn ToolProvider>
    }

    pub fn tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.0.tool_catalog)
    }

    pub fn preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        Arc::clone(&self.0.preamble)
    }

    /// The catalog the live registry resolves to now; see
    /// [`ToolCatalogArtifact::live_tool_catalog`].
    pub fn live_tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.0.live_tool_catalog)
    }

    /// The catalog's tools as definitions: what an execution-environment
    /// sync records as the turn's tool surface.
    pub fn definitions(&self) -> Vec<crate::ToolDefinition> {
        self.0
            .tool_catalog
            .tools
            .iter()
            .map(|entry| crate::ToolDefinition {
                manifest: entry.manifest.clone(),
                contract: (*entry.contract).clone(),
            })
            .collect()
    }

    /// How `tool_id`'s live definition drifted from the recorded surface.
    pub fn drift_for(&self, tool_id: &crate::ToolId) -> Option<&ToolSurfaceDrift> {
        self.0.drift.get(tool_id)
    }

    fn catalog(&self) -> Arc<Vec<serde_json::Value>> {
        Arc::clone(self.0.derived.catalog.get_or_init(|| {
            Arc::new(crate::tool_registry::project_tool_catalog(
                self.0.tool_catalog.tools.iter().cloned(),
            ))
        }))
    }
}

#[derive(Clone, Debug)]
pub struct InjectedTurnInput {
    pub id: Option<String>,
    pub message: PluginMessage,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("code execution is not available in this session")]
    CodeExecutionUnavailable,
    #[error("code execution runtime exited unexpectedly")]
    CodeExecutionRuntimeStopped,
    #[error(
        "provider mismatch for session `{session_id}`: persisted provider `{expected}` does not match live provider `{actual}`"
    )]
    ProviderMismatch {
        expected: String,
        actual: String,
        session_id: SessionId,
    },
    #[error("provider is not configured for session `{session_id}`")]
    ProviderUnconfigured { session_id: SessionId },
    #[error("provider `{provider_id}` is not registered for session `{session_id}`")]
    ProviderUnavailable {
        provider_id: String,
        session_id: SessionId,
    },
    #[error("{context}: {source}")]
    Store {
        context: String,
        #[source]
        source: crate::StoreError,
    },
    #[error("session config command has not settled yet: {0}")]
    SessionCommandPending(crate::SessionCommandReceipt),
    #[error("session config command was cancelled before settlement: {0}")]
    SessionCommandCancelled(crate::SessionCommandReceipt),
    /// The session opened under [`ToolSourcePolicy::Require`](crate::ToolSourcePolicy)
    /// and a persisted Tool Catalog member had no registered source.
    ///
    /// The refusal guarantees no config or state commit, no protocol restore,
    /// no `SessionRestored` event, and a released Session Execution Lease. It
    /// does not claim zero side effects: observer-intent reconcile, the
    /// admitted load, plugin materialisation and `initialize_session` have
    /// already run by the time tool state is installed.
    #[error(
        "session `{session_id}` requires every persisted tool source: no registered source resolves {}",
        report
            .lost_members
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )]
    ToolSourcesUnavailable {
        session_id: SessionId,
        /// The full restore report, including the classes that did not refuse.
        report: Box<crate::ToolRestoreReport>,
    },
    #[error(transparent)]
    Plugin(#[from] crate::PluginError),
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<lash_core_store::session_policy::ProviderPinMismatch> for SessionError {
    fn from(value: lash_core_store::session_policy::ProviderPinMismatch) -> Self {
        Self::ProviderMismatch {
            expected: value.expected,
            actual: value.actual,
            session_id: value.session_id,
        }
    }
}

impl SessionError {
    /// The typed failure an `exec_code` effect journals: the closed
    /// [`crate::ExecCodeFailureReason`] beside the human message, so trace and
    /// replay analysis never has to parse the prose.
    pub fn to_exec_code_failure(&self) -> crate::ExecCodeFailure {
        let reason = match self {
            Self::CodeExecutionUnavailable => crate::ExecCodeFailureReason::ExecutorUnavailable,
            Self::CodeExecutionRuntimeStopped => crate::ExecCodeFailureReason::RuntimeStopped,
            _ => crate::ExecCodeFailureReason::Session,
        };
        crate::ExecCodeFailure::new(reason, self.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub language: String,
    pub code: String,
}

pub struct Session {
    session_id: SessionId,
    services: RuntimeServices,
    context_overlay_revision: u64,
    context_tools: Vec<Arc<dyn ToolProvider>>,
    tool_registry: Arc<crate::ToolRegistry>,
    context_prompt_contributions: Vec<PromptContribution>,
    tool_catalog_cache: Arc<std::sync::Mutex<Option<(ToolCatalogCacheKey, ToolCatalogHandle)>>>,
    composition_tool_fingerprint_cache: CompositionToolFingerprintCache,
    /// Memoizes the rendered system prompt across turns. Most consecutive
    /// turns reuse the same template + context overlay, so the cache hits
    /// and we skip the section/Vec-join work in
    /// `lash_sansio::PromptTemplate::render`.
    prompt_cache: Arc<lash_sansio::PromptCache>,
    /// Fingerprint of the last model-facing composition emitted to the trace
    /// sink for this resident session. Effect-driver clones share the slot so
    /// a mid-turn execution-environment refresh cannot double-emit it.
    composition_trace_fingerprint: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
}

impl Session {
    pub async fn new(
        services: RuntimeServices,
        session_id: &SessionId,
    ) -> Result<Self, SessionError> {
        let tool_registry = services.plugins.tool_registry();
        let mut session = Self {
            session_id: SessionId::from(session_id.to_string()),
            services,
            context_overlay_revision: 0,
            context_tools: Vec::new(),
            tool_registry,
            context_prompt_contributions: Vec::new(),
            tool_catalog_cache: Arc::new(std::sync::Mutex::new(None)),
            composition_tool_fingerprint_cache: Arc::new(std::sync::Mutex::new(Vec::new())),
            prompt_cache: Arc::new(lash_sansio::PromptCache::new()),
            composition_trace_fingerprint: Arc::new(std::sync::Mutex::new(None)),
        };

        let protocol_session = Arc::clone(session.plugins().protocol_session());
        protocol_session
            .initialize_session(crate::plugin::ProtocolSessionContext::new(
                &mut session,
                session_id,
            ))
            .await?;

        Ok(session)
    }

    pub fn clone_for_effect(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            services: self.services.clone(),
            context_overlay_revision: self.context_overlay_revision,
            context_tools: self.context_tools.clone(),
            tool_registry: Arc::clone(&self.tool_registry),
            context_prompt_contributions: self.context_prompt_contributions.clone(),
            tool_catalog_cache: Arc::clone(&self.tool_catalog_cache),
            composition_tool_fingerprint_cache: Arc::clone(
                &self.composition_tool_fingerprint_cache,
            ),
            prompt_cache: Arc::clone(&self.prompt_cache),
            composition_trace_fingerprint: Arc::clone(&self.composition_trace_fingerprint),
        }
    }

    pub fn record_composition_trace_fingerprint(&self, fingerprint: [u8; 32]) -> bool {
        let mut last = self.composition_trace_fingerprint.lock_recover();
        if last.as_ref() == Some(&fingerprint) {
            return false;
        }
        *last = Some(fingerprint);
        true
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn protocol_extra_prompt_contributions(&self) -> Vec<PromptContribution> {
        // Protocol-specific prompt contributions are owned by the protocol
        // plugins via their
        // `reg.prompt().contribute(...)` hooks. Nothing to add here.
        Vec::new()
    }

    pub fn tools(&self) -> Arc<dyn ToolProvider> {
        Arc::clone(&self.tool_registry) as Arc<dyn ToolProvider>
    }

    pub fn plugins(&self) -> &Arc<crate::PluginSession> {
        &self.services.plugins
    }

    pub fn set_context_overlay(
        &mut self,
        tool_providers: Vec<Arc<dyn ToolProvider>>,
        prompt_contributions: Vec<PromptContribution>,
    ) -> Result<(), crate::PluginError> {
        let tool_providers_unchanged = self.context_tools.len() == tool_providers.len()
            && self
                .context_tools
                .iter()
                .zip(&tool_providers)
                .all(|(current, next)| Arc::ptr_eq(current, next));
        let overlay_unchanged =
            self.context_prompt_contributions == prompt_contributions && tool_providers_unchanged;
        let registry = self
            .services
            .plugins
            .tool_registry()
            .compose_session_catalog(tool_providers.clone())
            .map(Arc::new)
            .map_err(|err| {
                crate::PluginError::Session(format!("failed to build session tool registry: {err}"))
            })?;
        if !overlay_unchanged {
            self.context_overlay_revision = self.context_overlay_revision.wrapping_add(1);
        }
        self.context_tools = tool_providers;
        self.tool_registry = registry;
        self.context_prompt_contributions = prompt_contributions;
        *self.tool_catalog_cache.lock_recover() = None;
        Ok(())
    }

    pub fn prompt_cache(&self) -> Arc<lash_sansio::PromptCache> {
        Arc::clone(&self.prompt_cache)
    }

    pub fn context_prompt_contributions(&self) -> &[PromptContribution] {
        &self.context_prompt_contributions
    }

    pub fn history_store(&self) -> Option<Arc<dyn crate::store::RuntimePersistence>> {
        self.services.store.clone()
    }

    fn tool_catalog_cache_key(
        &self,
        tool_access: &crate::SessionToolAccess,
        tool_generation: u64,
    ) -> ToolCatalogCacheKey {
        ToolCatalogCacheKey {
            context_overlay_revision: self.context_overlay_revision,
            tool_generation,
            plugin_generations: self.plugins().state_generations(),
            authority_fingerprint: tool_catalog_authority_fingerprint(tool_access),
        }
    }

    fn build_tool_catalog_entry(
        &self,
        session_id: &SessionId,
        tool_registry: Arc<crate::ToolRegistry>,
        tool_access: crate::SessionToolAccess,
        subagent: Option<crate::SubagentSessionContext>,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let tool_catalog = Arc::new(self.plugins().resolve_live_tool_catalog(
            session_id,
            Arc::clone(&tool_registry) as Arc<dyn ToolProvider>,
            tool_access,
            subagent,
        )?);
        tool_registry.validate_resident_catalog_routes(&tool_catalog)?;
        let input = crate::ProtocolBuildInput {
            tool_catalog: Arc::clone(&tool_catalog),
            plugin_extensions: self.plugins().extensions().clone(),
            trigger_events: self.plugins().triggers().clone(),
            extra_prompt_contributions: self.protocol_extra_prompt_contributions(),
        };
        let driver = self.plugins().protocol_driver();
        let preamble = driver.build_preamble(input);
        Ok(ToolCatalogHandle(Arc::new(ToolCatalogArtifact {
            tool_registry,
            live_tool_catalog: Arc::clone(&tool_catalog),
            tool_catalog,
            preamble: Arc::new(preamble),
            drift: std::collections::BTreeMap::new(),
            derived: ToolCatalogDerived::default(),
        })))
    }

    /// Installs the tool surface a turn's execution-environment sync
    /// recorded as the surface its drive reads (FIG-3672 P7b).
    ///
    /// The recorded definitions are the catalog: membership, manifests and
    /// contracts are what the pass that wrote the journal saw, whichever
    /// worker replays it. The live registry supplies only the executors, and
    /// each recorded tool is judged against it on its own
    /// ([`ToolSurfaceDrift`]): a call on a drifted tool is served only from
    /// its recorded result.
    pub fn install_recorded_tool_surface(
        &self,
        session_id: &SessionId,
        tool_access: &crate::SessionToolAccess,
        subagent: Option<&crate::SubagentSessionContext>,
        recorded: &[crate::ToolDefinition],
    ) -> Result<(), crate::PluginError> {
        let tool_registry = self.pin_live_tool_registry()?;
        let key = self.tool_catalog_cache_key(tool_access, tool_registry.generation());
        let live = self.build_tool_catalog_entry(
            session_id,
            Arc::clone(&tool_registry),
            tool_access.clone(),
            subagent.cloned(),
        )?;
        let live_tool_catalog = live.tool_catalog();
        let drift = recorded
            .iter()
            .filter_map(|definition| {
                ToolSurfaceDrift::judge(definition, &live_tool_catalog)
                    .map(|drift| (definition.manifest.id.clone(), drift))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(recorded.to_vec()));
        let preamble = self
            .plugins()
            .protocol_driver()
            .build_preamble(crate::ProtocolBuildInput {
                tool_catalog: Arc::clone(&tool_catalog),
                plugin_extensions: self.plugins().extensions().clone(),
                trigger_events: self.plugins().triggers().clone(),
                extra_prompt_contributions: self.protocol_extra_prompt_contributions(),
            });
        let handle = ToolCatalogHandle(Arc::new(ToolCatalogArtifact {
            tool_registry,
            tool_catalog,
            live_tool_catalog,
            preamble: Arc::new(preamble),
            drift,
            derived: ToolCatalogDerived::default(),
        }));
        *self.tool_catalog_cache.lock_recover() = Some((key, handle));
        Ok(())
    }

    /// The protocol driver's preamble for a turn machine whose environment its
    /// protocol-start sync supplies. It is built over an empty catalog: the
    /// driver's configuration is host configuration and does not depend on the
    /// tools, and the prompt and tool specs it would render are replaced by
    /// the recorded sync. The machine always syncs, since that sync is the
    /// only way the environment reaches it.
    pub fn protocol_driver_preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        let mut preamble =
            self.plugins()
                .protocol_driver()
                .build_preamble(crate::ProtocolBuildInput {
                    tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
                    plugin_extensions: self.plugins().extensions().clone(),
                    trigger_events: self.plugins().triggers().clone(),
                    extra_prompt_contributions: self.protocol_extra_prompt_contributions(),
                });
        preamble.config.sync_execution_environment = true;
        Arc::new(preamble)
    }

    /// How `tool_id` drifted from the turn's installed recorded surface, if it
    /// did; `None` for an undrifted tool or a live surface.
    pub fn tool_surface_drift(
        &self,
        session_id: &SessionId,
        tool_id: &crate::ToolId,
    ) -> Result<Option<ToolSurfaceDrift>, crate::PluginError> {
        Ok(self
            .active_tool_surface_entry(session_id)?
            .drift_for(tool_id)
            .cloned())
    }

    fn tool_catalog_cache_entry(
        &self,
        session_id: &SessionId,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let tool_access = self.plugins().tool_access();
        let subagent = self.plugins().subagent_context().cloned();
        let key = self.tool_catalog_cache_key(&tool_access, self.tool_registry.generation());
        let mut cache = self.tool_catalog_cache.lock_recover();
        if let Some((entry_key, entry)) = cache.as_ref()
            && *entry_key == key
        {
            return Ok(entry.clone());
        }
        let tool_registry = self.pin_live_tool_registry()?;
        let key = self.tool_catalog_cache_key(&tool_access, tool_registry.generation());
        let entry =
            self.build_tool_catalog_entry(session_id, tool_registry, tool_access, subagent)?;
        *cache = Some((key, entry.clone()));
        Ok(entry)
    }

    fn active_tool_surface_entry(
        &self,
        session_id: &SessionId,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        if let Some((_, entry)) = self.tool_catalog_cache.lock_recover().as_ref() {
            return Ok(entry.clone());
        }
        self.tool_catalog_cache_entry(session_id)
    }

    /// Live sources are enumerated exactly once before the registry snapshot is
    /// frozen. Host curation is already attached by ToolId in that snapshot;
    /// authority and plugin contributions then filter the model-facing names.
    /// The returned handle owns both the catalog and the registry dispatch will
    /// use for calls from that request.
    ///
    /// Building a surface installs nothing: an execution-environment sync
    /// records the surface it built, and the drive installs what the sync
    /// recorded ([`Self::install_recorded_tool_surface`]).
    // `ToolCatalogHandle` is only `pub` under the `testing` feature, so this
    // accessor's visibility tracks it exactly (`private_interfaces`).
    #[cfg(feature = "testing")]
    pub fn pin_tool_surface(
        &self,
        session_id: &SessionId,
        tool_access: &crate::SessionToolAccess,
        subagent: Option<&crate::SubagentSessionContext>,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        self.pin_tool_surface_inner(session_id, tool_access, subagent)
    }

    #[cfg(not(feature = "testing"))]
    pub fn pin_tool_surface(
        &self,
        session_id: &SessionId,
        tool_access: &crate::SessionToolAccess,
        subagent: Option<&crate::SubagentSessionContext>,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        self.pin_tool_surface_inner(session_id, tool_access, subagent)
    }

    fn pin_tool_surface_inner(
        &self,
        session_id: &SessionId,
        tool_access: &crate::SessionToolAccess,
        subagent: Option<&crate::SubagentSessionContext>,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let tool_registry = self.pin_live_tool_registry()?;
        self.build_tool_catalog_entry(
            session_id,
            tool_registry,
            tool_access.clone(),
            subagent.cloned(),
        )
    }

    fn pin_live_tool_registry(&self) -> Result<Arc<crate::ToolRegistry>, crate::PluginError> {
        self.plugins()
            .tool_registry()
            .pin_session_surface(self.context_tools.clone())
            .map(Arc::new)
            .map_err(|err| {
                crate::PluginError::Session(format!("failed to pin session tool surface: {err}"))
            })
    }

    pub fn resolved_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        Ok(self.active_tool_surface_entry(session_id)?.tool_catalog())
    }

    pub fn composition_tool_fingerprints(
        &self,
        tools: &Arc<Vec<crate::llm::types::LlmToolSpec>>,
    ) -> ToolContractFingerprints {
        let mut cache = self.composition_tool_fingerprint_cache.lock_recover();
        if let Some((_, fingerprints)) = cache.iter().find(|(cached_tools, _)| {
            Arc::ptr_eq(cached_tools, tools) || cached_tools.as_slice() == tools.as_slice()
        }) {
            return Arc::clone(fingerprints);
        }
        let fingerprints = Arc::new(
            tools
                .iter()
                .map(crate::trace::composition_tool_fingerprint)
                .collect(),
        );
        const MAX_COMPOSITION_FINGERPRINT_GENERATIONS: usize = 8;
        if cache.len() == MAX_COMPOSITION_FINGERPRINT_GENERATIONS {
            cache.remove(0);
        }
        cache.push((Arc::clone(tools), Arc::clone(&fingerprints)));
        fingerprints
    }

    pub fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        Ok(self.active_tool_surface_entry(session_id)?.catalog())
    }

    pub fn tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        Ok(self.shared_tool_catalog(session_id)?.as_ref().clone())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "code execution bridge carries explicit per-turn runtime dependencies"
    )]
    pub fn code_execution_context<'run>(
        &self,
        session_id: &SessionId,
        agent_frame_id: crate::FrameNodeId,
        sessions: Arc<dyn crate::plugin::SessionStateService>,
        session_lifecycle: Arc<dyn crate::plugin::SessionLifecycleService>,
        session_graph: Arc<dyn crate::plugin::SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        direct_completions: crate::DirectCompletionClient<'run>,
        trigger_router: Option<crate::TriggerRouter>,
        process_definitions: Option<std::sync::Arc<dyn crate::ProcessDefinitionRegistry>>,
        process_engines: crate::ProcessEngineRegistry,
        observer: Arc<dyn crate::engine::ObservationSink>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        protocol_extension: Option<crate::ProtocolTurnExtensionHandle>,
        turn_context: crate::TurnContext,
        execution_env_spec: crate::ProcessExecutionEnvSpec,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer,
        attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    ) -> Result<RuntimeExecutionContext<'run>, crate::PluginError> {
        let tool_surface = self.active_tool_surface_entry(session_id)?;
        let dispatch = Arc::new(ToolDispatchContext {
            plugins: Arc::clone(self.plugins()),
            tools: tool_surface.tools(),
            tool_registry: Some(tool_surface.tool_registry()),
            tool_catalog: tool_surface.tool_catalog(),
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            trigger_router,
            process_definitions,
            process_engines,
            effect_controller,
            direct_completions: direct_completions.clone(),
            parent_invocation: None,
            execution_env_spec: execution_env_spec.clone(),
            session_id: SessionId::from(session_id.to_string()),
            agent_frame_id,
            observer,
            checkpoint_messages,
            trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
            attachment_store: Arc::clone(&self.services.attachment_store),
            attachment_source_policy,
            turn_context: turn_context.clone(),
            clock: Arc::clone(&self.services.clock),
        });
        Ok(RuntimeExecutionContext::new(
            SessionId::from(session_id.to_string()),
            dispatch,
            Arc::clone(&self.services.process_env_store),
            Arc::clone(&self.services.attachment_store),
            chronological_projection,
            protocol_extension,
            turn_context,
        ))
        .map(|context| {
            context
                .with_execution_env_spec(execution_env_spec)
                .with_live_tool_catalog(tool_surface.live_tool_catalog())
                .with_unrecorded_session_sources(crate::runtime::effect::UnrecordedSessionSources {
                    context_overlay_tools: !self.context_tools.is_empty(),
                    ..Default::default()
                })
        })
    }

    pub fn invalidate_runtime_caches(&self) {
        *self.tool_catalog_cache.lock_recover() = None;
        self.prompt_cache.clear();
    }

    pub async fn refresh_tool_catalog(&mut self) -> Result<(), SessionError> {
        self.tool_registry = self
            .services
            .plugins
            .tool_registry()
            .compose_session_catalog(self.context_tools.clone())
            .map(Arc::new)
            .map_err(|err| SessionError::Protocol(format!("tool reconfigure failed: {err}")))?;
        *self.tool_catalog_cache.lock_recover() = None;
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "`SessionToolAccess` is composed entirely of strings, enums and maps, whose serialization has no failing case"
)]
fn tool_catalog_authority_fingerprint(tool_access: &crate::SessionToolAccess) -> [u8; 32] {
    let encoded = serde_json::to_vec(tool_access)
        .expect("SessionToolAccess is composed entirely of serializable authority values");
    lash_sansio::core_support::blake3_domain_hash("lash-tool-catalog-authority/v2", encoded)
}

#[cfg(test)]
mod tool_catalog_cache_tests {
    use super::*;
    use crate::plugin::StaticPluginFactory;
    use lash_sansio::sync::MutexExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingDynamicProvider {
        names: Arc<std::sync::Mutex<Vec<String>>>,
        manifest_reads: Arc<AtomicUsize>,
    }

    struct ReassignableResidentProvider {
        label: &'static str,
        active: Arc<AtomicBool>,
        prepares: Arc<AtomicUsize>,
        executions: Arc<AtomicUsize>,
        defer_queries: Arc<AtomicUsize>,
        attempts: Arc<AtomicUsize>,
    }

    struct AdmissionProbeProvider {
        contract_available: bool,
        prepare_calls: Arc<AtomicUsize>,
    }

    impl AdmissionProbeProvider {
        fn definition() -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                "tool:resident",
                "resident",
                "resident admission probe",
                serde_json::json!({
                    "type": "object",
                    "properties": { "pinned": { "type": "string" } },
                    "required": ["pinned"],
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "string" }),
            )
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for AdmissionProbeProvider {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            vec![Self::definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (self.contract_available && name == "resident")
                .then(|| Arc::new(Self::definition().contract()))
        }

        async fn prepare_tool_call(
            &self,
            call: crate::ToolPrepareCall<'_>,
        ) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!("resident")).into()
        }
    }

    fn admission_probe_plugins(
        provider: Arc<dyn ToolProvider>,
        tool_access: crate::SessionToolAccess,
    ) -> Arc<crate::PluginSession> {
        let mut factories = crate::testing::test_standard_protocol_factories();
        factories.push(Arc::new(StaticPluginFactory::new(
            "admission_probe",
            crate::PluginSpec::new().with_tool_provider(provider),
        )));
        crate::PluginHost::new(factories)
            .build_session_with_parent(
                "admission-probe",
                None,
                crate::plugin::SessionCreationConfig {
                    authority: crate::plugin::SessionAuthorityContext {
                        tool_access,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .expect("plugin session")
    }

    async fn admission_probe_session(provider: Arc<dyn ToolProvider>) -> Session {
        let plugins = admission_probe_plugins(provider, crate::SessionToolAccess::default());
        Session::new(
            crate::testing::runtime_services_without_ports(plugins),
            &SessionId::from("admission-probe"),
        )
        .await
        .expect("runtime session")
    }

    impl CountingDynamicProvider {
        fn definition(name: &str) -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                format!("tool:{name}"),
                name,
                format!("dynamic {name}"),
                crate::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )
        }
    }

    impl ReassignableResidentProvider {
        fn definition(&self) -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                "tool:reassigned",
                "reassigned",
                format!("resident route {}", self.label),
                serde_json::json!({
                    "type": "object",
                    "properties": { self.label: { "type": "string" } },
                    "required": [self.label],
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "string" }),
            )
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for ReassignableResidentProvider {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            self.active
                .load(Ordering::SeqCst)
                .then(|| self.definition().manifest())
                .into_iter()
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            (name == "reassigned").then(|| Arc::new(self.definition().contract()))
        }

        async fn prepare_tool_call(
            &self,
            call: crate::ToolPrepareCall<'_>,
        ) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(crate::PreparedToolCall::identity(
                call.tool_id,
                call.pending,
            ))
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.attempts.fetch_add(1, Ordering::SeqCst);
            crate::ToolOutcome::ok(serde_json::json!(format!("attempt_{}", self.label))).into()
        }

        fn attempt_may_defer(&self, _tool_id: &crate::ToolId) -> bool {
            self.defer_queries.fetch_add(1, Ordering::SeqCst);
            self.label == "route_a"
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for CountingDynamicProvider {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            self.manifest_reads.fetch_add(1, Ordering::SeqCst);
            self.names
                .lock_recover()
                .iter()
                .map(|name| Self::definition(name).manifest())
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            self.names
                .lock_recover()
                .iter()
                .any(|candidate| candidate == name)
                .then(|| Arc::new(Self::definition(name).contract()))
        }

        async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::ok(serde_json::json!(call.name())).into()
        }
    }

    #[test]
    fn authority_fingerprint_covers_hidden_tools_and_explicit_definitions() {
        let base = crate::SessionToolAccess::default();
        let hidden = base
            .clone()
            .with_hidden_tools(["hidden"])
            .expect("valid hidden name");
        let explicit = crate::SessionToolAccess::restricted([crate::ToolDefinition::raw(
            "tool:explicit",
            "explicit",
            "authority-defined tool",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )])
        .expect("valid restricted definition");

        assert_ne!(
            tool_catalog_authority_fingerprint(&base),
            tool_catalog_authority_fingerprint(&hidden)
        );
        assert_ne!(
            tool_catalog_authority_fingerprint(&base),
            tool_catalog_authority_fingerprint(&explicit)
        );
    }

    #[test]
    fn ambient_and_restricted_empty_select_distinct_resident_catalogs() {
        let ambient = admission_probe_plugins(
            Arc::new(AdmissionProbeProvider {
                contract_available: true,
                prepare_calls: Arc::new(AtomicUsize::new(0)),
            }),
            crate::SessionToolAccess::ambient(),
        );
        assert!(
            ambient
                .resolved_tool_catalog(&SessionId::from("ambient-access"))
                .expect("ambient resident catalog")
                .has_callable_tool("resident")
        );

        let restricted = admission_probe_plugins(
            Arc::new(AdmissionProbeProvider {
                contract_available: true,
                prepare_calls: Arc::new(AtomicUsize::new(0)),
            }),
            crate::SessionToolAccess::restricted([]).expect("restricted empty is valid"),
        );
        assert!(
            restricted
                .resolved_tool_catalog(&SessionId::from("restricted-empty-access"))
                .expect("restricted-empty resident catalog")
                .tools
                .is_empty()
        );
        assert!(
            restricted
                .tool_registry()
                .export_state()
                .iter()
                .any(|(_, entry)| entry.manifest().name == "resident" && entry.is_member()),
            "restricted access curates the session catalog without changing registry membership"
        );
    }

    #[tokio::test]
    async fn model_request_pin_enumerates_once_and_freezes_catalog_and_dispatch() {
        let names = Arc::new(std::sync::Mutex::new(vec!["alpha".to_string()]));
        let manifest_reads = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn ToolProvider> = Arc::new(CountingDynamicProvider {
            names: Arc::clone(&names),
            manifest_reads: Arc::clone(&manifest_reads),
        });
        let mut factories = crate::testing::test_standard_protocol_factories();
        factories.push(Arc::new(StaticPluginFactory::new(
            "pinned_surface",
            crate::PluginSpec::new().with_tool_provider(provider),
        )));
        let plugins = crate::PluginHost::new(factories)
            .build_session("pinned-surface")
            .expect("plugin session");
        let session = Session::new(
            crate::testing::runtime_services_without_ports(plugins),
            &SessionId::from("pinned-surface"),
        )
        .await
        .expect("runtime session");
        let reads_before_pin = manifest_reads.load(Ordering::SeqCst);

        let first = session
            .pin_tool_surface(
                &SessionId::from("pinned-surface"),
                &crate::SessionToolAccess::default(),
                None,
            )
            .expect("first request surface");
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_before_pin + 1,
            "one model-request pin performs one live-source enumeration"
        );
        assert!(first.tool_catalog().has_callable_tool("alpha"));

        names.lock_recover().push("beta".to_string());
        assert!(!first.tool_catalog().has_callable_tool("beta"));
        assert!(first.tools().resolve_manifest("beta").is_none());
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_before_pin + 1,
            "catalog and dispatch reads stay on the same frozen request surface"
        );

        let second = session
            .pin_tool_surface(
                &SessionId::from("pinned-surface"),
                &crate::SessionToolAccess::default(),
                None,
            )
            .expect("next request surface");
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_before_pin + 2,
            "the next model request performs exactly one fresh enumeration"
        );
        assert!(second.tool_catalog().has_callable_tool("beta"));
        assert!(second.tools().resolve_manifest("beta").is_some());
        assert!(
            session
                .plugins()
                .tool_registry()
                .export_state()
                .iter()
                .any(|(_, entry)| entry.manifest().name == "beta"),
            "request admission updates the registry captured at the next durable turn boundary"
        );

        let hidden_access = crate::SessionToolAccess::ambient()
            .with_hidden_tools(["alpha"])
            .expect("valid hidden name");
        let hidden = session
            .pin_tool_surface(&SessionId::from("pinned-surface"), &hidden_access, None)
            .expect("authority-hidden request surface");
        assert!(!hidden.tool_catalog().has_callable_tool("alpha"));
        // Building a surface installs nothing; consumers read the surface a
        // sync recorded, once the drive installs it.
        session
            .install_recorded_tool_surface(
                &SessionId::from("pinned-surface"),
                &hidden_access,
                None,
                &hidden.definitions(),
            )
            .expect("install the recorded hidden surface");
        assert!(
            !session
                .resolved_tool_catalog(&SessionId::from("pinned-surface"))
                .expect("active hidden request surface")
                .has_callable_tool("alpha"),
            "consumers retain the exact recorded surface even when session-construction authority differs"
        );
        assert!(
            hidden
                .tool_registry()
                .export_state()
                .iter()
                .find(|(_, entry)| entry.manifest().name == "alpha")
                .is_some_and(|(_, entry)| entry.is_member()),
            "authority suppression must leave host curation true"
        );

        let unhidden = session
            .pin_tool_surface(
                &SessionId::from("pinned-surface"),
                &crate::SessionToolAccess::default(),
                None,
            )
            .expect("next request with broader authority");
        assert!(
            unhidden.tool_catalog().has_callable_tool("alpha"),
            "removing an authority suppression takes effect on the next request"
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_before_pin + 5,
            "each of four request pins and the install enumerated the live source exactly once"
        );
    }

    #[tokio::test]
    async fn model_request_pin_captures_provider_route_across_same_id_reassignment() {
        let a_active = Arc::new(AtomicBool::new(true));
        let b_active = Arc::new(AtomicBool::new(false));
        let a_prepares = Arc::new(AtomicUsize::new(0));
        let b_prepares = Arc::new(AtomicUsize::new(0));
        let a_executions = Arc::new(AtomicUsize::new(0));
        let b_executions = Arc::new(AtomicUsize::new(0));
        let a_defer_queries = Arc::new(AtomicUsize::new(0));
        let b_defer_queries = Arc::new(AtomicUsize::new(0));
        let a_attempts = Arc::new(AtomicUsize::new(0));
        let b_attempts = Arc::new(AtomicUsize::new(0));
        let providers = [
            Arc::new(ReassignableResidentProvider {
                label: "route_a",
                active: Arc::clone(&a_active),
                prepares: Arc::clone(&a_prepares),
                executions: Arc::clone(&a_executions),
                defer_queries: Arc::clone(&a_defer_queries),
                attempts: Arc::clone(&a_attempts),
            }) as Arc<dyn ToolProvider>,
            Arc::new(ReassignableResidentProvider {
                label: "route_b",
                active: Arc::clone(&b_active),
                prepares: Arc::clone(&b_prepares),
                executions: Arc::clone(&b_executions),
                defer_queries: Arc::clone(&b_defer_queries),
                attempts: Arc::clone(&b_attempts),
            }) as Arc<dyn ToolProvider>,
        ];
        let mut factories = crate::testing::test_standard_protocol_factories();
        let spec = providers
            .into_iter()
            .fold(crate::PluginSpec::new(), |spec, provider| {
                spec.with_tool_provider(provider)
            });
        factories.push(Arc::new(StaticPluginFactory::new("reassignable", spec)));
        let plugins = crate::PluginHost::new(factories)
            .build_session("route-reassignment")
            .expect("plugin session");
        let session_id = SessionId::from("route-reassignment");
        let session = Session::new(
            crate::testing::runtime_services_without_ports(plugins),
            &session_id,
        )
        .await
        .expect("runtime session");

        let old = session
            .pin_tool_surface(&session_id, &crate::SessionToolAccess::default(), None)
            .expect("request pinned while provider A owns the id");
        let old_entries = old
            .tool_catalog()
            .tools
            .iter()
            .filter(|entry| entry.manifest.id.as_str() == "tool:reassigned")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(old_entries.len(), 1, "the id is advertised exactly once");
        assert!(
            old_entries[0]
                .contract
                .input_schema
                .canonical()
                .pointer("/properties/route_a")
                .is_some(),
            "the old request captures provider A's schema"
        );

        a_active.store(false, Ordering::SeqCst);
        b_active.store(true, Ordering::SeqCst);
        let fresh = session
            .pin_tool_surface(&session_id, &crate::SessionToolAccess::default(), None)
            .expect("next request pinned after provider B owns the id");
        let fresh_entries = fresh
            .tool_catalog()
            .tools
            .iter()
            .filter(|entry| entry.manifest.id.as_str() == "tool:reassigned")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(fresh_entries.len(), 1, "the reassigned id stays unique");
        assert!(
            fresh_entries[0]
                .contract
                .input_schema
                .canonical()
                .pointer("/properties/route_b")
                .is_some(),
            "the fresh request captures provider B's schema"
        );

        let prepare_context = crate::ToolPrepareContext::with_execution_binding(
            session_id,
            Arc::new(crate::testing::MockSessionManager::default()),
            crate::TurnContext::default(),
            Some("reassigned-call".to_string()),
            serde_json::json!({}),
        );
        old.tools()
            .prepare_tool_call(crate::ToolPrepareCall {
                tool_id: crate::ToolId::from("tool:reassigned"),
                pending: crate::sansio::PendingToolCall {
                    call_id: "reassigned-call".to_string(),
                    tool_name: "reassigned".to_string(),
                    args: serde_json::json!({ "route_a": "old" }),
                    replay: None,
                },
                context: &prepare_context,
            })
            .await
            .expect("the old request prepares through provider A");
        let tool_id = crate::ToolId::from("tool:reassigned");
        let attempt_context = crate::testing::mock_attempt_context();
        let old_manifest = old
            .tools()
            .resolve_manifest_by_id(&tool_id)
            .expect("the old surface resolves the reassigned manifest");
        let old_attempt = old
            .tools()
            .execute(crate::ToolCall::new(
                &old_manifest,
                &serde_json::json!({ "route_a": "old" }),
                &attempt_context,
            ))
            .await;
        let crate::ToolAttemptOutcome::Done { result, intents } = old_attempt else {
            panic!("provider A returns a completed attempt")
        };
        assert!(intents.is_empty());
        assert_eq!(
            result.into_output().value_for_projection(),
            serde_json::json!("attempt_route_a")
        );
        assert_eq!(a_prepares.load(Ordering::SeqCst), 1);
        assert_eq!(b_prepares.load(Ordering::SeqCst), 0);
        assert_eq!(a_executions.load(Ordering::SeqCst), 1);
        assert_eq!(b_executions.load(Ordering::SeqCst), 0);
        assert!(old.tools().attempt_may_defer(&tool_id));
        assert_eq!(a_defer_queries.load(Ordering::SeqCst), 1);
        assert_eq!(b_defer_queries.load(Ordering::SeqCst), 0);
        assert_eq!(a_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(b_attempts.load(Ordering::SeqCst), 0);

        fresh
            .tools()
            .prepare_tool_call(crate::ToolPrepareCall {
                tool_id: crate::ToolId::from("tool:reassigned"),
                pending: crate::sansio::PendingToolCall {
                    call_id: "fresh-reassigned-call".to_string(),
                    tool_name: "reassigned".to_string(),
                    args: serde_json::json!({ "route_b": "fresh" }),
                    replay: None,
                },
                context: &prepare_context,
            })
            .await
            .expect("the fresh request prepares through provider B");
        let fresh_manifest = fresh
            .tools()
            .resolve_manifest_by_id(&tool_id)
            .expect("the fresh surface resolves the reassigned manifest");
        let fresh_attempt = fresh
            .tools()
            .execute(crate::ToolCall::new(
                &fresh_manifest,
                &serde_json::json!({ "route_b": "fresh" }),
                &attempt_context,
            ))
            .await;
        let crate::ToolAttemptOutcome::Done { result, intents } = fresh_attempt else {
            panic!("provider B returns a completed attempt")
        };
        assert!(intents.is_empty());
        assert_eq!(
            result.into_output().value_for_projection(),
            serde_json::json!("attempt_route_b")
        );
        assert_eq!(a_prepares.load(Ordering::SeqCst), 1);
        assert_eq!(b_prepares.load(Ordering::SeqCst), 1);
        assert_eq!(a_executions.load(Ordering::SeqCst), 1);
        assert_eq!(b_executions.load(Ordering::SeqCst), 1);
        assert!(!fresh.tools().attempt_may_defer(&tool_id));
        assert_eq!(a_defer_queries.load(Ordering::SeqCst), 1);
        assert_eq!(b_defer_queries.load(Ordering::SeqCst), 1);
        assert_eq!(a_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(b_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn effective_member_without_contract_is_refused_before_prepare() {
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let session = admission_probe_session(Arc::new(AdmissionProbeProvider {
            contract_available: false,
            prepare_calls: Arc::clone(&prepare_calls),
        }))
        .await;

        let error = match session.pin_tool_surface(
            &SessionId::from("admission-probe"),
            &crate::SessionToolAccess::default(),
            None,
        ) {
            Ok(_) => panic!("missing resident contract must be refused before advertisement"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            crate::PluginError::ResidentToolContractUnavailable { ref tool_id, ref name }
                if tool_id.as_str() == "tool:resident" && name == "resident"
        ));
        assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn restricted_definition_uses_id_route_and_missing_route_is_refused_before_prepare() {
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let session = admission_probe_session(Arc::new(AdmissionProbeProvider {
            contract_available: true,
            prepare_calls: Arc::clone(&prepare_calls),
        }))
        .await;
        let renamed = crate::ToolDefinition::raw(
            "tool:resident",
            "resident_alias",
            "authority-owned alias",
            serde_json::json!({
                "type": "object",
                "properties": { "authority": { "type": "string" } },
                "required": ["authority"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        );
        let renamed_surface = session
            .pin_tool_surface(
                &SessionId::from("admission-probe"),
                &crate::SessionToolAccess::restricted([renamed])
                    .expect("valid restricted definition"),
                None,
            )
            .expect("the same ToolId retains its pinned route under an authority-owned alias");
        let entry = &renamed_surface.tool_catalog().tools[0];
        assert_eq!(entry.manifest.name, "resident_alias");
        assert!(
            entry
                .contract
                .input_schema
                .canonical()
                .pointer("/properties/authority")
                .is_some()
        );

        let missing = crate::ToolDefinition::raw(
            "tool:missing-route",
            "missing_route",
            "no resident execution route",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        );
        let error = match session.pin_tool_surface(
            &SessionId::from("admission-probe"),
            &crate::SessionToolAccess::restricted([missing]).expect("valid restricted definition"),
            None,
        ) {
            Ok(_) => panic!("missing resident route must be refused before advertisement"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            crate::PluginError::ResidentToolRouteUnavailable { ref tool_id, ref name, .. }
                if tool_id.as_str() == "tool:missing-route" && name == "missing_route"
        ));
        assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn plugin_session_refuses_missing_resident_route_before_advertisement() {
        let prepare_calls = Arc::new(AtomicUsize::new(0));
        let missing = crate::ToolDefinition::raw(
            "tool:missing-route",
            "missing_route",
            "no resident execution route",
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        );
        let plugins = admission_probe_plugins(
            Arc::new(AdmissionProbeProvider {
                contract_available: true,
                prepare_calls: Arc::clone(&prepare_calls),
            }),
            crate::SessionToolAccess::restricted([missing]).expect("valid restricted definition"),
        );

        let error = plugins
            .resolved_tool_catalog(&SessionId::from("admission-probe"))
            .expect_err("direct plugin-session consumers must refuse a missing resident route");
        assert!(matches!(
            error,
            crate::PluginError::ResidentToolRouteUnavailable { ref tool_id, ref name, .. }
                if tool_id.as_str() == "tool:missing-route" && name == "missing_route"
        ));
        assert_eq!(prepare_calls.load(Ordering::SeqCst), 0);
    }
}
