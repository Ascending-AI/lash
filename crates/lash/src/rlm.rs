use crate::support::{ProtocolTurnOptions, Result};
use lash_core::facade_support::ProtocolTurnOptionsFacadeOps;

#[cfg(feature = "rlm")]
pub use lash_lashlang_runtime::LanguageTraceHost;

/// The RLM termination setters on a [`send`](crate::LashSession::send).
#[cfg(feature = "rlm")]
pub trait RlmSendBuilderExt: Sized {
    /// Requires the RLM turn to finish through the finish tool.
    fn require_finish(self) -> Result<Self>;
    /// Requires the RLM finish tool to produce a value matching the schema.
    fn require_finish_schema(self, schema: serde_json::Value) -> Result<Self>;
    /// Allows an RLM turn to return prose or invoke the finish tool.
    fn allow_prose_or_finish(self) -> Result<Self>;
}

#[cfg(feature = "rlm")]
impl RlmSendBuilderExt for crate::SendBuilder {
    fn require_finish(self) -> Result<Self> {
        with_rlm_termination(
            self,
            lash_rlm_types::RlmTermination::FinishRequired { schema: None },
        )
    }

    fn require_finish_schema(self, schema: serde_json::Value) -> Result<Self> {
        with_rlm_termination(
            self,
            lash_rlm_types::RlmTermination::FinishRequired {
                schema: Some(schema),
            },
        )
    }

    fn allow_prose_or_finish(self) -> Result<Self> {
        with_rlm_termination(self, lash_rlm_types::RlmTermination::Natural)
    }
}

/// `builder` with `termination` recorded in its run spec's protocol turn
/// options, over whatever options the builder already set.
#[cfg(feature = "rlm")]
fn with_rlm_termination(
    mut builder: crate::SendBuilder,
    termination: lash_rlm_types::RlmTermination,
) -> Result<crate::SendBuilder> {
    builder.run_spec.overrides.protocol_turn_options = Some(rlm_termination_options(
        builder.run_spec.overrides.protocol_turn_options.as_ref(),
        termination,
    )?);
    Ok(builder)
}

