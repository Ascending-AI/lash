use lash_core::plugin::PluginSessionRequest;
use lash_sansio::SessionId;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::sync::{Arc, OnceLock};

use lash_core::facade_support::PluginHost;
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngineContributionContext, SessionAuthorityContext, SessionPlugin,
};
use lash_lashlang_runtime::{
    LashlangArtifacts, LashlangHostEnvironment, LashlangProcessEngine, LashlangSurface,
    SharedDeferredToolResolver, SharedDeferredTriggerResolver,
};

use super::registration::register_rlm_protocol_plugin;
use super::{
    RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig, RlmRecordedBehaviour, RlmRecordedConfig,
};
use crate::dialect::{Dialect, RlmDialectServices, SessionDialect};

/// Apply the RLM protocol config transformation: enable, when process lifecycle
/// is available, the process/sleep/signal abilities.
///
/// This is protocol logic; it lives here rather than in the facade because both
/// the plugin surface and the contributed Lashlang process engine derive from
/// it.
///
/// Language features are NOT transformed here. The default (label annotations
/// on) is decided once, where every config is born — `RlmProtocolPluginConfig`'s
/// builder and serde default — so a host that turns a feature off keeps it off
/// end to end, as ADR 0085 promises (FIG-2768).
pub fn rlm_protocol_config(
    config: RlmProtocolPluginConfig,
    process_lifecycle: bool,
) -> RlmProtocolPluginConfig {
    let mut config = config;
    if process_lifecycle {
        config.lashlang_abilities = config.lashlang_abilities.with_sleep();
    }
    config
}

/// Build the Lashlang surface for the contributed process engine from an
/// (already [`rlm_protocol_config`]-transformed) config.
pub fn rlm_lashlang_surface(
    config: &RlmProtocolPluginConfig,
    process_lifecycle: bool,
) -> LashlangSurface {
    let surface = LashlangSurface::new(
        config.lashlang_abilities.into_engine(),
        config.lashlang_language_features.into_engine(),
        lashlang::LashlangHostCatalog::new(),
    );
    if process_lifecycle {
        surface.for_process_registry(true)
    } else {
        surface
    }
}

pub struct RlmProtocolPluginFactory {
    config: RlmProtocolPluginConfig,
    /// The host's one dialect selection: every session this factory builds
    /// parses, spells tools and prompts in it, and records its language id.
    dialect: Arc<dyn Dialect>,
    workers: lash_vm_client::service::Service,
    deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    deferred_trigger_resolver: Option<SharedDeferredTriggerResolver>,
    artifact_store: LashlangArtifacts,
    /// The binding identity of the backend `artifact_store` belongs to: a
    /// runtime over any other backend refuses this factory.
    artifact_backend: Arc<str>,
    /// Whether this deployment has process lifecycle available. Recorded once —
    /// by core installing process-engine contributions (before any session is
    /// built), by [`Self::with_process_lifecycle`] for hosts that assemble a
    /// plugin host directly, or by the compile path's explicit argument — and
    /// read back when building the per-session plugin surface so the prompt
    /// advertises the same abilities the engine offers. Building a session
    /// before the value is recorded fails loudly instead of silently degrading
    /// abilities; conflicting recordings fail loudly too. The config owner
    /// shares it: a session created here records the abilities it implies.
    process_lifecycle: Arc<OnceLock<bool>>,
}

impl RlmProtocolPluginFactory {
    /// An RLM protocol in `dialect` over `backend`, the substrate its
    /// Lashlang module artifacts live in (ADR 0102, D2).
    ///
    /// `dialect` is the host's selection of the language its models write
    /// (ADR 0096): cells, `processes.create` sources, compiled modules and
    /// every prompt fragment go through it, and a session records its
    /// language id so it never resumes under another dialect.
    ///
    /// The artifact store comes from the backend, never beside it: a session
    /// resumed after a restart reopens the same backend and finds the modules
    /// it published, and the backend's artifact cleanup sweeps the store the
    /// sessions wrote. Pass the backend the runtime is built over; a runtime
    /// over another backend refuses this factory
    /// ([`PluginFactory::bound_backend`]).
    pub fn new(
        config: RlmProtocolPluginConfig,
        dialect: Arc<dyn Dialect>,
        backend: &lash_core::Backend,
    ) -> Self {
        let workers = dialect.worker_service();
        Self {
            config,
            dialect,
            workers,
            deferred_tool_resolver: None,
            deferred_trigger_resolver: None,
            artifact_store: LashlangArtifacts::of_backend(backend),
            artifact_backend: Arc::from(backend.binding_identity().as_str()),
            process_lifecycle: Arc::new(OnceLock::new()),
        }
    }

