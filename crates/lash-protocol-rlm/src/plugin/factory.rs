use std::sync::Arc;

use lash_core::plugin::{PluginError, PluginFactory};

use super::registration::register_rlm_protocol_plugin;
use super::{
    RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig, RlmRecordedBehaviour, RlmRecordedConfig,
};
use crate::dialect::{CellDialect, CodeModeDialectServices, SessionDialect};

pub struct RlmProtocolPluginFactory {
    config: RlmProtocolPluginConfig,
    /// The dialects this host has installed, the first being the one a
    /// session it creates records. A session's cells run in the dialect its
    /// record names, whichever of these that is.
    dialects: Vec<CellDialect>,
    workers: lash_vm_client::service::Service,
    /// The library functions the workers were assembled with; `None` is
    /// the embedding lash ships.
    worker_functions: Option<Arc<lash_kernel_doc::FunctionRegistry>>,
    /// The host's process preparation owner survives runtime binding.
    process_factory: std::sync::OnceLock<lash_vm_runtime::KernelProcessPluginFactory>,
    #[cfg(test)]
    process_factory_preparations: std::sync::atomic::AtomicUsize,
    /// The kernel version this host's process engine writes, when it
    /// stands in for the build before the synthetic successor.
    #[cfg(feature = "synthetic-next")]
    kernel_writes: Option<lash_kernel_doc::KernelVersion>,
    /// Which helper release this host's cells are written against; `None`
    /// is the build's own (FIG-5799).
    helpers: Option<Arc<dyn super::HelperReleaseGate>>,
}

