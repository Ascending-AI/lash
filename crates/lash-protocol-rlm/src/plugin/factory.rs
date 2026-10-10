use std::sync::Arc;

use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};

use super::registration::register_rlm_protocol_plugin;
use super::{
    RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginConfig, RlmRecordedBehaviour, RlmRecordedConfig,
};
use crate::deferred::SharedDeferredToolResolver;
use crate::dialect::{CellDialect, RlmDialectServices, SessionDialect};

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
    /// The kernel version this host's process engine writes, when it
    /// stands in for the build before the synthetic successor.
    #[cfg(feature = "synthetic-next")]
    kernel_writes: Option<lash_kernel_doc::KernelVersion>,
    deferred_tool_resolver: Option<SharedDeferredToolResolver>,
    /// Which helper release this host's cells are written against; `None`
    /// is the build's own (FIG-5799).
    helpers: Option<Arc<dyn super::HelperReleaseGate>>,
    /// Whether the kernel engine adopts processes written against an
    /// earlier helper release onto the build's own (FIG-5799).
    adopting_helpers: bool,
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
            #[cfg(feature = "synthetic-next")]
            kernel_writes: None,
            deferred_tool_resolver: None,
            helpers: None,
            adopting_helpers: false,
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

    /// Adopt each kernel process this host's node claims that pins a
    /// function of a helper release before the build's own onto the
    /// build's function of the same name, when its run is not parked
    /// inside a helper the adoption changes (FIG-5799): the operator's
    /// choice before a build that stops retaining that release starts. A
    /// process it does not adopt goes on as written, and `lashctl
    /// kernel-migration list` names it with the typed reason.
    #[must_use]
    pub fn adopting_helpers(mut self) -> Self {
        self.adopting_helpers = true;
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

    /// Wire a host-provided [`DeferredToolResolver`](crate::DeferredToolResolver)
    /// that resolves each cell's batch of call paths absent from the
    /// session's tools into per-path Tool Grants or unavailable outcomes.
    /// Most hosts ship none.
    pub fn with_deferred_tool_resolver(mut self, resolver: SharedDeferredToolResolver) -> Self {
        self.deferred_tool_resolver = Some(resolver);
        self
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
            None => Ok(self.config.recorded_behaviour()),
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
                dialect: self.creation_dialect().name().to_owned(),
                config: self.config.clone(),
            },
        )
    }

    /// The kernel process engine: a process is an entry of an admitted
    /// document, kept in the runtime's own backend and run on this
    /// factory's workers.
    fn process_engine_contributions(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, PluginError> {
        let factory = lash_vm_runtime::KernelProcessPluginFactory::new(
            self.workers.clone(),
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
        let factory = if self.adopting_helpers {
            factory.adopting_helpers()
        } else {
            factory
        };
        factory.process_engine_contributions(ctx)
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
        let dialect = self.session_dialect(recorded.as_ref(), ctx.materialization)?;
        #[cfg(feature = "synthetic-next")]
        let writes = self.kernel_writes;
        #[cfg(not(feature = "synthetic-next"))]
        let writes = None;
        let services = RlmDialectServices {
            presentation: config.presentation,
            workers: self.workers.clone(),
            code_renderer: config.code_renderer.clone(),
            deferred_tool_resolver: self.deferred_tool_resolver.clone(),
            execution_bounds: config.execution_bounds(),
            channel: config.channel,
            kernel: crate::executor::KernelCarry::new(self.worker_functions.clone(), writes),
            helpers: self.helpers.clone(),
        };
        let dialect = Arc::new(SessionDialect::new(dialect, services));
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
            render: None,
            termination: None,
            channel: Some(crate::RlmChannel::Cell),
            dialect: dialect.map(str::to_owned),
            behaviour: config.recorded_behaviour(),
        }
    }

    fn factory() -> RlmProtocolPluginFactory {
        let config = RlmProtocolPluginConfig::standard()
            .channel(crate::RlmChannel::Cell)
            .instruction_limit(crate::InstructionBound::unbounded())
            .memory_limit(crate::MemoryBound::unbounded())
            .build();
        RlmProtocolPluginFactory::new(config, CellDialect::typescript())
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