    /// Select the host's worker entry, pool bounds and deadlines. This service
    /// is shared by compilation, cells, process creation and durable bodies.
    pub fn with_worker_service(mut self, workers: lash_vm_client::service::Service) -> Self {
        self.workers = workers;
        self
    }
    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// Wire a host-provided [`DeferredToolResolver`](lash_lashlang_runtime::DeferredToolResolver)
    /// that resolves each link's batch of Lashlang call-paths absent from the
    /// host environment into per-path Tool Grants or unavailable outcomes.
    /// Most hosts ship none.
    pub fn with_deferred_tool_resolver(mut self, resolver: SharedDeferredToolResolver) -> Self {
        self.deferred_tool_resolver = Some(resolver);
        self
    }

    /// Wire a dedicated trigger-definition resolver. Discovery is link-only:
    /// registration remains the first operation allowed to activate a route.
    pub fn with_deferred_trigger_resolver(
        mut self,
        resolver: SharedDeferredTriggerResolver,
    ) -> Self {
        self.deferred_trigger_resolver = Some(resolver);
        self
    }

    pub fn artifact_store(&self) -> LashlangArtifacts {
        self.artifact_store.clone()
    }

    /// Declare process-lifecycle availability explicitly, for hosts that build
    /// sessions from a hand-assembled [`PluginHost`] (or durable worker)
    /// instead of a core that installs process-engine contributions — the
    /// install path records this automatically.
    ///
    /// Panics if a conflicting value was already recorded: that is a wiring
    /// bug, not a runtime condition.
    #[expect(
        clippy::expect_used,
        reason = "documented panicking builder half of record_process_lifecycle; the Err half is the wiring bug this panic names"
    )]
    pub fn with_process_lifecycle(self, process_lifecycle_available: bool) -> Self {
        self.record_process_lifecycle(process_lifecycle_available)
            .expect("conflicting process-lifecycle availability recorded on RLM protocol factory");
        self
    }

    /// Record process-lifecycle availability: first recording wins, repeated
    /// agreeing recordings are no-ops, and a conflicting recording is an error
    /// (a silent flip would desynchronize the per-session plugin surface from
    /// engines already handed out).
    fn record_process_lifecycle(&self, process_lifecycle_available: bool) -> Result<(), String> {
        if self
            .process_lifecycle
            .set(process_lifecycle_available)
            .is_err()
            && self.process_lifecycle.get() != Some(&process_lifecycle_available)
        {
            return Err(format!(
                "RLM protocol factory already recorded process_lifecycle_available={}; \
                 refusing conflicting recording of {}",
                !process_lifecycle_available, process_lifecycle_available
            ));
        }
        Ok(())
    }

    /// The behaviour a session whose plugin configuration is
    /// `plugin_config` runs under: the one its RLM namespace recorded
    /// (FIG-4398). A session being created has recorded none yet and runs
    /// under what its creation records, this deployment's; a rebuilt session
    /// that recorded none is refused, never given this deployment's.
    fn session_behaviour(
        &self,
        plugin_config: &lash_core::PluginConfig,
        materialization: lash_core::plugin::PluginSessionMaterialization,
    ) -> Result<RlmRecordedBehaviour, PluginError> {
        match recorded_config(plugin_config)? {
            Some(recorded) => Ok(recorded.behaviour),
            None if matches!(
                materialization,
                lash_core::plugin::PluginSessionMaterialization::Rematerialization
            ) =>
            {
                Err(PluginError::MissingRecordedSessionConfig {
                    plugin_id: RLM_PROTOCOL_PLUGIN_ID.to_string(),
                    field: "behaviour".to_string(),
                })
            }
            None => Ok(self.config.recorded_behaviour(self.process_lifecycle()?)),
        }
    }

    fn process_lifecycle(&self) -> Result<bool, PluginError> {
        self.process_lifecycle.get().copied().ok_or_else(|| {
            PluginError::Registration(
                "RLM protocol factory built a session before learning whether process \
                 lifecycle is available; abilities would silently degrade. Install the \
                 factory through a core (which records this while installing \
                 process-engine contributions) or declare it explicitly with \
                 `with_process_lifecycle`."
                    .to_string(),
            )
        })
    }

    /// Operation over the factory and a plugin host: the caller supplies a plugin
    /// host containing this protocol factory plus any tool plugins to resolve,
    /// and whether process lifecycle is available.
    pub fn lashlang_compile_surface(
        &self,
        plugin_host: &PluginHost,
        process_lifecycle_available: bool,
        request: LashlangCompileSurfaceRequest,
    ) -> Result<LashlangCompileSurface, PluginError> {
        // The compile caller is the authority on process-lifecycle availability
        // here; record it before building the throwaway catalog-resolution
        // session below, which invokes this factory's `build` (the plugin host
        // contains this factory) and reads the recorded value.
        self.record_process_lifecycle(process_lifecycle_available)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let behaviour = self.session_behaviour(
            &request.execution_env_spec.plugin_config.config,
            lash_core::plugin::PluginSessionMaterialization::Creation,
        )?;
        let plugins = plugin_host.build_session(PluginSessionRequest::creation(
            &request.session_id,
            SessionAuthorityContext {
                plugin_config: request.execution_env_spec.plugin_config,
                ..Default::default()
            },
        ))?;
        let tool_catalog = plugins.resolved_tool_catalog()?;
        let config = self.config.clone().under_recorded_behaviour(&behaviour);
        let surface = rlm_lashlang_surface(&config, process_lifecycle_available)
            .with_plugin_extensions(plugin_host.extensions())
            .and_then(|surface| surface.with_plugin_extensions(plugins.session_extensions()))
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let host_environment = surface
            .host_environment(&tool_catalog)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        Ok(LashlangCompileSurface {
            host_environment,
            tool_catalog,
            surface,
        })
    }

    /// Compile a Lashlang module against the compile-time surface in a worker.
    #[allow(
        clippy::result_large_err,
        reason = "boxing LashlangModuleCompileError would change this public compile API"
    )]
    pub async fn compile_lashlang_module(
        &self,
        plugin_host: &PluginHost,
        process_lifecycle_available: bool,
        request: LashlangModuleCompileRequest,
    ) -> Result<ModuleCompileOutput, LashlangModuleCompileError> {
        let surface = self
            .lashlang_compile_surface(
                plugin_host,
                process_lifecycle_available,
                LashlangCompileSurfaceRequest {
                    session_id: request.session_id,
                    execution_env_spec: request.execution_env_spec,
                },
            )
            .map_err(LashlangModuleCompileError::Surface)?;
        match self
            .workers
            .request_accounted(lash_vm_client::service::Request::CompileModule {
                source: request.source,
                environment: surface.host_environment,
                cell: false,
            })
            .await
            .map_err(LashlangModuleCompileError::Worker)?
        {
            lash_vm_client::service::Response::Module(module) => Ok(*module),
            lash_vm_client::service::Response::CompileRefused { error, .. } => Err(error.into()),
            _ => Err(LashlangModuleCompileError::Worker(
                lash_vm_client::PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                ),
            )),
        }
    }
}

