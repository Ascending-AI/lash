use crate::support::{EmbedError, ProtocolTurnOptions, Result, SessionError};
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
/// The write half of the pair is the session config patch: state the facts
/// with [`rlm_session_config_patch`] and apply it with
/// [`SessionConfigAdmin::update`](crate::admin::SessionConfigAdmin::update).
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
/// plugin options keyed by [`RLM_PROTOCOL_PLUGIN_ID`], and a reopen never
/// changes them (FIG-4099). Every later change is the one durable config
/// command: [`rlm_session_config_patch`] states the facts, and
/// [`SessionConfigAdmin::update`](crate::admin::SessionConfigAdmin::update) applies
/// them as a guarded set-if-unset — a fact is written only where the session
/// recorded nothing, restating a recorded fact is a no-op, and a *different*
/// value is refused with the typed [`RlmSessionConfigConflict`]
/// ([`rlm_session_config_conflict`] reads it off the error). A host that
/// wants a fact *asserted* compares [`RlmSessionExt::rlm_config`] against
/// what it requires and refuses loudly.
///
/// There is no language among these facts: TypeScript is the sole RLM dialect
/// (ADR 0096), so a session neither states nor records one, and a session that
/// still carries a recorded `dialect` is refused as an incompatible format
/// rather than read.
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

/// A session config patch that states `config`'s RLM facts, and nothing else
/// (FIG-4099).
///
/// Apply it with [`SessionConfigAdmin::update`](crate::admin::SessionConfigAdmin::update);
/// a stated fact that conflicts with the recorded one is refused typed, and
/// [`rlm_session_config_conflict`] reads the conflict off the error.
#[cfg(feature = "rlm")]
pub fn rlm_session_config_patch(
    config: &lash_rlm_types::RlmSessionConfig,
) -> Result<crate::SessionConfigPatch> {
    let plugin_options = lash_core::PluginOptions::typed(
        RLM_PROTOCOL_PLUGIN_ID,
        lash_rlm_types::RlmCreateExtras::from(config),
    )
    .map_err(EmbedError::ProtocolTurnOptions)?;
    Ok(crate::SessionConfigPatch {
        plugin_options: Some(plugin_options),
        ..crate::SessionConfigPatch::default()
    })
}

/// The RLM fact conflict an [`update`](crate::admin::SessionConfigAdmin::update) was
/// refused with, if that is why it was refused.
#[cfg(feature = "rlm")]
pub fn rlm_session_config_conflict(
    error: &EmbedError,
) -> Option<&lash_rlm_types::RlmSessionConfigConflict> {
    match error {
        EmbedError::Session(SessionError::SessionConfigRefused(refusal)) => refusal.downcast_ref(),
        _ => None,
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
pub use lash_protocol_rlm::{
    BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ExecutionBounds, InstructionBound,
    MemoryBound, NamedDataType, RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmProtocolPluginConfig,
    RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory, RlmSessionConfigDecodeError,
    TypeExpr, TypeField, UnsetBound, format_type_expr,
};
/// Projection vocabulary: bind projected values to the active session via
/// [`rlm_session_projection_extension`]. Session extensions are process-local
/// runtime configuration; durable session seeds use [`RlmSeed`].
pub use lash_protocol_rlm::{RlmProjectedBindings, RlmSeed, rlm_session_projection_extension};
pub use lash_render::{RenderParams, RenderParamsPatch};
pub use lash_rlm_types::{
    RlmCreateExtras, RlmFinalAnswerFormat, RlmRenderPatch, RlmSessionConfig,
    RlmSessionConfigConflict, RlmTermination, RlmTurnOptions,
};
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
