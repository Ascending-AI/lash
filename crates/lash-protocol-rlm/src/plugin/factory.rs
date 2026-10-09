use lash_core::plugin::PluginSessionRequest;
use lash_sansio::SessionId;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::sync::Arc;

use lash_core::facade_support::PluginHost;
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    ProcessEngineContributionContext, SessionAuthorityContext, SessionPlugin,
};
use lash_vm_runtime::{
    LashVmArtifacts, LashVmHostEnvironment, LashVmProcessEngine, LashVmSurface,
    SharedDeferredToolResolver,
};

use super::registration::register_rlm_protocol_plugin;
use super::{
    RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig, RlmRecordedBehaviour, RlmRecordedConfig,
};
use crate::dialect::{Dialect, RlmDialectServices, SessionDialect};

/// Build the process engine's Lash VM surface under the host's language features.
pub fn rlm_lash_vm_surface(config: &RlmProtocolPluginConfig) -> LashVmSurface {
    LashVmSurface::new(
        config.lash_vm_language_features.into_engine(),
        lash_vm::LashVmHostCatalog::new(),
    )
}

pub struct RlmProtocolPluginFactory {
    config: RlmProtocolPluginConfig,
    /// The host's one dialect selection: every session this factory builds
    /// parses, spells tools and prompts in it, and records its language id.
    dialect: Arc<dyn Dialect>,
    workers: lash_vm_client::service::Service,
    segment_policy: lash_vm_runtime::VmSegmentPolicy,
    deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    artifact_store: LashVmArtifacts,
    /// The binding identity of the backend `artifact_store` belongs to: a
    /// runtime over any other backend refuses this factory.
    artifact_backend: Arc<str>,
}

impl RlmProtocolPluginFactory {
    /// An RLM protocol in `dialect` over `backend`, the substrate its
    /// Lash VM module artifacts live in (ADR 0102, D2).
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
            segment_policy: lash_vm_runtime::VmSegmentPolicy::standard(),
            deferred_tool_resolver: None,
            artifact_store: LashVmArtifacts::of_backend(backend),
            artifact_backend: Arc::from(backend.binding_identity().as_str()),
        }
    }

    /// Select the host's worker entry, pool bounds and deadlines. This service
    /// is shared by compilation, cells, process creation and durable bodies.
    pub fn with_worker_service(mut self, workers: lash_vm_client::service::Service) -> Self {
        self.workers = workers;
        self
    }
    /// Set the VM segment policy used by this factory's process engine.
    pub fn with_segment_policy(mut self, policy: lash_vm_runtime::VmSegmentPolicy) -> Self {
        self.segment_policy = policy;
        self
    }

    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// Wire a host-provided [`DeferredToolResolver`](lash_vm_runtime::DeferredToolResolver)
    /// that resolves each link's batch of Lash VM call-paths absent from the
    /// host environment into per-path Tool Grants or unavailable outcomes.
    /// Most hosts ship none.
    pub fn with_deferred_tool_resolver(mut self, resolver: SharedDeferredToolResolver) -> Self {
        self.deferred_tool_resolver = Some(resolver);
        self
    }

    pub fn artifact_store(&self) -> LashVmArtifacts {
        self.artifact_store.clone()
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
            None => Ok(self.config.recorded_behaviour()),
        }
    }

    /// Operation over the factory and a plugin host: the caller supplies a plugin
    /// host containing this protocol factory plus any tool plugins to resolve.
    pub fn lash_vm_compile_surface(
        &self,
        plugin_host: &PluginHost,
        request: LashVmCompileSurfaceRequest,
    ) -> Result<LashVmCompileSurface, PluginError> {
        let behaviour = self.session_behaviour(
            &request.execution_env_spec.plugin_config.config,
            lash_core::plugin::PluginSessionMaterialization::Creation,
        )?;
        let plugins = plugin_host.build_session(PluginSessionRequest::creation(
            &request.session_id,
            // Compile against the authority this process environment records.
            SessionAuthorityContext {
                tool_access: request.execution_env_spec.tool_access,
                plugin_config: request.execution_env_spec.plugin_config,
            },
        ))?;
        let tool_catalog = plugins.resolved_tool_catalog()?;
        let config = self.config.clone().under_recorded_behaviour(&behaviour);
        let surface = rlm_lash_vm_surface(&config)
            .with_plugin_extensions(plugin_host.extensions())
            .and_then(|surface| surface.with_plugin_extensions(plugins.session_extensions()))
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let host_environment = surface
            .host_environment(&tool_catalog)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        Ok(LashVmCompileSurface {
            host_environment,
            tool_catalog,
            surface,
        })
    }

    /// Compile a Lash VM module against the compile-time surface in a worker.
    #[allow(
        clippy::result_large_err,
        reason = "boxing LashVmModuleCompileError would change this public compile API"
    )]
    pub async fn compile_lash_vm_module(
        &self,
        plugin_host: &PluginHost,
        request: LashVmModuleCompileRequest,
    ) -> Result<ModuleCompileOutput, LashVmModuleCompileError> {
        let surface = self
            .lash_vm_compile_surface(
                plugin_host,
                LashVmCompileSurfaceRequest {
                    session_id: request.session_id,
                    execution_env_spec: request.execution_env_spec,
                },
            )
            .map_err(LashVmModuleCompileError::Surface)?;
        match self
            .workers
            .request_accounted(lash_vm_client::service::Request::CompileModule {
                source: request.source,
                environment: surface.host_environment,
                cell: false,
            })
            .await
            .map_err(LashVmModuleCompileError::Worker)?
        {
            lash_vm_client::service::Response::Module(module) => Ok(*module),
            lash_vm_client::service::Response::CompileRefused { error, .. } => Err(error.into()),
            _ => Err(LashVmModuleCompileError::Worker(
                lash_vm_client::PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                ),
            )),
        }
    }
}