impl PluginFactory for RlmProtocolPluginFactory {
    fn transcript_projector(
        &self,
    ) -> Option<Arc<dyn lash_core::plugin::TranscriptRowProjectorPlugin>> {
        Some(Arc::new(
            crate::projection::transcript::RlmTranscriptProjector,
        ))
    }

    fn id(&self) -> &'static str {
        RLM_PROTOCOL_PLUGIN_ID
    }

    /// The session's RLM namespace and its one command (FIG-4379): the
    /// channel and dialect this host selected are recorded at creation.
    fn register_config(
        &self,
        registrar: &mut lash_core::plugin::ConfigRegistrar,
    ) -> Result<(), lash_core::plugin::ConfigRegistrationError> {
        super::config_owner::register(
            registrar,
            super::config_owner::RlmConfigOwner {
                channel: self.config.channel,
                dialect: self.dialect.language_id(),
                config: self.config.clone(),
                process_lifecycle: Arc::clone(&self.process_lifecycle),
            },
        )
    }

    /// The backend this factory's Lashlang artifacts live in: a runtime over
    /// another backend would resume sessions whose modules it cannot find and
    /// sweep an artifact store nobody wrote.
    fn bound_backend(&self) -> Option<&str> {
        Some(&self.artifact_backend)
    }

    fn process_engine_contributions(
        &self,
        ctx: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, PluginError> {
        let process_lifecycle = ctx.process_lifecycle_available();
        // Record for the per-session plugin surface; install runs before any
        // session is built on this (shared) factory.
        self.record_process_lifecycle(process_lifecycle)
            .map_err(PluginError::Registration)?;
        let config = rlm_protocol_config(self.config.clone(), process_lifecycle);
        let surface = rlm_lashlang_surface(&config, process_lifecycle)
            .with_plugin_extensions(ctx.extensions())
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let recorder = Arc::new(RlmProcessSettingsRecorder {
            deployment_config: self.config.clone(),
            plugin_host: ctx.plugin_host().clone(),
            process_lifecycle,
        });
        let engine = LashlangProcessEngine::new(self.artifact_store.clone(), surface)
            .with_worker_service(self.workers.clone())
            .with_execution_bounds(config.execution_bounds().into_engine())
            .with_run_settings_recorder(recorder);
        Ok(vec![
            lash_lashlang_runtime::lashlang_process_engine_registration(engine),
        ])
    }

    /// The session's plugin runs under the behaviour the session recorded:
    /// it is pinned at creation, so the value at build is every run's.
    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        let behaviour = self.session_behaviour(&ctx.plugin_config.config, ctx.materialization)?;
        let config = self.config.clone().under_recorded_behaviour(&behaviour);
        let recorded = recorded_config(&ctx.plugin_config.config)?;
        super::channel::validate_channel(
            recorded.as_ref(),
            self.config.channel,
            ctx.materialization,
        )?;
        super::channel::validate_dialect(
            recorded.as_ref(),
            self.dialect.language_id(),
            ctx.materialization,
        )?;
        let lashlang_surface = LashlangSurface::new(
            config.lashlang_abilities.into_engine(),
            config.lashlang_language_features.into_engine(),
            lashlang::LashlangHostCatalog::new(),
        )
        .with_plugin_extensions(&ctx.extensions)
        .map_err(|err| PluginError::Registration(err.to_string()))?;
        let services = RlmDialectServices {
            workers: self.workers.clone(),
            code_renderer: config.code_renderer.clone(),
            artifact_store: self.artifact_store.clone(),
            deferred_tool_resolver: self.deferred_tool_resolver.clone(),
            deferred_trigger_resolver: self.deferred_trigger_resolver.clone(),
            execution_bounds: config.execution_bounds(),
            channel: config.channel,
        };
        let dialect = Arc::new(SessionDialect::new(
            Arc::clone(&self.dialect),
            lashlang_surface,
            services,
        ));
        if config.channel == super::RlmChannel::NativeTool {
            return Ok(Arc::new(crate::native::RlmNativeToolPlugin {
                config,
                dialect,
            }));
        }
        Ok(Arc::new(RlmProtocolPlugin { config, dialect }))
    }
}

