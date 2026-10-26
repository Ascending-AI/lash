#[doc(hidden)]
pub mod environment;

use crate::session::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt;
use std::sync::{Arc, OnceLock};

use crate::PluginMessage;
use crate::RuntimeServices;
use crate::tool_dispatch::ToolDispatchContext;

mod execution_context;
pub(crate) use execution_context::{ToolCallStart, ToolObservationAttribution};
mod opener_run;
mod process_handles;
mod settlement_incorporation;
mod tool_attempt;
pub(crate) mod tool_execution;

pub use execution_context::RuntimeExecutionTracing;
pub(crate) use execution_context::runtime_ops;
pub use execution_context::{
    CellEndHandoff, EndedCell, HeldResponse, LinkedExecution, ReadHistory, RuntimeExecutionContext,
};
pub(crate) use execution_context::{
    attach_process_lineage, execution_claim_of, process_lineage_of,
};
pub use opener_run::OpenerState;
pub use settlement_incorporation::{Incorporated, IncorporationLedger, SettlementSource};
/// Runtime tool invocation requests and their collected replies.
pub use tool_execution::{CompletedProtocolToolCall, ToolInvocation, ToolInvocationReply};

#[derive(Debug, Default)]
struct ToolCatalogDerived {
    catalog: OnceLock<Arc<Vec<serde_json::Value>>>,
}

struct ToolCatalogArtifact {
    version: lash_core_store::environment::EnvironmentId,
    /// The immutable projection of the round's recorded environment version.
    surface: Arc<crate::RecordedSurface>,
    /// The surface's calls, each routed to its entry's provider.
    tools: crate::SurfaceTools,
    tool_catalog: Arc<crate::ToolCatalog>,
    driver: Arc<dyn crate::plugin::ProtocolDriverPlugin>,
    preamble_input: crate::ProtocolBuildInput,
    preamble: OnceLock<Arc<crate::TurnDriverPreamble>>,
    derived: ToolCatalogDerived,
}

#[derive(Clone)]
pub struct ToolCatalogHandle(Arc<ToolCatalogArtifact>);

/// Resident projections are indexed by immutable content, never by mutable
/// configuration or a keyless installed surface. The weak directory does not
/// accumulate discarded projections; the most recently read version and the
/// installed round stay resident. Metadata owns no durable artifact referrers.
#[derive(Default)]
pub(crate) struct EnvironmentProjections {
    versions: std::collections::BTreeMap<
        lash_core_store::environment::EnvironmentId,
        std::sync::Weak<ToolCatalogArtifact>,
    >,
    pub(crate) installed: Option<ToolCatalogHandle>,
    resident: Option<ToolCatalogHandle>,
    pub(crate) empty_catalog: Arc<Vec<serde_json::Value>>,
}

