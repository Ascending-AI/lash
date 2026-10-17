use crate::SessionId;
use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use lash_protocol_types::sync::MutexExt;
use std::sync::{Arc, OnceLock};

use crate::PluginMessage;
use crate::RuntimeServices;
use crate::tool_dispatch::ToolDispatchContext;

mod execution_context;
pub(crate) use execution_context::{RecordedGrants, ToolCallStart, ToolObservationAttribution};
mod opener_run;
mod process_handles;
mod settlement_incorporation;
mod tool_attempt;
pub(crate) mod tool_execution;

pub use execution_context::RuntimeExecutionTracing;
pub(crate) use execution_context::runtime_ops;
pub use execution_context::{CellEndHandoff, EndedCell, HeldResponse, RuntimeExecutionContext};
pub(crate) use execution_context::{
    attach_process_lineage, execution_claim_of, process_lineage_of,
};
pub use opener_run::OpenerState;
pub use settlement_incorporation::{Incorporated, IncorporationLedger, SettlementSource};
/// Runtime tool invocation requests and their collected replies.
pub use tool_execution::{CompletedProtocolToolCall, ToolInvocation, ToolInvocationReply};

/// What a live surface was built under: the plugins' state generations and
/// the run's resolved configuration, which every provider advertises under
/// (S2). Two runs whose configurations differ never share a surface.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolCatalogCacheKey {
    plugin_generations: std::collections::BTreeMap<String, u64>,
    config: crate::plugin::AdmittedPluginConfig,
}

#[derive(Debug, Default)]
struct ToolCatalogDerived {
    catalog: OnceLock<Arc<Vec<serde_json::Value>>>,
}

struct ToolCatalogArtifact {
    /// The surface the run's model calls are admitted under: a recorded
    /// one, or one built from live advertisements for a sync to record.
    surface: Arc<crate::RecordedSurface>,
    /// The surface's calls, each routed to its entry's provider.
    tools: crate::SurfaceTools,
    tool_catalog: Arc<crate::ToolCatalog>,
    preamble: Arc<crate::TurnDriverPreamble>,
    derived: ToolCatalogDerived,
}

/// The part of a tool definition that decides how a call links and
/// dispatches: its identity, binding, argument projection, retry policy,
/// declaration (its turn controls, with the schema a Finish value must
/// match) and schemas. The description and examples reach only the model's
/// prompt, which a redrive serves from the journaled environment sync.
#[derive(serde::Serialize, PartialEq)]
pub struct ToolDispatchSurface<'a> {
    id: &'a crate::ToolId,
    name: &'a str,
    bindings: &'a std::collections::BTreeMap<String, serde_json::Value>,
    argument_projection: &'a crate::ToolArgumentProjectionPolicy,
    execution_policy: &'a crate::ExecutionPolicy,
    declaration: &'a crate::ToolDeclaration,
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
        argument_projection: &manifest.argument_projection,
        execution_policy: &manifest.execution_policy,
        declaration: manifest.declaration(),
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

impl ToolCatalogHandle {
    /// The surface's calls, each routed to its entry's provider.
    pub fn tools(&self) -> crate::SurfaceTools {
        self.0.tools.clone()
    }

    /// The surface this handle offers.
    pub fn surface(&self) -> Arc<crate::RecordedSurface> {
        Arc::clone(&self.0.surface)
    }

    pub fn tool_catalog(&self) -> Arc<crate::ToolCatalog> {
        Arc::clone(&self.0.tool_catalog)
    }