impl lash_core::plugin::PluginDefinition for RlmProtocolPluginFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(RLM_PROTOCOL_PLUGIN_ID)
    }
}

/// The RLM namespace `plugin_config` recorded, as its owner's recorded type.
fn recorded_config(
    plugin_config: &lash_core::PluginConfig,
) -> Result<Option<RlmRecordedConfig>, PluginError> {
    plugin_config
        .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
        .map_err(|error| {
            PluginError::Session(format!("invalid recorded RLM session config: {error}"))
        })
}

/// Maps the captured RLM namespace and creation-time resource grants into the
/// engine's one process record. RLM prompt and render choices stay on sessions.
struct RlmProcessSettingsRecorder {
    deployment_config: RlmProtocolPluginConfig,
    plugin_host: PluginHost,
    process_lifecycle: bool,
}

impl lash_lashlang_runtime::LashlangRunSettingsRecorder for RlmProcessSettingsRecorder {
    fn record(
        &self,
        plugin_config: &lash_core::AdmittedPluginConfig,
    ) -> Result<lash_lashlang_runtime::LashlangRecordedSettings, PluginError> {
        let captured = plugin_config
            .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
            .map_err(|error| PluginError::StoredDataCorrupt {
                record_kind: "captured RLM process config".to_owned(),
                message: error.to_string(),
            })?;
        let behaviour = captured
            .map(|recorded| recorded.behaviour)
            .unwrap_or_else(|| {
                self.deployment_config
                    .recorded_behaviour(self.process_lifecycle)
            });
        let config = self
            .deployment_config
            .clone()
            .under_recorded_behaviour(&behaviour);
        let mut surface = rlm_lashlang_surface(&config, self.process_lifecycle)
            .with_plugin_extensions(self.plugin_host.extensions())
            .map_err(|error| PluginError::Registration(error.to_string()))?;
        let context = PluginSessionContext {
            tracing: self.plugin_host.trace_runtime().clone(),
            trace: None,
            owner: lash_core::RuntimeOwner::Process(lash_core::mint_process_id()),
            tool_access: Default::default(),
            subagent: None,
            plugin_config: plugin_config.clone(),
            materialization: lash_core::plugin::PluginSessionMaterialization::Creation,
            extensions: self.plugin_host.extensions().clone(),
            parent_session_id: None,
        };
        for factory in self.plugin_host.factories() {
            let plugin = factory.build(&context)?;
            let extensions =
                lash_core::PluginExtensions::from_contributions(plugin.extension_contributions());
            surface = surface
                .with_plugin_extensions(&extensions)
                .map_err(|error| PluginError::Registration(error.to_string()))?;
        }
        Ok(lash_lashlang_runtime::LashlangRecordedSettings::new(
            surface,
            config.execution_bounds().into_engine(),
        ))
    }
}