impl PluginFactory for RlmProtocolPluginFactory {
    fn transcript_decoder(&self) -> Option<Arc<dyn lash_core::plugin::TranscriptDecoderPlugin>> {
        Some(Arc::new(
            crate::projection::transcript::RlmTranscriptDecoder,
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
            },
        )
    }

    /// The backend this factory's Lash VM artifacts live in: a runtime over
    /// another backend would resume sessions whose modules it cannot find and
    /// sweep an artifact store nobody wrote.
    fn bound_backend(&self) -> Option<&str> {
        Some(&self.artifact_backend)
    }

    fn process_engine_contributions(
        &self,
        ctx: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, PluginError> {
        let config = self.config.clone();
        let surface = rlm_lash_vm_surface(&config)
            .with_plugin_extensions(ctx.extensions())
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        let recorder = Arc::new(RlmProcessSettingsRecorder {
            deployment_config: self.config.clone(),
            plugin_host: ctx.plugin_host().clone(),
        });
        let engine = LashVmProcessEngine::new(self.artifact_store.clone(), surface)
            .with_segment_policy(self.segment_policy)
            .with_trace_runtime(ctx.trace_runtime().clone())
            .with_worker_service(self.workers.clone())
            .with_execution_bounds(config.execution_bounds().into_engine())
            .with_run_settings_recorder(recorder);
        Ok(vec![lash_vm_runtime::lash_vm_process_engine_registration(
            engine,
        )])
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
        let lash_vm_surface = LashVmSurface::new(
            config.lash_vm_language_features.into_engine(),
            lash_vm::LashVmHostCatalog::new(),
        )
        .with_plugin_extensions(&ctx.extensions)
        .map_err(|err| PluginError::Registration(err.to_string()))?;
        let services = RlmDialectServices {
            presentation: config.presentation,
            workers: self.workers.clone(),
            code_renderer: config.code_renderer.clone(),
            artifact_store: self.artifact_store.clone(),
            deferred_tool_resolver: self.deferred_tool_resolver.clone(),
            execution_bounds: config.execution_bounds(),
            channel: config.channel,
        };
        let dialect = Arc::new(SessionDialect::new(
            Arc::clone(&self.dialect),
            lash_vm_surface,
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
}

impl lash_vm_runtime::LashVmRunSettingsRecorder for RlmProcessSettingsRecorder {
    fn record(
        &self,
        environment: &lash_core::ProcessExecutionEnvSpec,
    ) -> Result<lash_vm_runtime::LashVmRecordedSettings, PluginError> {
        let plugin_config = &environment.plugin_config;
        let captured = plugin_config
            .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
            .map_err(|error| PluginError::StoredDataCorrupt {
                record_kind: "captured RLM process config".to_owned(),
                message: error.to_string(),
            })?;
        let behaviour = captured
            .map(|recorded| recorded.behaviour)
            .unwrap_or_else(|| self.deployment_config.recorded_behaviour());
        let config = self
            .deployment_config
            .clone()
            .under_recorded_behaviour(&behaviour);
        let mut surface = rlm_lash_vm_surface(&config)
            .with_plugin_extensions(self.plugin_host.extensions())
            .map_err(|error| PluginError::Registration(error.to_string()))?;
        let context = PluginSessionContext {
            tracing: self.plugin_host.trace_runtime().clone(),
            trace: None,
            owner: lash_core::RuntimeOwner::Process(lash_core::mint_process_id()),
            tool_access: environment.tool_access.clone(),
            plugin_config: plugin_config.clone(),
            materialization: lash_core::plugin::PluginSessionMaterialization::Creation,
            extensions: self.plugin_host.extensions().clone(),
        };
        for factory in self.plugin_host.factories() {
            let plugin = factory.build(&context)?;
            let extensions =
                lash_core::PluginExtensions::from_contributions(plugin.extension_contributions());
            surface = surface
                .with_plugin_extensions(&extensions)
                .map_err(|error| PluginError::Registration(error.to_string()))?;
        }
        Ok(lash_vm_runtime::LashVmRecordedSettings::new(
            surface,
            config.execution_bounds().into_engine(),
        ))
    }
}

/// Request for [`RlmProtocolPluginFactory::lash_vm_compile_surface`].
pub struct LashVmCompileSurfaceRequest {
    pub session_id: SessionId,
    pub execution_env_spec: lash_core::ProcessExecutionEnvSpec,
}

impl LashVmCompileSurfaceRequest {
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

/// Request for [`RlmProtocolPluginFactory::compile_lash_vm_module`].
pub struct LashVmModuleCompileRequest {
    pub session_id: SessionId,
    pub source: String,
    pub execution_env_spec: lash_core::ProcessExecutionEnvSpec,
}

impl LashVmModuleCompileRequest {
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

pub struct LashVmCompileSurface {
    pub host_environment: LashVmHostEnvironment,
    pub tool_catalog: Arc<lash_core::ToolCatalog>,
    pub surface: LashVmSurface,
}

/// A compile diagnostic, worker fault, or failure to assemble the host surface.
#[derive(Clone, Debug, thiserror::Error)]
pub enum LashVmModuleCompileError {
    #[error(transparent)]
    Compile(#[from] lash_vm::ModuleCompileError),
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
    use super::rlm_lash_vm_surface;
    use crate::plugin::{InstructionBound, MemoryBound, RlmProtocolPluginConfig};

    fn base_config() -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig::builder()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .memory_limit(MemoryBound::mebibytes(64))
            .build()
    }

    fn rendered_surface(config: RlmProtocolPluginConfig) -> lash_vm::LashVmHostEnvironment {
        rlm_lash_vm_surface(&config)
            .host_environment(&lash_core::ToolCatalog::from_tool_definitions(Vec::new()))
            .expect("host environment")
    }

    /// `@label(title: "Answer") finish "ok"` — a label annotation has no
    /// TypeScript form, so the witness states the AST the host feature gate
    /// rejects.
    fn labelled_program() -> lash_vm::Program {
        use lash_vm::testing::ast_builders as b;

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
        config.lash_vm_language_features.label_annotations = false;
        let host_environment = rendered_surface(config);

        assert!(!host_environment.language_features.label_annotations);
        let err = lash_vm::LinkedModule::link(labelled_program(), &host_environment)
            .expect_err("label syntax must be rejected when the host disabled the feature");
        assert!(
            matches!(
                err,
                lash_vm::LinkError::FeatureDisabled {
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
        lash_vm::LinkedModule::link(labelled_program(), &host_environment)
            .expect("default surface links label annotations");
    }

    #[tokio::test]
    async fn typescript_parse_failure_keeps_its_own_source_span() {
        // FIG-3268: the factory used to hand `parse_failure` only
        // `span.start`, so the diagnostic's `span` stayed `None` and the
        // end of the offending token was lost.
        let factory = std::sync::Arc::new(crate::RlmProtocolPluginFactory::new(
            crate::RlmProtocolPluginConfig::builder()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(crate::InstructionBound::instructions(1_000_000))
                .memory_limit(crate::MemoryBound::mebibytes(64))
                .build(),
            std::sync::Arc::new(crate::TypescriptDialect),
            &crate::testing::sqlite_memory_store_backend().await,
        ));
        let factory_plugin: std::sync::Arc<dyn lash_core::facade_support::PluginFactory> =
            factory.clone();
        let plugin_host = lash_core::facade_support::PluginHost::new(
            vec![factory_plugin],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        );
        let source = "process 42oops() { finish \"x\" }";
        let err = factory
            .compile_lash_vm_module(
                &plugin_host,
                crate::LashVmModuleCompileRequest::new(
                    "factory-test",
                    source,
                    lash_core::ProcessExecutionEnvSpec::new(
                        Default::default(),
                        lash_core::SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                            lash_core::MaxToolCalls::new(1024),
                            lash_core::NoProgressBudget::bounded(12),
                        ),
                        lash_core::SessionToolAccess::ambient(),
                    ),
                ),
            )
            .await
            .expect_err("invalid typescript must fail to parse");

        let super::LashVmModuleCompileError::Compile(lash_vm::ModuleCompileError::Parse(
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
        assert!(config.lash_vm_language_features.label_annotations);

        let config: RlmProtocolPluginConfig = serde_json::from_value(serde_json::json!({
            "channel": "cell",
            "instruction_limit": { "bounded": 1_000_000 },
            "memory_limit": { "bounded": 67_108_864 },
            "lash_vm_language_features": { "label_annotations": false }
        }))
        .expect("rlm config");
        assert!(!config.lash_vm_language_features.label_annotations);
    }
}

/// FIG-4398, ported by FIG-5310 from the deleted
/// `recorded_inheritance_tests.rs`: a lash_vm process records one engine
/// shape at creation, from the RLM namespace its environment captured, not
/// from the factory of the deployment that installs the engine.
#[cfg(test)]
mod process_settings_tests {
    use std::sync::Arc;

    use lash_core::facade_support::{PluginFactory, PluginHost, RuntimeHostConfig};
    use lash_core::{CommitBudget, ProcessEngine as _, QueuedWorkBatchingConfig};

    use crate::plugin::{InstructionBound, MemoryBound, RlmProtocolPluginConfig};
    use crate::{RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmProtocolPluginFactory, TypescriptDialect};

    /// The deployment that records the session's RLM namespace.
    fn creating() -> RlmProtocolPluginConfig {
        RlmProtocolPluginConfig::builder()
            .channel(RlmChannel::Cell)
            .instruction_limit(InstructionBound::instructions(1_000_000))
            .memory_limit(MemoryBound::mebibytes(64))
            .build()
    }

    /// The deployment whose factory installs the engine: other bounds and
    /// features.
    fn installing() -> RlmProtocolPluginConfig {
        let mut config = RlmProtocolPluginConfig::builder()
            .channel(RlmChannel::Cell)
            .instruction_limit(InstructionBound::instructions(50))
            .memory_limit(MemoryBound::mebibytes(1))
            .build();
        config.prompt_features.decomposition = false;
        config.lash_vm_language_features.label_annotations = false;
        config.max_output_chars = 100;
        config.continue_as_soft_warn_tokens = None;
        config
    }

    fn resources() -> lash_vm::LashVmHostCatalog {
        let mut resources = lash_vm::LashVmHostCatalog::new();
        resources
            .add_named_data_type(
                lash_vm::NamedDataType::object(
                    "settings.Record",
                    vec![lash_vm::TypeField {
                        name: "value".into(),
                        ty: lash_vm::TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("a named data type"),
            )
            .expect("the type registers");
        resources
    }

    /// A plugin contributing [`resources`] to the lash_vm surface.
    fn resource_factory(
        observed: Arc<std::sync::Mutex<Vec<lash_core::SessionToolAccess>>>,
    ) -> Arc<dyn PluginFactory> {
        Arc::new(lash_core::plugin::PluginSpecFactory::new(
            lash_core::plugin::PluginDeclaration::initial("settings-resources"),
            Arc::new(move |context| {
                observed
                    .lock()
                    .expect("authority probe")
                    .push(context.tool_access.clone());
                Ok(
                    lash_core::plugin::PluginSpec::new().with_extension_contribution(
                        lash_vm_runtime::lash_vm_surface_extension(
                            &lash_vm_runtime::LashVmSurfaceContribution::new(
                                lash_vm::LashVmLanguageFeatures::default(),
                                resources(),
                            ),
                        )
                        .expect("the surface extension"),
                    ),
                )
            }),
        ))
    }

    #[tokio::test]
    async fn process_settings_have_one_recorded_engine_shape() {
        let backend = crate::testing::sqlite_memory_store_backend().await;
        let creating_factory = Arc::new(RlmProtocolPluginFactory::new(
            creating(),
            Arc::new(TypescriptDialect),
            &backend,
        ));
        let authority =
            lash_core::SessionToolAccess::restricted(Vec::new()).expect("no resident tools");
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let installing_host = PluginHost::new(
            vec![
                Arc::new(RlmProtocolPluginFactory::new(
                    installing(),
                    Arc::new(TypescriptDialect),
                    &backend,
                )),
                resource_factory(Arc::clone(&observed)),
            ],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        );
        let runtime_host = installing_host
            .install_process_engine_contributions(
                RuntimeHostConfig::new(
                    backend.clone(),
                    CommitBudget::bounded(8 * 1024 * 1024, 1024),
                    QueuedWorkBatchingConfig::new(1),
                    lash_core::ToolSourcePolicy::Tolerate,
                    lash_core::ExecutionBudgets::recommended(),
                    lash_core::runtime::DeltaCoalescing::recommended(),
                    lash_core::facade_support::DataRetentionConfig::standard(),
                ),
                true,
            )
            .expect("the engine installs");
        let plugin_config = PluginHost::new(
            vec![creating_factory.clone()],
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        )
        .resolve_creation_plugin_config(
            Some(RLM_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creating deployment records the RLM namespace");
        let environment = lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::new(plugin_config, 0),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ),
            authority.clone(),
        );
        let record = runtime_host
            .process_engines
            .require(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .expect("the lash_vm engine is installed")
            .creation_config(&environment)
            .expect("the settings record")
            .expect("captured settings are mapped into the process row");
        assert_eq!(*observed.lock().expect("authority probe"), vec![authority]);
        assert_eq!(
            record["execution_bounds"]["instruction_budget"],
            serde_json::json!({"bounded": 1_000_000})
        );
        assert_eq!(
            record["execution_bounds"]["memory_limit"],
            serde_json::json!({"bounded": 67_108_864})
        );
        assert_eq!(
            record["language_features"]["label_annotations"],
            serde_json::json!(true)
        );
        assert_eq!(
            record["resources"],
            serde_json::to_value(resources()).expect("the resources encode"),
            "creation captures dynamic plugin resources"
        );
        for unused in [
            "prompt_features",
            "max_output_chars",
            "continue_as_soft_warn_tokens",
            "discovery_operation",
            "render",
        ] {
            assert!(
                record.get(unused).is_none(),
                "process record contains unused RLM field {unused}"
            );
        }
        let hand_built = lash_vm_runtime::LashVmProcessEngine::new(
            creating_factory.artifact_store(),
            lash_vm_runtime::LashVmSurface::new(
                lash_vm::LashVmLanguageFeatures::default().with_label_annotations(),
                resources(),
            ),
        )
        .with_execution_bounds(creating().execution_bounds().into_engine());
        assert_eq!(
            hand_built
                .creation_config(&environment)
                .expect("the settings record"),
            Some(record),
            "a hand-built engine on the recorded facts records the same shape"
        );
    }
}