    pub fn preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        Arc::clone(&self.0.preamble)
    }

    /// The catalog's tools as definitions.
    pub fn definitions(&self) -> Vec<crate::ToolDefinition> {
        self.0.surface.definitions().cloned().collect()
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
    /// The session has selected no model, so it has nothing to run a turn
    /// with.
    #[error("session `{session_id}` has selected no model")]
    LlmProfileUnconfigured { session_id: SessionId },
    /// The session's recorded model has no binding on this deployment.
    #[error("session `{session_id}` cannot run its model: {source}")]
    LlmProfileUnavailable {
        session_id: SessionId,
        #[source]
        source: crate::LlmProfileUnavailable,
    },
    /// A model change named a key the host's registry does not register. It
    /// is refused before anything is written.
    #[error("model change refused for session `{session_id}`: {source}")]
    LlmProfileUnknown {
        session_id: SessionId,
        #[source]
        source: crate::LlmProfileUnavailable,
    },
    #[error("{context}: {source}")]
    Store {
        context: String,
        #[source]
        source: crate::StoreError,
    },
    #[error("session config command was cancelled before settlement: {0}")]
    SessionCommandCancelled(crate::SessionCommandReceipt),
    /// Session config a creation stated that was refused, before anything
    /// was written (FIG-4099, FIG-4379): which owner, and why, typed
    /// ([`ConfigRefusal`](crate::ConfigRefusal)). Its
    /// [`reason`](crate::ConfigRefusal::reason) carries the owner's own
    /// refusal type as data, or the framework's reason, such as an owner no
    /// installed plugin registers. A later change is a config transaction,
    /// whose refusal is its settled outcome.
    #[error("session config refused: {0}")]
    SessionConfigRefused(crate::ConfigRefusal),
    #[error(transparent)]
    Plugin(#[from] crate::PluginError),
    /// A protocol refused its saved execution state. The source retains the
    /// protocol's public restore error so hosts can inspect its typed cause.
    #[error("execution state restore failed: {source}")]
    ExecutionStateRestore {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<crate::FormatRefusal> for SessionError {
    fn from(refusal: crate::FormatRefusal) -> Self {
        Self::Plugin(refusal.into())
    }
}

impl From<crate::plugin::CreationConfigError> for SessionError {
    fn from(error: crate::plugin::CreationConfigError) -> Self {
        match error {
            crate::plugin::CreationConfigError::Recipe(error) => Self::Plugin(*error),
            crate::plugin::CreationConfigError::Format(refusal) => refusal.into(),
            crate::plugin::CreationConfigError::Refused(refusal) => {
                Self::SessionConfigRefused(refusal)
            }
            crate::plugin::CreationConfigError::RecordedCorrupt(corrupt) => Self::Store {
                context: "resolving a session's creation config".to_string(),
                source: corrupt.into_store_error(),
            },
            crate::plugin::CreationConfigError::Registration(error) => {
                Self::Plugin(crate::PluginError::ConfigRegistration(error))
            }
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
            Self::Io(_)
            | Self::Json(_)
            | Self::LlmProfileUnconfigured { .. }
            | Self::LlmProfileUnavailable { .. }
            | Self::LlmProfileUnknown { .. }
            | Self::Store { .. }
            | Self::SessionCommandCancelled(_)
            | Self::SessionConfigRefused(_)
            | Self::Plugin(_)
            | Self::ExecutionStateRestore { .. }
            | Self::Protocol(_) => crate::ExecCodeFailureReason::Session,
        };
        crate::ExecCodeFailure::new(reason, self.to_string())
    }
}

/// Why an execution-environment sync could not rebuild the environment.
pub enum ExecutionEnvironmentSyncError {
    /// Deterministic over the turn's inputs: journaled as the sync's outcome,
    /// which fails the turn and replays identically.
    Recorded(crate::sansio::ExecutionEnvironmentSyncFailure),
    /// A fact about this attempt — a store, lease or session fault — that must
    /// not be journaled: the turn aborts and a redrive rebuilds the
    /// environment (the FIG-3575 live-fault class).
    Live(crate::RuntimeError),
}

impl ExecutionEnvironmentSyncError {
    /// A plugin's failure keeps the code its own classification gives it;
    /// `kind` names the part of the sync that raised it.
    pub fn of_plugin_error(
        kind: crate::sansio::ExecutionEnvironmentSyncFailureKind,
        error: crate::PluginError,
    ) -> Self {
        use crate::sansio::ExecutionEnvironmentSyncFailureKind as Kind;
        let message = format!("protocol error: {error}");
        let failure = error.into_turn_failure(match kind {
            Kind::ToolSurface => crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
            Kind::ProtocolFacts => crate::RuntimeErrorCode::ProtocolBeforeLlmCall,
        });
        if failure.turn_failure_cause().aborts_invocation() {
            Self::Live(failure)
        } else {
            Self::Recorded(crate::sansio::ExecutionEnvironmentSyncFailure {
                code: crate::FailureCode::from(&failure.code),
                kind,
                message,
            })
        }
    }

    pub fn of_session_error(
        kind: crate::sansio::ExecutionEnvironmentSyncFailureKind,
        error: SessionError,
    ) -> Self {
        match error {
            SessionError::Plugin(error) => Self::of_plugin_error(kind, error),
            error @ SessionError::Store { .. } => Self::Live(crate::RuntimeError::new(
                crate::RuntimeErrorCode::StoreCommitFailed,
                error.to_string(),
            )),
            // A session-layer refusal of the render itself: deterministic
            // over the turn's inputs, with no narrower code of its own.
            error @ (SessionError::Io(_)
            | SessionError::Json(_)
            | SessionError::CodeExecutionUnavailable
            | SessionError::CodeExecutionRuntimeStopped
            | SessionError::LlmProfileUnconfigured { .. }
            | SessionError::LlmProfileUnavailable { .. }
            | SessionError::LlmProfileUnknown { .. }
            | SessionError::SessionCommandCancelled(_)
            | SessionError::SessionConfigRefused(_)
            | SessionError::ExecutionStateRestore { .. }
            | SessionError::Protocol(_)) => {
                Self::Recorded(crate::sansio::ExecutionEnvironmentSyncFailure {
                    code: crate::TurnFailureCode::ReconfigureFailed.into(),
                    kind,
                    message: error.to_string(),
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub code: String,
}

pub struct Session {
    session_id: SessionId,
    services: RuntimeServices,
    /// The installed surface: a recorded one (no key), or a live one built
    /// under its key.
    tool_catalog_cache: Arc<std::sync::Mutex<Option<InstalledToolCatalog>>>,
}

/// An installed surface and the key it was built under, `None` for a
/// recorded one.
type InstalledToolCatalog = (Option<ToolCatalogCacheKey>, ToolCatalogHandle);

impl Session {
    pub async fn new(
        services: RuntimeServices,
        session_id: &SessionId,
    ) -> Result<Self, SessionError> {
        let session = Self {
            session_id: session_id.clone(),
            services,
            tool_catalog_cache: Arc::new(std::sync::Mutex::new(None)),
        };

        let protocol_session = Arc::clone(session.plugins().protocol_session());
        protocol_session
            .initialize_session(crate::plugin::ProtocolSessionContext::new(
                session_id,
                session.fleet_format(),
            ))
            .await?;

        Ok(session)
    }

    pub fn clone_for_effect(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            services: self.services.clone(),
            tool_catalog_cache: Arc::clone(&self.tool_catalog_cache),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn plugins(&self) -> &Arc<crate::PluginSession> {
        &self.services.plugins
    }

    pub fn history_store(&self) -> Option<crate::store::SessionStore> {
        self.services.store.clone()
    }

    /// The `F` the bound store recorded; a session with no durable store falls
    /// back to this build's own fleet format (FIG-3796).
    pub fn fleet_format(&self) -> crate::FleetFormat {
        self.services
            .store
            .as_ref()
            .map(crate::store::SessionStore::fleet_format)
            .unwrap_or_else(crate::FleetFormat::current)
    }

    fn handle(
        &self,
        surface: Arc<crate::RecordedSurface>,
        tool_catalog: Arc<crate::ToolCatalog>,
    ) -> ToolCatalogHandle {
        let preamble = self
            .plugins()
            .protocol_driver()
            .build_preamble(crate::ProtocolBuildInput {
                tool_catalog: Arc::clone(&tool_catalog),
                plugin_extensions: self.plugins().extensions().clone(),
                writer_formats: Arc::new(crate::FleetWriterFormats(self.fleet_format())),
            });
        ToolCatalogHandle(Arc::new(ToolCatalogArtifact {
            tools: crate::SurfaceTools::of_surface(&surface, self.plugins().tool_registry()),
            surface,
            tool_catalog,
            preamble: Arc::new(preamble),
            derived: ToolCatalogDerived::default(),
        }))
    }

    /// The surface the session's providers offer under `config` (S4),
    /// built and not installed: a sync records it, and the shift installs
    /// what the sync recorded ([`Self::install_recorded_tool_surface`]).
    fn build_tool_surface(
        &self,
        config: &crate::plugin::AdmittedPluginConfig,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let built = self.plugins().build_tool_surface(config)?;
        Ok(self.handle(built.surface, built.catalog))
    }

    /// Installs the surface a run recorded as the one its model calls are
    /// admitted under (S5).
    ///
    /// The recorded surface is the catalog: its tools, their providers and
    /// definitions are what the pass that recorded it saw, whichever worker
    /// replays it. Nothing is re-advertised: a call whose provider no longer
    /// serves its recorded definition is judged when it is called
    /// ([`crate::SurfaceTools::drift`]), and parks.
    pub fn install_recorded_tool_surface(
        &self,
        recorded: Arc<crate::RecordedSurface>,
    ) -> Result<ToolCatalogHandle, crate::PluginError> {
        let tool_catalog = Arc::new(self.plugins().surface_catalog(&recorded)?);
        let handle = self.handle(recorded, tool_catalog);
        *self.tool_catalog_cache.lock_recover() = Some((None, handle.clone()));
        Ok(handle)
    }

    /// The protocol driver's preamble for a turn machine whose environment its
    /// protocol-start sync supplies. It is built over an empty catalog: the
    /// driver's configuration is host configuration and does not depend on the
    /// tools, and the prompt and tool specs it would render are replaced by
    /// the recorded sync. The machine always syncs, since that sync is the
    /// only way the environment reaches it.
    pub fn protocol_driver_preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        let preamble = self
            .plugins()
            .protocol_driver()
            .build_preamble(crate::ProtocolBuildInput {
                tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
                plugin_extensions: self.plugins().extensions().clone(),
                writer_formats: Arc::new(crate::FleetWriterFormats(self.fleet_format())),
            });
        Arc::new(preamble)
    }

    /// How `tool_id`'s entry on the installed surface drifted on this node
    /// under the run's configuration, if it did; `None` for an undrifted
    /// tool or one the surface does not offer.
    pub fn tool_surface_drift(
        &self,
        tool_id: &crate::ToolId,
    ) -> Result<Option<crate::ToolSurfaceDrift>, crate::PluginError> {
        let config =
            crate::plugin::OwnerConfigView::session(self.plugins().admitted_plugin_config());
        Ok(self
            .active_tool_surface_entry()?
            .tools()
            .drift(tool_id, &config))
    }

    fn tool_catalog_cache_entry(&self) -> Result<ToolCatalogHandle, crate::PluginError> {
        let key = ToolCatalogCacheKey {
            plugin_generations: self.plugins().state_generations(),
            config: self.plugins().admitted_plugin_config(),
        };
        let mut cache = self.tool_catalog_cache.lock_recover();
        if let Some((Some(entry_key), entry)) = cache.as_ref()
            && *entry_key == key
        {
            return Ok(entry.clone());
        }
        let entry = self.build_tool_surface(&key.config)?;
        *cache = Some((Some(key), entry.clone()));
        Ok(entry)
    }

    fn active_tool_surface_entry(&self) -> Result<ToolCatalogHandle, crate::PluginError> {
        if let Some((_, entry)) = self.tool_catalog_cache.lock_recover().as_ref() {
            return Ok(entry.clone());
        }
        self.tool_catalog_cache_entry()
    }

    /// The surface the session's providers offer now, under the run's
    /// configuration: built once per configuration and plugin state, and
    /// installed for nothing. A sync records it.
    // `ToolCatalogHandle` is only `pub` under the `testing` feature, so this
    // accessor's visibility tracks it exactly (`private_interfaces`).
    #[cfg(feature = "testing")]
    pub fn pin_tool_surface(&self) -> Result<ToolCatalogHandle, crate::PluginError> {
        self.build_tool_surface(&self.plugins().admitted_plugin_config())
    }

    #[cfg(not(feature = "testing"))]
    pub fn pin_tool_surface(&self) -> Result<ToolCatalogHandle, crate::PluginError> {
        self.build_tool_surface(&self.plugins().admitted_plugin_config())
    }

    /// Build the surface a sync records (S4) and judge it against the
    /// protocol before anything records it.
    pub async fn admit_tool_surface(&self) -> Result<ToolCatalogHandle, crate::PluginError> {
        let surface = self.build_tool_surface(&self.plugins().admitted_plugin_config())?;
        self.plugins()
            .protocol_session()
            .validate_tool_catalog(&surface.tool_catalog(), None)
            .await?;
        Ok(surface)
    }

    pub fn resolved_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        Ok(self.active_tool_surface_entry()?.tool_catalog())
    }

    pub fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, crate::PluginError> {
        Ok(self.active_tool_surface_entry()?.catalog())
    }

    pub fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, crate::PluginError> {
        Ok(self.shared_tool_catalog()?.as_ref().clone())
    }

    /// The pinned catalog of the installed surface: the one the turn's latest
    /// execution-environment sync recorded, which its model calls offer.
    pub fn installed_tool_catalog(&self) -> Result<Arc<crate::ToolCatalog>, crate::PluginError> {
        Ok(self.active_tool_surface_entry()?.tool_catalog())
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
        effect_controller: crate::ActorContext,
        direct_completions: crate::DirectCompletionClient<'run>,
        process_engines: crate::ProcessEngineRegistry,
        observer: Arc<dyn crate::engine::ObservationSink>,
        chronological_projection: Arc<crate::ChronologicalProjection>,
        turn_context: crate::TurnContext,
        facts: crate::plugin::ExecutionFacts,
    ) -> Result<RuntimeExecutionContext<'run>, crate::PluginError> {
        let tool_surface = self.active_tool_surface_entry()?;
        let dispatch = Arc::new(ToolDispatchContext {
            fleet_format: self
                .history_store()
                .map_or_else(crate::FleetFormat::current, |store| {
                    store.store().fleet_format()
                }),
            composition: (self.plugins()).composition(),
            config: crate::plugin::OwnerConfigView::session(
                (self.plugins()).admitted_plugin_config(),
            ),
            execution_budgets: (self.plugins()).execution_budgets(),
            state: (self.plugins()).clone(),
            tools: Arc::new(tool_surface.tools()),
            routes: tool_surface.tools(),
            tool_catalog: tool_surface.tool_catalog(),
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            process_engines,
            effect_controller,
            direct_completions: direct_completions.clone(),
            parent_invocation: None,
            observation_call_key: None,
            facts,
            owner: crate::ExecutionOwner::SessionFrame {
                session_id: session_id.clone(),
                agent_frame_id,
            },
            observer,
            attachment_store: Arc::clone(&self.services.attachment_store),
            turn_context: turn_context.clone(),
            clock: Arc::clone(&self.services.clock),
            process_lineage: process_lineage_of(&turn_context),
            process_originator: None,
        });
        Ok(RuntimeExecutionContext::new(
            dispatch,
            Arc::clone(&self.services.process_env_store),
            Arc::clone(&self.services.attachment_store),
            chronological_projection,
            turn_context,
        )
        .with_tool_live(self.services.tool_live.clone()))
        .map(|context| {
            context.with_fleet_format(
                self.services
                    .store
                    .as_ref()
                    .map(crate::store::SessionStore::fleet_format)
                    .unwrap_or_else(crate::FleetFormat::current),
            )
        })
    }

    pub fn invalidate_runtime_caches(&self) {
        *self.tool_catalog_cache.lock_recover() = None;
    }

    /// Forget the built surface: the next read builds it again from what
    /// the session's providers advertise under the run's configuration.
    pub fn refresh_tool_catalog(&mut self) {
        *self.tool_catalog_cache.lock_recover() = None;
    }
}