/// Request for [`RlmProtocolPluginFactory::lashlang_compile_surface`].
pub struct LashlangCompileSurfaceRequest {
    pub session_id: SessionId,
    pub execution_env_spec: lash_core::ProcessExecutionEnvSpec,
}

impl LashlangCompileSurfaceRequest {
    pub fn new(
        session_id: impl Into<SessionId>,
        execution_env_spec: lash_core::ProcessExecutionEnvSpec,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            execution_env_spec,
        }
    }
}

/// Request for [`RlmProtocolPluginFactory::compile_lashlang_module`].
pub struct LashlangModuleCompileRequest {
    pub session_id: SessionId,
    pub source: String,
    pub execution_env_spec: lash_core::ProcessExecutionEnvSpec,
}

impl LashlangModuleCompileRequest {
    pub fn new(
        session_id: impl Into<SessionId>,
        source: impl Into<String>,
        execution_env_spec: lash_core::ProcessExecutionEnvSpec,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            source: source.into(),
            execution_env_spec,
        }
    }
}

pub struct LashlangCompileSurface {
    pub host_environment: LashlangHostEnvironment,
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
    pub surface: LashlangSurface,
}

/// A compile diagnostic, worker fault, or failure to assemble the host surface.
#[derive(Clone, Debug, thiserror::Error)]
pub enum LashlangModuleCompileError {
    #[error(transparent)]
    Compile(#[from] lashlang::ModuleCompileError),
    #[error(transparent)]
    Worker(lash_vm_client::PoolError),
    #[error(transparent)]
    Surface(PluginError),
}
pub type ModuleCompileOutput = lash_vm_client::service::CompiledModule;

struct RlmProtocolPlugin {
    config: RlmProtocolPluginConfig,
    dialect: Arc<SessionDialect>,
}

impl SessionPlugin for RlmProtocolPlugin {
    fn id(&self) -> &'static str {
        RLM_PROTOCOL_PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_rlm_protocol_plugin(reg, self.config.clone(), Arc::clone(&self.dialect))
    }
}

#[cfg(test)]
mod label_annotation_tests {
    use super::{rlm_lashlang_surface, rlm_protocol_config};
    use crate::plugin::{InstructionBound, MemoryBound, RlmProtocolPluginConfig};