impl ToolCatalogHandle {
    pub(crate) fn version(&self) -> &lash_core_store::environment::EnvironmentId {
        &self.0.version
    }

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
        Arc::clone(
            self.0.preamble.get_or_init(|| {
                Arc::new(self.0.driver.build_preamble(self.0.preamble_input.clone()))
            }),
        )
    }

    /// The catalog's tools as definitions.
    pub fn definitions(&self) -> Vec<lash_vocabulary::tool_contract::ToolDefinition> {
        self.0.surface.definitions().cloned().collect()
    }

    pub(crate) fn catalog(&self) -> Arc<Vec<serde_json::Value>> {
        Arc::clone(self.0.derived.catalog.get_or_init(|| {
            Arc::new(crate::tool_registry::project_tool_catalog(
                self.0.tool_catalog.entries().iter().cloned(),
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
    Plugin(#[from] lash_execution_contracts::plugin_error::PluginError),
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
            crate::plugin::CreationConfigError::Registration(error) => Self::Plugin(
                lash_execution_contracts::plugin_error::PluginError::ConfigRegistration(error),
            ),
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
        error: lash_execution_contracts::plugin_error::PluginError,
    ) -> Self {
        use crate::sansio::ExecutionEnvironmentSyncFailureKind as Kind;
        let package_refusal = matches!(&error, lash_execution_contracts::PluginError::Package(_));
        let message = format!("protocol error: {error}");
        let failure = error.into_turn_failure(match kind {
            Kind::ToolSurface => crate::RuntimeErrorCode::ToolCatalogResolutionFailed,
            Kind::ProtocolFacts => crate::RuntimeErrorCode::ProtocolBeforeLlmCall,
        });
        if package_refusal || failure.turn_failure_cause().aborts_invocation() {
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
    bootstrap_preamble: Arc<OnceLock<Arc<crate::TurnDriverPreamble>>>,
}

impl Session {
    pub async fn new(
        services: RuntimeServices,
        session_id: &SessionId,
    ) -> Result<Self, SessionError> {
        let session = Self {
            session_id: session_id.clone(),
            services,
            bootstrap_preamble: Arc::new(OnceLock::new()),
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
            bootstrap_preamble: Arc::clone(&self.bootstrap_preamble),
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
        plugins: &crate::PluginSession,
        fleet_format: crate::FleetFormat,
        version: lash_core_store::environment::EnvironmentId,
        surface: Arc<crate::RecordedSurface>,
        tool_catalog: Arc<crate::ToolCatalog>,
    ) -> ToolCatalogHandle {
        let tool_catalog = Arc::new(plugins.protocol_driver().model_catalog(&tool_catalog));
        let preamble_input = crate::ProtocolBuildInput {
            tool_catalog: Arc::clone(&tool_catalog),
            plugin_extensions: plugins.extensions().clone(),
            writer_formats: Arc::new(crate::FleetWriterFormats(fleet_format)),
        };
        ToolCatalogHandle(Arc::new(ToolCatalogArtifact {
            version,
            tools: crate::SurfaceTools::of_surface(&surface, plugins.tool_registry()),
            surface,
            tool_catalog,
            driver: plugins.protocol_driver(),
            preamble_input,
            preamble: OnceLock::new(),
            derived: ToolCatalogDerived::default(),
        }))
    }

    /// Driver configuration before the first environment is adopted. Subsequent
    /// turns use the installed version's preamble. The empty bootstrap is built
    /// only once and is never offered as a live tool catalog.
    pub fn protocol_driver_preamble(&self) -> Arc<crate::TurnDriverPreamble> {
        if let Ok(handle) = self.installed_environment() {
            return handle.preamble();
        }
        Arc::clone(self.bootstrap_preamble.get_or_init(|| {
            Arc::new(
                self.plugins()
                    .protocol_driver()
                    .build_preamble(crate::ProtocolBuildInput {
                        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(
                            Vec::new(),
                        )),
                        plugin_extensions: self.plugins().extensions().clone(),
                        writer_formats: Arc::new(crate::FleetWriterFormats(self.fleet_format())),
                    }),
            )
        }))
    }

    /// How `tool_id`'s entry on the installed surface drifted on this node
    /// under the run's configuration, if it did; `None` for an undrifted
    /// tool or one the surface does not offer.
    pub fn tool_surface_drift(
        &self,
        tool_id: &lash_vocabulary::tool_contract::ToolId,
    ) -> Result<Option<crate::ToolSurfaceDrift>, lash_execution_contracts::plugin_error::PluginError>
    {
        let config =
            crate::plugin::OwnerConfigView::session(self.plugins().admitted_plugin_config());
        Ok(self
            .installed_environment()?
            .tools()
            .drift(tool_id, None, &config))
    }

    /// Project a recorded version once while it has a resident reader. Derive
    /// outside the lock, then reclassify by version before publishing the result.
    pub fn project_environment(
        &self,
        body: &lash_core_store::environment::EnvironmentBody,
    ) -> Result<ToolCatalogHandle, lash_execution_contracts::PluginError> {
        let handle = Self::project_environment_for(self.plugins(), self.fleet_format(), body)?;
        // Executing adoption and replay prepare the protocol before any package
        // hold, as before. Catalog-only inspection never initializes execution.
        drop(handle.preamble());
        Ok(handle)
    }

    /// Bind immutable recorded metadata to prepared capabilities. Inspection
    /// does not require a materialized session or initialize its execution.
    pub fn project_environment_for(
        plugins: &crate::PluginSession,
        fleet_format: crate::FleetFormat,
        body: &lash_core_store::environment::EnvironmentBody,
    ) -> Result<ToolCatalogHandle, lash_execution_contracts::PluginError> {
        let projections = &plugins.environment_projections;
        {
            let mut projections = projections.lock_recover();
            if let Some(handle) = &projections.resident
                && handle.version() == body.id()
            {
                return Ok(handle.clone());
            }
            if let Some(artifact) = projections
                .versions
                .get(body.id())
                .and_then(std::sync::Weak::upgrade)
            {
                let handle = ToolCatalogHandle(artifact);
                let displaced = projections.resident.replace(handle.clone());
                drop(projections);
                drop(displaced);
                return Ok(handle);
            }
        }
        let built = plugins.project_environment(body)?;
        let candidate = Self::handle(
            plugins,
            fleet_format,
            body.id().clone(),
            built.surface,
            built.catalog,
        );
        let mut projections = projections.lock_recover();
        projections
            .versions
            .retain(|_, artifact| artifact.strong_count() > 0);
        if let Some(artifact) = projections
            .versions
            .get(body.id())
            .and_then(std::sync::Weak::upgrade)
        {
            let handle = ToolCatalogHandle(artifact);
            let displaced = projections.resident.replace(handle.clone());
            drop(projections);
            drop(displaced);
            return Ok(handle);
        }
        projections
            .versions
            .insert(candidate.version().clone(), Arc::downgrade(&candidate.0));
        let displaced = projections.resident.replace(candidate.clone());
        drop(projections);
        drop(displaced);
        Ok(candidate)
    }

    /// Adoption and replay install only an explicitly versioned projection.
    pub fn install_environment(&self, handle: ToolCatalogHandle) {
        let displaced = {
            let mut projections = self.plugins().environment_projections.lock_recover();
            projections.installed.replace(handle)
        };
        drop(displaced);
    }

    pub fn installed_environment(
        &self,
    ) -> Result<ToolCatalogHandle, lash_execution_contracts::PluginError> {
        self.plugins().installed_environment()
    }

    pub fn resolved_tool_catalog(
        &self,
    ) -> Result<Arc<crate::ToolCatalog>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(self.installed_environment()?.tool_catalog())
    }

    pub fn shared_tool_catalog(
        &self,
    ) -> Result<Arc<Vec<serde_json::Value>>, lash_execution_contracts::plugin_error::PluginError>
    {
        self.plugins().shared_tool_catalog()
    }

    pub fn tool_catalog(
        &self,
    ) -> Result<Vec<serde_json::Value>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(self.shared_tool_catalog()?.as_ref().clone())
    }

    /// The package projection of the installed round's recorded environment.
    pub fn installed_packages(
        &self,
    ) -> Result<
        Arc<lash_core_store::tool_surface::RecordedPackages>,
        lash_execution_contracts::PluginError,
    > {
        Ok(Arc::new(
            self.installed_environment()?.surface().packages().clone(),
        ))
    }

    pub fn installed_tool_catalog(
        &self,
    ) -> Result<Arc<crate::ToolCatalog>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(self.installed_environment()?.tool_catalog())
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
    ) -> Result<RuntimeExecutionContext<'run>, lash_execution_contracts::plugin_error::PluginError>
    {
        let tool_surface = self.installed_environment()?;
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
}
