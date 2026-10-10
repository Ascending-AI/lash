use crate::support::{ProtocolTurnOptions, Result};

/// The RLM completion setters on a [`send`](crate::LashSession::send).
#[cfg(feature = "rlm")]
pub trait RlmSendBuilderExt: Sized {
    /// Requires the RLM turn to end through a declared control call
    /// (`control.finish`, `control.continue_as` or a host control tool):
    /// prose alone never ends it.
    fn require_finish(self) -> Result<Self>;
    /// Allows an RLM turn to end with prose or through a control call.
    fn allow_prose_or_finish(self) -> Result<Self>;
    /// States the schema `control.finish` takes its value under for this
    /// turn: the host's final-answer schema. A value that does not match
    /// fails that call, and the turn goes on.
    fn finish_schema(self, schema: serde_json::Value) -> Result<Self>;
}

#[cfg(feature = "rlm")]
impl RlmSendBuilderExt for crate::SendBuilder {
    fn require_finish(self) -> Result<Self> {
        with_rlm_options(self, |options| {
            options.termination = Some(lash_core::TerminationMode::TerminalRequired);
            Ok(())
        })
    }

    fn allow_prose_or_finish(self) -> Result<Self> {
        with_rlm_options(self, |options| {
            options.termination = Some(lash_core::TerminationMode::Natural);
            Ok(())
        })
    }

    fn finish_schema(self, schema: serde_json::Value) -> Result<Self> {
        with_rlm_options(self, |options| {
            options.finish_schema = Some(admit_finish_schema(schema)?);
            Ok(())
        })
    }
}

#[cfg(feature = "rlm")]
fn admit_finish_schema(schema: serde_json::Value) -> Result<lash_core::JsonSchema> {
    lash_core::JsonSchema::admit(schema).map_err(|source| {
        crate::EmbedError::Plugin(lash_core::PluginError::UnusableSchema {
            source: Box::new(source),
        })
    })
}

/// `builder` with `state` applied to the RLM run options its run spec
/// already states. Options that are not the RLM owner's run options are an
/// error here, before anything is sent.
#[cfg(feature = "rlm")]
fn with_rlm_options(
    mut builder: crate::SendBuilder,
    state: impl FnOnce(&mut lash_rlm_types::RlmTurnOptions) -> Result<()>,
) -> Result<crate::SendBuilder> {
    let mut options = builder
        .run_spec
        .overrides
        .protocol_turn_options
        .as_ref()
        .map(ProtocolTurnOptions::decode::<lash_rlm_types::RlmTurnOptions>)
        .transpose()?
        .unwrap_or_default();
    state(&mut options)?;
    builder.run_spec.overrides.protocol_turn_options = Some(ProtocolTurnOptions::typed(options)?);
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
/// nor a config command changes them (FIG-4099, FIG-4379): a turn states
/// them again through its run's protocol turn options. The RLM setting a
/// session changes is its render preferences, through the config command
/// [`SetRlmRender`]. Its prompt is the protocol's keyed sections
/// ([`rlm_section_keys`]), which a host's own sections and wrappers complement or
/// replace ([`crate::plugins::PromptSection`]). A host that wants a fact
/// *asserted* compares
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

// RLM-specific Lash VM host vocabulary. The catalogue-preview, tool-binding,
// and process-input names are single-homed under `lash::tools` and
// `lash::process`; they are not re-exported here.
/// How a session's turns may end.
pub use lash_core::TerminationMode;
/// Which helper release a host's cells are written against (FIG-5799).
pub use lash_protocol_rlm::HelperReleaseGate;
/// Identifies the RLM protocol's durable output by its typed message origin.
pub use lash_protocol_rlm::is_rlm_protocol_output;
/// The initial nodes that bind a [`RlmSeed`] in a session being created: a
/// creator states them on its create request (FIG-5296).
pub use lash_protocol_rlm::rlm_seed_initial_nodes;
pub use lash_protocol_rlm::{
    BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ExecutionBounds, InstructionBound,
    MemoryBound, RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmPresentationConfig,
    RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory,
    RlmSessionConfigDecodeError, UnsetBound, rlm_protocol_event,
};
/// The stable codes of the typed observations a cell's failure carries, and
/// the error kinds a cell's program catches from a tool call.
pub use lash_protocol_rlm::{
    CELL_BOUND_EXCEEDED, CELL_DEADLOCK, CELL_TASKS_OUTSTANDING, CONTROL_REFUSED,
    SESSION_BINDING_NOT_CARRIED, TOOL_ARGUMENTS, TOOL_CALL_LIMIT, TOOL_FAILED, UNKNOWN_EFFECT,
};
/// The code-mode dialect seam: a host selects the [`CellDialect`] its new
/// sessions write where it constructs the RLM protocol, and installs any
/// other a recorded session may name. A dialect is a kernel dialect package
/// by name with the [`DialectPrompts`] that word prompts in it;
/// [`CellDialect::typescript`] and [`CellDialect::python`] are the shipped
/// ones.
pub use lash_protocol_rlm::{
    CellDialect, CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal,
    DialectRefusalKind, ExecutionSection, ExecutionSectionRequest, PythonPrompts,
    ResolvedToolBinding, TypescriptPrompts,
};
/// The schema shapes a [`DialectPrompts`] is handed to spell: one reading of a tool's
/// JSON Schemas, shared by every prompt surface.
pub use lash_protocol_rlm::{
    ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaShape, ShapeConstraints,
    ShapeField, ShapeKind, ShapeRow,
};
/// Lash's two control tools, as a session advertises them.
pub use lash_protocol_rlm::{
    FINISH_TOOL_NAME, continue_as_tool_definition, finish_tool_definition,
};
/// Projection vocabulary: bind projected values to the active session via
/// [`rlm_session_projection_extension`], a durable session extension the
/// session's command lane records as an [`RlmSeed`] event (FIG-5134).
pub use lash_protocol_rlm::{
    ProjectedBindingError, RlmProjectedBindings, RlmSeed, rlm_session_projection_extension,
};
/// The RLM protocol's config owner and its command (FIG-4379).
pub use lash_protocol_rlm::{
    RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig, RlmRecordedBehaviour, RlmRecordedConfig,
    RlmRenderRefusal, RlmRunOptions, SetRlmRender,
};
/// The code-mode prompt sections (ADR 0133): the keys the protocol
/// registers its sections under, and its built-in intro.
pub use lash_protocol_rlm::{RlmProjectorConfig, section_id, section_keys as rlm_section_keys};
/// The config groups and builder state an [`RlmProtocolPluginConfig`] is
/// assembled from.
pub use lash_protocol_rlm::{RlmPromptFeatures, UnsetChannel};
/// Restore checks retained as the source of
/// [`crate::SessionError::ExecutionStateRestore`].
pub use lash_protocol_rlm::{RlmSnapshotError, snapshot};
pub use lash_render::{RenderParams, RenderParamsPatch};
/// The committed RLM event variants the protocol owns and
/// the record types their fields name.
pub use lash_rlm_types::{
    RlmAssistantContent, RlmDiagnosticEvent, RlmDiagnosticPhase, RlmGlobalsPatchPluginBody,
    RlmProtocolEvent,
};
pub use lash_rlm_types::{RlmCreateExtras, RlmRenderPatch, RlmSessionConfig, RlmTurnOptions};
pub use lash_rlm_types::{RlmProjectedSeedEntry, RlmProjectedSeedSnapshot, RlmSeedPluginBody};

pub use lash_protocol_rlm::recorded_extraction_decisions;