impl RlmProtocolPluginFactory {
    /// An RLM protocol whose new sessions write `dialect`.
    ///
    /// A session records the dialect's name at creation and reads it back
    /// from its record on every later run: cells, prompts and tool paths go
    /// through the recorded dialect, never through whatever this host would
    /// select today. The worker service must have the dialect's kernel
    /// package installed ([`Self::with_worker_service`]).
    pub fn new(config: RlmProtocolPluginConfig, dialect: CellDialect) -> Self {
        Self {
            config,
            dialects: vec![dialect],
            workers: lash_vm_client::service::Service::default(),
            worker_functions: None,
            process_factory: std::sync::OnceLock::new(),
            #[cfg(test)]
            process_factory_preparations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "synthetic-next")]
            kernel_writes: None,
            helpers: None,
        }
    }

    /// Install another dialect: a session that recorded it runs here, in
    /// it. Sessions this host creates still record the first dialect.
    pub fn with_installed_dialect(mut self, dialect: CellDialect) -> Self {
        if !self
            .dialects
            .iter()
            .any(|installed| installed.name() == dialect.name())
        {
            self.dialects.push(dialect);
        }
        self
    }

    /// Select the host's worker entry, pool bounds and deadlines. This
    /// service lowers every cell and hosts every cell's machine.
    pub fn with_worker_service(mut self, workers: lash_vm_client::service::Service) -> Self {
        self.process_factory.take();
        self.workers = workers;
        self
    }

    pub fn worker_service(&self) -> &lash_vm_client::service::Service {
        &self.workers
    }

    /// This host as the build before the synthetic successor (ADR 0115
    /// §6): its process engine writes kernel version `writes`, reads
    /// nothing newer and migrates nothing. The two-build laws run a node
    /// of each build from one binary with it.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn writing_kernel(mut self, writes: lash_kernel_doc::KernelVersion) -> Self {
        self.process_factory.take();
        self.kernel_writes = Some(writes);
        self
    }

    /// This host as a build of helper release `release` (FIG-5799): its
    /// cells are written against that release's functions, whatever the
    /// fleet holds. The two-build laws run a node of each release from one
    /// binary with it.
    #[cfg(feature = "synthetic-next")]
    #[must_use]
    pub fn writing_helpers(self, release: u32) -> Self {
        self.with_helper_gate(Arc::new(super::helpers::WritingRelease(release)))
    }

    /// Write this host's cells against the helper release `gate` answers
    /// as each is lowered: the newest every live node of the fleet holds
    /// (FIG-5799). A host whose fleet runs one build needs none.
    #[must_use]
    pub fn with_helper_gate(mut self, gate: Arc<dyn super::HelperReleaseGate>) -> Self {
        self.helpers = Some(gate);
        self
    }

    /// Whether a helper release gate is installed.
    pub fn has_helper_gate(&self) -> bool {
        self.helpers.is_some()
    }

    /// State the library functions the worker entry was assembled with,
    /// for a host whose entry is not the one lash ships: a workflow
    /// document is linked and admitted against exactly these.
    pub fn with_worker_functions(
        mut self,
        functions: Arc<lash_kernel_doc::FunctionRegistry>,
    ) -> Self {
        self.process_factory.take();
        self.worker_functions = Some(functions);
        self
    }

    /// The bounds a new process's run is held to: the deployment's
    /// instruction and memory bounds over the worker pool's own.
    fn process_bounds(&self) -> lash_vm_client::RunBounds {
        let pool = self.workers.config().run_bounds;
        let bounds = self.config.execution_bounds();
        lash_vm_client::RunBounds {
            charge: bounds
                .instruction_limit
                .limit()
                .map_or(u64::MAX, std::num::NonZeroU64::get),
            memory: bounds
                .memory_limit
                .limit()
                .map_or(u64::MAX, std::num::NonZeroU64::get),
            call_depth: pool.call_depth,
            live_tasks: pool.live_tasks,
            requests_per_park: pool.requests_per_park,
            join_members: pool.join_members,
        }
    }

    /// The dialect a session being created records.
    fn creation_dialect(&self) -> &CellDialect {
        &self.dialects[0]
    }

    /// The dialect a session runs in: the one its record names, read back
    /// from the record. A session being created has recorded none yet and
    /// runs in the one its creation records; a rebuilt session that
    /// recorded none, or one this host has not installed, is refused.
    fn session_dialect(
        &self,
        recorded: Option<&RlmRecordedConfig>,
        materialization: lash_core::plugin::PluginSessionMaterialization,
    ) -> Result<CellDialect, PluginError> {
        match recorded.and_then(|recorded| recorded.dialect.as_deref()) {
            Some(name) => self
                .dialects
                .iter()
                .find(|dialect| dialect.name() == name)
                .cloned()
                .ok_or_else(|| PluginError::RecordedSessionConfigConflict {
                    plugin_id: RLM_PROTOCOL_PLUGIN_ID.to_string(),
                    field: "dialect".to_string(),
                    recorded: name.to_string(),
                    requested: self
                        .dialects
                        .iter()
                        .map(CellDialect::name)
                        .collect::<Vec<_>>()
                        .join(", "),
                }),
            None => match materialization {
                lash_core::plugin::PluginSessionMaterialization::Rematerialization => {
                    Err(PluginError::MissingRecordedSessionConfig {
                        plugin_id: RLM_PROTOCOL_PLUGIN_ID.to_string(),
                        field: "dialect".to_string(),
                    })
                }
                lash_core::plugin::PluginSessionMaterialization::Creation => {
                    Ok(self.creation_dialect().clone())
                }
            },
        }
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
            None => self
                .config
                .recorded_behaviour(self.creation_dialect())
                .map_err(|refusal| tool_presentation_refused(self.creation_dialect(), &refusal)),
        }
    }
}

/// `dialect`'s tool presenter's `refusal`, as the plugin's.
pub(crate) fn tool_presentation_refused(
    dialect: &CellDialect,
    refusal: &lash_tool_presenter_api::PresentationRefusal,
) -> PluginError {
    PluginError::ToolPresentationRefused {
        plugin_id: RLM_PROTOCOL_PLUGIN_ID.to_string(),
        presenter: dialect.tool_presenter().presenter().id().to_owned(),
        code: refusal.code().to_owned(),
        message: refusal.to_string(),
    }
}

/// `dialect`, refused unless its tool presenter is the one `behaviour`
/// recorded: a session presents its tools only through the presenter it
/// was created with, under the config it recorded.
fn presenting_as_recorded(
    dialect: CellDialect,
    behaviour: &RlmRecordedBehaviour,
) -> Result<CellDialect, PluginError> {
    if dialect.tool_presenter().holds(&behaviour.tool_presenter) {
        return Ok(dialect);
    }
    Err(PluginError::RecordedSessionConfigConflict {
        plugin_id: RLM_PROTOCOL_PLUGIN_ID.to_string(),
        field: "tool_presenter".to_string(),
        recorded: behaviour.tool_presenter.id.clone(),
        requested: dialect.tool_presenter().presenter().id().to_owned(),
    })
}