/// Reads the durable RLM facts a session actually recorded (ADR 0066).
///
/// Every field is `Option`-shaped: `None` is "this session has stated nothing",
/// which is a different answer from the value the default resolves to. Anything
/// a host labels with a language — a rendered transcript, an API payload, an
/// evidence bundle — reads the recorded value rather than repeating its own
/// configuration, or it labels the wrong value precisely in the case the label
/// exists to disambiguate.
///
/// The facts are recorded when the session is created, from the creator's
/// plugin options keyed by [`RLM_PROTOCOL_PLUGIN_ID`], and no config command
/// changes them (FIG-4379).
///
/// The read is strict (FIG-1979): a bag that does not decode is an error, not
/// an empty config. A swallowed decode failure reads as "this session recorded
/// nothing", which is the one answer a host must never infer from a corrupted
/// bag — it resolves every fact to a default the session is not running.
#[cfg(feature = "rlm")]
pub trait RlmSessionReadViewExt {
    /// The RLM config this session recorded, as recorded.
    fn rlm_config(
        &self,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigDecodeError>;
}

#[cfg(feature = "rlm")]
impl RlmSessionReadViewExt for lash_core::SessionReadView {
    fn rlm_config(
        &self,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigDecodeError> {
        lash_protocol_rlm::rlm_session_config(self.protocol_turn_options())
    }
}

/// The durable RLM facts of an opened session, read as recorded (ADR 0066).
///
/// A session's RLM facts are baked in when it is created, from the creator's
/// plugin options keyed by [`RLM_PROTOCOL_PLUGIN_ID`], and neither a reopen
/// nor a config command changes them (FIG-4099, FIG-4379): a turn restates
/// them through its run's protocol turn options. The one RLM setting a
/// session changes is its render preferences, through the config command
/// [`SetRlmRender`]. A host that wants a fact *asserted* compares
/// [`RlmSessionExt::rlm_config`] against what it requires and refuses
/// loudly.
///
/// The session's dialect is not among these facts: the host selects it where
/// it constructs the RLM protocol, the session records its language id when
/// it is created, and a host that selects another dialect is refused when it
/// reopens the session (ADR 0096).
#[cfg(feature = "rlm")]
pub trait RlmSessionExt {
    /// The RLM config this session recorded, as recorded.
    fn rlm_config(
        &self,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigDecodeError>;
}

#[cfg(feature = "rlm")]
impl RlmSessionExt for crate::LashSession {
    fn rlm_config(
        &self,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigDecodeError> {
        self.read_view().rlm_config()
    }
}

// RLM-specific Lashlang host vocabulary. The catalogue-preview, tool-binding,
// and process-input names are single-homed under `lash::tools` and
// `lash::process`; they are not re-exported here.
pub use lash_lashlang_runtime::{
    DeferredTriggerProvider, DeferredTriggerProviderRegistry, DeferredTriggerResolutionError,
    DeferredTriggerResolver, LASHLANG_SURFACE_EXTENSION_ID, LashlangAbilities, LashlangHostCatalog,
    LashlangHostEnvironment, LashlangLanguageFeatures, LashlangProcessEngine, LashlangSurface,
    LashlangSurfaceContribution, SharedDeferredTriggerResolver, TriggerGrant, TriggerResolution,
    lashlang_surface_extension,
};
pub use lash_lashlang_runtime::{
    LashlangProcessAdmissionRefusal, LashlangRuntimeError, ToolBindingError,
};
pub use lash_protocol_rlm::{
    BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ExecutionBounds, InstructionBound,
    MemoryBound, NamedDataType, RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmProtocolPluginConfig,
    RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory, RlmSessionConfigDecodeError,
    TypeExpr, TypeField, UnsetBound, format_type_expr,
};
/// The code-mode dialect seam: a host selects one [`Dialect`] where it
/// constructs the RLM protocol; [`TypescriptDialect`] is the shipped one.
pub use lash_protocol_rlm::{
    CellTags, Dialect, DialectPromptVocabulary, DialectRefusal, DialectRefusalKind,
    ExecutionSectionRequest, ResolvedToolBinding, ShapeNotation, TypescriptDialect,
};
/// The config groups and builder state an [`RlmProtocolPluginConfig`] is
/// assembled from.
pub use lash_protocol_rlm::{RlmAbilities, RlmLanguageFeatures, RlmPromptFeatures, UnsetChannel};
/// The RLM protocol's config owner and its one command (FIG-4379).
pub use lash_protocol_rlm::{
    RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig, RlmRecordedConfig, SetRlmRender,
};
/// Projection vocabulary: bind projected values to the active session via
/// [`rlm_session_projection_extension`]. Session extensions are process-local
/// runtime configuration; durable session seeds use [`RlmSeed`].
pub use lash_protocol_rlm::{RlmProjectedBindings, RlmSeed, rlm_session_projection_extension};
pub use lash_render::{RenderParams, RenderParamsPatch};
pub use lash_rlm_types::{
    RlmCreateExtras, RlmFinalAnswerFormat, RlmRenderPatch, RlmSessionConfig, RlmTermination,
    RlmTurnOptions,
};
pub use lash_rlm_types::{RlmProjectedSeedEntry, RlmProjectedSeedSnapshot, RlmSeedPluginBody};
pub use lashlang::LinkedModule;

/// The Lashlang compile APIs are operations over an
/// [`RlmProtocolPluginFactory`] and a plugin host; they live in
/// `lash-protocol-rlm` and are re-exported here.
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput,
};

/// The Lashlang language surface: the AST, values, compile/link requests,
/// introspection, trigger vocabulary, resource operations and artifact-store
/// traits a host needs to author, compile and inspect Lashlang programs.
///
/// This is `lashlang`'s own root namespace, re-exported whole rather than
/// item-by-item: the language vocabulary is the internal crate's root and
/// splitting it here would give the same name two homes.
pub mod lang {
    pub use lashlang::*;
}

/// `current` with its RLM termination overridden by `termination`.
#[cfg(feature = "rlm")]
fn rlm_termination_options(
    current: Option<&ProtocolTurnOptions>,
    termination: lash_rlm_types::RlmTermination,
) -> Result<ProtocolTurnOptions> {
    let override_options = ProtocolTurnOptions::typed(lash_rlm_types::RlmTurnOptions {
        termination: Some(termination),
        final_answer_format: None,
        render: None,
    })?;
    Ok(current
        .map(|current| current.merged_with_override(&override_options))
        .unwrap_or(override_options))
}

/// One shared pool for RLM cells, process bodies, and pure language work.
///
/// SDK releases attach `lash-sdk-worker-VERSION-TARGET.tar.gz` and its SHA256.
/// Pass the extracted `bin/lash-vm-worker` path to [`WorkerService::subprocess`]
/// or [`WorkerEntry::helper`]. Hosts may build the SDK from registry packages.
/// The manifest records protocol and crate diagnostics; crate versions never
/// decide compatibility. Pool admission refuses an unsupported wire version.
/// [`WorkerService::default`] explicitly defaults to the helper beside the host
/// executable and does not search PATH or a repository.
///
/// A single-binary host calls [`worker_entry_with_frontend`] as its first action,
/// before runtime creation, credentials, stores or providers, and returns from
/// main when that call returns `true`. It selects [`WorkerEntry::reexec`].
/// `examples/worker_host.rs` proves this bootstrap with the TypeScript frontend.
/// The child starts with an empty environment and closes inherited descriptors.
/// The language bounds guest authority; the process contains native crashes.
/// A native escape still has the worker user's OS access.
pub use lash_vm_client::service::Service as WorkerService;
/// Host-selected worker entry, pool bounds, and execution deadlines.
pub use lash_vm_client::{
    Deadlines as WorkerDeadlines, PoolConfig as WorkerPoolConfig, WorkerEntry,
};

/// A source frontend lives in the worker entry the dialect selects.
pub use lash_vm_worker::{
    Frontend as WorkerFrontend, FrontendRefusal as WorkerFrontendRefusal,
    worker_entry_with_frontend,
};