    fn base_config() -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig::builder()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .memory_limit(MemoryBound::mebibytes(64))
            .build()
    }

    fn rendered_surface(config: RlmProtocolPluginConfig) -> lashlang::LashlangHostEnvironment {
        let config = rlm_protocol_config(config, false);
        rlm_lashlang_surface(&config, false)
            .host_environment(&lash_core::ToolCatalog::from_tool_definitions(Vec::new()))
            .expect("host environment")
    }

    /// `@label(title: "Answer") finish "ok"` — a label annotation has no
    /// TypeScript form, so the witness states the AST the host feature gate
    /// rejects.
    fn labelled_program() -> lashlang::Program {
        use lashlang::testing::ast_builders as b;

        b::program(vec![b::labelled(
            b::label("Answer", None),
            b::finish(b::string("ok")),
        )])
    }

    #[test]
    fn host_disabled_label_annotations_are_absent_from_the_language() {
        // Toolbench's shape (examples/toolbench/src/runtime.rs): every optional
        // language feature spelled off.
        let mut config = base_config();
        config.lashlang_language_features.label_annotations = false;
        let host_environment = rendered_surface(config);

        assert!(!host_environment.language_features.label_annotations);
        let err = lashlang::LinkedModule::link(labelled_program(), &host_environment)
            .expect_err("label syntax must be rejected when the host disabled the feature");
        assert!(
            matches!(
                err,
                lashlang::LinkError::FeatureDisabled {
                    feature: "label annotations",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn default_config_keeps_label_annotations_on() {
        let host_environment = rendered_surface(base_config());

        assert!(host_environment.language_features.label_annotations);
        lashlang::LinkedModule::link(labelled_program(), &host_environment)
            .expect("default surface links label annotations");
    }

    #[tokio::test]
    async fn typescript_parse_failure_keeps_its_own_source_span() {
        // FIG-3268: the factory used to hand `parse_failure` only
        // `span.start`, so the diagnostic's `span` stayed `None` and the
        // end of the offending token was lost.
        let factory = std::sync::Arc::new(
            crate::RlmProtocolPluginFactory::new(
                crate::RlmProtocolPluginConfig::builder()
                    .channel(crate::RlmChannel::Cell)
                    .instruction_limit(crate::InstructionBound::instructions(1_000_000))
                    .memory_limit(crate::MemoryBound::mebibytes(64))
                    .build(),
                std::sync::Arc::new(crate::TypescriptDialect),
                &crate::testing::sqlite_memory_store_backend().await,
            )
            .with_process_lifecycle(false),
        );
        let factory_plugin: std::sync::Arc<dyn lash_core::facade_support::PluginFactory> =
            factory.clone();
        let plugin_host = lash_core::facade_support::PluginHost::new(vec![factory_plugin]);
        let source = "process 42oops() { finish \"x\" }";
        let err = factory
            .compile_lashlang_module(
                &plugin_host,
                false,
                crate::LashlangModuleCompileRequest::new(
                    "factory-test",
                    source,
                    lash_core::ProcessExecutionEnvSpec::new(
                        Default::default(),
                        lash_core::SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                            lash_core::MaxToolCalls::new(1024),
                        ),
                    ),
                ),
            )
            .await
            .expect_err("invalid typescript must fail to parse");

        let super::LashlangModuleCompileError::Compile(lashlang::ModuleCompileError::Parse(
            diagnostic,
        )) = err
        else {
            panic!("expected parse error");
        };
        let span = diagnostic.span.expect("parse failure carries a span");
        assert_eq!(diagnostic.offset(), Some(span.start));
        assert_eq!(&source[span.start..span.end], "42");
    }

    #[test]
    fn serde_config_without_language_features_keeps_the_default() {
        let config: RlmProtocolPluginConfig = serde_json::from_value(serde_json::json!({
            "channel": "cell",
            "instruction_limit": { "bounded": 1_000_000 },
            "memory_limit": { "bounded": 67_108_864 }
        }))
        .expect("rlm config");
        assert!(config.lashlang_language_features.label_annotations);

        let config: RlmProtocolPluginConfig = serde_json::from_value(serde_json::json!({
            "channel": "cell",
            "instruction_limit": { "bounded": 1_000_000 },
            "memory_limit": { "bounded": 67_108_864 },
            "lashlang_language_features": { "label_annotations": false }
        }))
        .expect("rlm config");
        assert!(!config.lashlang_language_features.label_annotations);
    }
}