impl PluginFactory for RlmProtocolPluginFactory {
    fn id(&self) -> &'static str {
        RLM_PROTOCOL_PLUGIN_ID
    }

    /// The session's RLM namespace and its one command (FIG-4379): the
    /// channel and dialect this host selected are recorded at creation.
    /// The kernel process engine: a process is an entry of an admitted
    /// document, kept in the runtime's own backend and run on this
    /// factory's workers.
    /// The session's plugin runs under the behaviour the session recorded:
    /// it is pinned at creation, so the value at build is every run's.
    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash_core::plugin::PluginRecipe, lash_core::plugin::PluginError> {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<
            lash_core::plugin::PluginRecipe,
            lash_core::plugin::PluginError,
        > = {
            let mut reg = lash_core::plugin::PluginRegistrar::<RlmProtocolPlugin>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            match self.config.channel {
                super::RlmChannel::Cell => register_rlm_protocol_plugin(&mut reg)?,
                super::RlmChannel::NativeTool => {
                    crate::native::plugin::register_native_plugin(&mut reg)?
                }
            }
            let id = lash_core::plugin::PluginFactory::id(self.as_ref());
            Ok(lash_core::plugin::PluginRecipe::bound(
                id,
                reg,
                move |ctx, _state| {
                    let behaviour =
                        self.session_behaviour(&ctx.plugin_config.config, ctx.materialization)?;
                    let config = self.config.clone().under_recorded_behaviour(&behaviour);
                    let recorded = recorded_config(&ctx.plugin_config.config)?;
                    super::channel::validate_channel(
                        recorded.as_ref(),
                        self.config.channel,
                        ctx.materialization,
                    )?;
                    let dialect = presenting_as_recorded(
                        self.session_dialect(recorded.as_ref(), ctx.materialization)?,
                        &behaviour,
                    )?;
                    #[cfg(feature = "synthetic-next")]
                    let writes = self.kernel_writes;
                    #[cfg(not(feature = "synthetic-next"))]
                    let writes = None;
                    let services = CodeModeDialectServices {
                        presentation: config.presentation,
                        tool_presentation: config.tool_presentation.clone(),
                        workers: self.workers.clone(),
                        code_renderer: config.code_renderer.clone(),
                        execution_bounds: config.execution_bounds(),
                        channel: config.channel,
                        kernel: crate::executor::KernelCarry::new(
                            self.worker_functions.clone(),
                            writes,
                        ),
                        helpers: self.helpers.clone(),
                    };
                    let dialect = Arc::new(SessionDialect::new(dialect, services));

                    let runtime_state = Arc::new(
                        super::runtime_state::RlmRuntimeState::new(Arc::clone(&dialect)).map_err(
                            |error| lash_core::plugin::PluginError::Session(error.to_string()),
                        )?,
                    );
                    let code_executor = Arc::new(super::runtime_state::CodeModeCodeExecutor::new(
                        Arc::clone(&runtime_state),
                    ));
                    let protocol_session =
                        Arc::new(super::protocol_session::RlmProtocolSession::new(
                            config.clone(),
                            runtime_state,
                        ));
                    let mask = crate::stream_mask::bind_stream_mask(Arc::clone(&dialect));
                    Ok(Arc::new(RlmProtocolPlugin {
                        config,
                        dialect,
                        code_executor,
                        protocol_session,
                        mask,
                    }))
                },
            ))
        };
        let mut recipe = recipe?.with_transcript_decoder(Arc::new(
            crate::projection::transcript::RlmTranscriptDecoder,
        ));
        let mut configuration = lash_core::plugin::ConfigRegistrar::new(
            lash_core::plugin::PluginFactory::id(recipe_factory.as_ref()),
        );
        recipe_factory.declare_configuration(&mut configuration)?;
        recipe = recipe.with_configuration(configuration);
        let engine_factory = Arc::clone(&recipe_factory);
        recipe = recipe.with_process_engine(lash_vm_runtime::LASH_VM_ENGINE_KIND, move |context| {
            engine_factory.bind_process_engine(context)
        });
        Ok(recipe)
    }
}
impl RlmProtocolPluginFactory {
    pub fn declare_configuration(
        &self,
        registrar: &mut lash_core::plugin::ConfigRegistrar,
    ) -> Result<(), lash_core::plugin::ConfigRegistrationError> {
        super::config_owner::register(
            registrar,
            super::config_owner::RlmConfigOwner {
                channel: self.config.channel,
                dialect: self.creation_dialect().clone(),
                config: self.config.clone(),
            },
        )
    }
    pub fn bind_process_engine(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<lash_core::ProcessEngineRegistration, PluginError> {
        self.process_factory().bind_process_engine(ctx)
    }

    fn process_factory(&self) -> &lash_vm_runtime::KernelProcessPluginFactory {
        self.process_factory.get_or_init(|| {
            #[cfg(test)]
            self.process_factory_preparations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // The processes a session's cells start run documents written in
            // the session's dialect.
            let factory = lash_vm_runtime::KernelProcessPluginFactory::new(
                self.workers.clone(),
                self.creation_dialect().name(),
                self.process_bounds(),
            );
            let factory = match &self.worker_functions {
                Some(functions) => factory.with_functions(Arc::clone(functions)),
                None => factory,
            };
            #[cfg(feature = "synthetic-next")]
            let factory = match self.kernel_writes {
                Some(writes) => factory.writing_kernel(writes),
                None => factory,
            };
            factory
        })
    }
}

impl RlmProtocolPluginFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(RLM_PROTOCOL_PLUGIN_ID)
    }
}
impl lash_core::plugin::PluginMetadata for RlmProtocolPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
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

pub(crate) struct RlmProtocolPlugin {
    pub(crate) config: RlmProtocolPluginConfig,
    pub(crate) dialect: Arc<SessionDialect>,
    pub(crate) code_executor: Arc<super::runtime_state::CodeModeCodeExecutor>,
    pub(crate) protocol_session: Arc<super::protocol_session::RlmProtocolSession>,
    pub(crate) mask: crate::stream_mask::StreamMaskBindings,
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::plugin::PluginSessionMaterialization::{Creation, Rematerialization};

    /// A recorded namespace naming `dialect`.
    fn recorded(dialect: Option<&str>) -> RlmRecordedConfig {
        let config = RlmProtocolPluginConfig::standard()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::unbounded())
            .memory_limit(crate::MemoryBound::unbounded())
            .build();
        RlmRecordedConfig {
            finish_value_schema: None,
            render: None,
            termination: None,
            channel: Some(crate::RlmChannel::Cell),
            dialect: dialect.map(str::to_owned),
            behaviour: config
                .recorded_behaviour(&CellDialect::typescript())
                .expect("the standard tool presentation admits"),
        }
    }

    /// FIG-6148: the RLM hosting factory must preserve the process
    /// preparation owner across independently created runtime bindings.
    #[test]
    fn process_runtime_bindings_prepare_their_factory_once() {
        let factory = factory();
        for _ in 0..3 {
            let _binding = factory.process_factory();
        }
        assert_eq!(
            factory
                .process_factory_preparations
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let factory =
            factory.with_worker_functions(Arc::new(lash_kernel_doc::FunctionRegistry::new()));
        for _ in 0..3 {
            let _binding = factory.process_factory();
        }
        assert_eq!(
            factory
                .process_factory_preparations
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        let factory = factory.with_worker_service(lash_vm_client::service::Service::default());
        for _ in 0..3 {
            let _binding = factory.process_factory();
        }
        assert_eq!(
            factory
                .process_factory_preparations
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
    }

    fn factory() -> RlmProtocolPluginFactory {
        let config = RlmProtocolPluginConfig::standard()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::unbounded())
            .memory_limit(crate::MemoryBound::unbounded())
            .build();
        RlmProtocolPluginFactory::new(config, CellDialect::typescript())
    }

    /// A host whose tool presentation states `presentation`, presenting
    /// through `dialect`.
    fn presenting_host(
        dialect: CellDialect,
        presentation: serde_json::Value,
    ) -> RlmProtocolPluginFactory {
        let mut config = RlmProtocolPluginConfig::standard()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::unbounded())
            .memory_limit(crate::MemoryBound::unbounded())
            .build();
        config.tool_presentation = presentation;
        RlmProtocolPluginFactory::new(config, dialect)
    }

    /// FIG-6489: a session records its tool presenter's id and the complete
    /// config the presenter admitted, and every open presents under that
    /// record: a reopening host's changed tool presentation is never read,
    /// a host whose slot holds another presenter is refused with a typed
    /// conflict, and a record that names no presenter is refused.
    #[test]
    fn a_session_presents_its_tools_as_its_record_names() {
        let creator = presenting_host(CellDialect::typescript(), serde_json::json!({}));
        let behaviour = creator
            .session_behaviour(&lash_core::PluginConfig::default(), Creation)
            .expect("a session being created");
        assert_eq!(
            behaviour.tool_presenter,
            lash_tool_presenter_api::RecordedToolPresenter {
                id: "lash.tools.typescript.v1".to_owned(),
                config: serde_json::json!({}),
            },
            "the record is the admitted config whole, not the keys the host stated"
        );
        let mut namespace = recorded(Some("typescript"));
        namespace.behaviour = behaviour.clone();
        let mut stored = lash_core::PluginConfig::default();
        stored.insert(
            RLM_PROTOCOL_PLUGIN_ID,
            serde_json::to_value(&namespace).expect("the recorded namespace encodes"),
        );

        let reopening = presenting_host(
            CellDialect::typescript(),
            serde_json::json!({ "schema_depth": 2 }),
        );
        let reopened = reopening
            .session_behaviour(&stored, Rematerialization)
            .expect("a recorded session");
        assert_eq!(reopened, behaviour);
        assert_eq!(
            reopening
                .config
                .clone()
                .under_recorded_behaviour(&reopened)
                .tool_presentation,
            behaviour.tool_presenter.config,
            "the session presents under its record, never this host's presentation"
        );
        presenting_as_recorded(CellDialect::typescript(), &reopened)
            .expect("the recorded presenter is installed");

        let replaced = CellDialect::typescript().with_tool_presenter(
            lash_tool_presenter_api::ToolPresenterSlot::new(
                lash_tool_presenter_api::WrappedToolPresenter::new(
                    "host.tools.v1",
                    lash_tool_presenter_typescript::TypescriptToolPresenter,
                ),
            ),
        );
        let refused = presenting_as_recorded(replaced, &reopened)
            .expect_err("the recorded presenter is not in this host's slot");
        assert!(
            matches!(
                &refused,
                PluginError::RecordedSessionConfigConflict { field, recorded, requested, .. }
                    if field == "tool_presenter"
                        && recorded == "lash.tools.typescript.v1"
                        && requested == "host.tools.v1"
            ),
            "{refused:?}"
        );

        let mut unpresented = serde_json::to_value(&namespace).expect("the namespace encodes");
        unpresented["behaviour"]
            .as_object_mut()
            .expect("a recorded behaviour")
            .remove("tool_presenter");
        let mut stored = lash_core::PluginConfig::default();
        stored.insert(RLM_PROTOCOL_PLUGIN_ID, unpresented);
        let missing = reopening
            .session_behaviour(&stored, Rematerialization)
            .expect_err("a record that names no presenter");
        assert!(
            matches!(&missing, PluginError::Session(message) if message.contains("tool_presenter")),
            "{missing:?}"
        );

        let refusing = presenting_host(
            CellDialect::typescript(),
            serde_json::json!({ "schema_depth": "two" }),
        );
        assert!(matches!(
            refusing.session_behaviour(&lash_core::PluginConfig::default(), Creation),
            Err(PluginError::ToolPresentationRefused { ref presenter, ref code, .. })
                if presenter == "lash.tools.typescript.v1" && code == "config.invalid"
        ));
    }

    /// A session runs in the dialect its record names, whatever this host
    /// would select for a session it creates today; a recorded dialect the
    /// host has not installed, and a rebuilt session that recorded none,
    /// are refused.
    #[test]
    fn a_session_runs_in_the_dialect_its_record_names() {
        let host = factory().with_installed_dialect(CellDialect::python());
        let python = recorded(Some("python"));
        for materialization in [Creation, Rematerialization] {
            let dialect = host
                .session_dialect(Some(&python), materialization)
                .expect("an installed dialect");
            assert_eq!(dialect.name(), "python");
        }
        assert_eq!(
            host.session_dialect(None, Creation)
                .expect("a session being created")
                .name(),
            "typescript",
            "a session this host creates records its first dialect"
        );

        let refused = factory()
            .session_dialect(Some(&python), Rematerialization)
            .expect_err("python is not installed here");
        assert!(matches!(
            refused,
            PluginError::RecordedSessionConfigConflict { ref field, ref recorded, .. }
                if field == "dialect" && recorded == "python"
        ));
        assert!(matches!(
            host.session_dialect(Some(&recorded(None)), Rematerialization),
            Err(PluginError::MissingRecordedSessionConfig { ref field, .. }) if field == "dialect"
        ));
    }
}
