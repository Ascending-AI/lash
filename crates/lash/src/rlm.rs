use crate::support::*;
use lash_core::facade_support::ProtocolTurnOptionsFacadeOps;

#[cfg(feature = "rlm")]
/// Adds RLM-specific completion constraints to turn builders.
pub trait RlmTurnBuilderExt: Sized {
    /// Requires the RLM turn to finish through the finish tool.
    fn require_finish(self) -> Result<Self>;
    /// Requires the RLM finish tool to produce a value matching the schema.
    fn require_finish_schema(self, schema: serde_json::Value) -> Result<Self>;
    /// Allows an RLM turn to return prose or invoke the finish tool.
    fn allow_prose_or_finish(self) -> Result<Self>;
}

#[cfg(feature = "rlm")]
impl RlmTurnBuilderExt for TurnBuilder {
    fn require_finish(self) -> Result<Self> {
        rlm_termination(
            self,
            lash_rlm_types::RlmTermination::FinishRequired { schema: None },
        )
    }

    fn require_finish_schema(self, schema: serde_json::Value) -> Result<Self> {
        rlm_termination(
            self,
            lash_rlm_types::RlmTermination::FinishRequired {
                schema: Some(schema),
            },
        )
    }

    fn allow_prose_or_finish(self) -> Result<Self> {
        rlm_termination(self, lash_rlm_types::RlmTermination::Natural)
    }
}

/// Reads the durable RLM facts a session actually recorded (ADR 0066).
///
/// Every field is `Option`-shaped: `None` is "this session has stated nothing",
/// which is a different answer from the value the default resolves to. Anything
/// a host labels with a language — a rendered transcript, an API payload, an
/// evidence bundle — reads the recorded value rather than repeating its own
/// configuration, or it labels the wrong dialect precisely in the case the label
/// exists to disambiguate.
///
/// The write half of the pair is
/// [`RlmSessionExt::set_rlm_config_if_unset`].
#[cfg(feature = "rlm")]
pub trait RlmSessionReadViewExt {
    /// The RLM config this session recorded, as recorded.
    fn rlm_config(&self) -> lash_rlm_types::RlmSessionConfig;
}

#[cfg(feature = "rlm")]
impl RlmSessionReadViewExt for lash_core::SessionReadView {
    fn rlm_config(&self) -> lash_rlm_types::RlmSessionConfig {
        lash_protocol_rlm::rlm_session_config(self.protocol_turn_options()).unwrap_or_default()
    }
}

/// A guarded write that a session refused, or that could not reach the session.
#[cfg(feature = "rlm")]
#[derive(Debug)]
pub enum RlmSessionConfigError {
    /// The session already recorded a different value for that fact.
    Conflict(lash_rlm_types::RlmSessionConfigConflict),
    /// The write never got as far as the durable facts.
    Session(EmbedError),
}

#[cfg(feature = "rlm")]
impl std::fmt::Display for RlmSessionConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => write!(f, "{conflict}"),
            Self::Session(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(feature = "rlm")]
impl std::error::Error for RlmSessionConfigError {}

#[cfg(feature = "rlm")]
impl From<EmbedError> for RlmSessionConfigError {
    fn from(error: EmbedError) -> Self {
        Self::Session(error)
    }
}

/// The durable RLM facts of an opened session: a typed read and a guarded
/// set-if-unset write (ADR 0066).
///
/// There is no RLM-specific *request* API for any of these. A host that wants a
/// fact *asserted* compares [`RlmSessionExt::rlm_config`] against what it
/// requires and refuses loudly; a host that wants to *prefer* a value calls
/// [`RlmSessionExt::set_rlm_config_if_unset`] and lets the recorded value win.
/// Both are one line of host code, and neither can smooth a mismatch away by
/// accident.
///
/// The one fact this surface cannot *introduce* is the dialect: the protocol
/// plugin selects its dialect implementation when the session's plugins are
/// built, which is before any post-open write can reach it. A session states
/// its dialect through the plugin-agnostic
/// [`SessionBuilder::plugin_option`](crate::SessionBuilder::plugin_option) seam
/// keyed by [`RLM_PROTOCOL_PLUGIN_ID`] — the same seam `lash-subagents` writes a
/// parent's dialect forward through — and that statement is applied by the same
/// guarded set-if-unset engine, refusing with the same typed conflict. Calling
/// this method with the dialect the session already resolved is a no-op;
/// calling it with a different one is refused. Reading the recorded dialect
/// *before* opening is FIG-1556's preflight surface, not this one.
#[cfg(feature = "rlm")]
#[async_trait::async_trait]
pub trait RlmSessionExt {
    /// The RLM config this session recorded, as recorded.
    fn rlm_config(&self) -> lash_rlm_types::RlmSessionConfig;

    /// Write every fact the request states that the session has not recorded,
    /// and return the resulting config.
    ///
    /// Restating a fact the session already recorded is a no-op, so this is
    /// safe to call on every open. Stating a *different* value is refused with
    /// [`RlmSessionConfigError::Conflict`] — the write never lands and the
    /// session keeps what it recorded.
    ///
    /// A stated dialect is only ever *compared*, never written. An open session
    /// is already running one dialect implementation, and a session that
    /// recorded no dialect resolved the default one, so the comparison is
    /// against the dialect the session is running rather than against the
    /// recorded `Option` — writing a dialect here would leave the recorded fact
    /// disagreeing with the running plugin.
    ///
    /// The write follows the durability the pin has always had: it lands with
    /// the session's next commit.
    async fn set_rlm_config_if_unset(
        &self,
        requested: lash_rlm_types::RlmSessionConfig,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigError>;
}

#[cfg(feature = "rlm")]
#[async_trait::async_trait]
impl RlmSessionExt for crate::LashSession {
    fn rlm_config(&self) -> lash_rlm_types::RlmSessionConfig {
        self.read_view().rlm_config()
    }

    async fn set_rlm_config_if_unset(
        &self,
        requested: lash_rlm_types::RlmSessionConfig,
    ) -> std::result::Result<lash_rlm_types::RlmSessionConfig, RlmSessionConfigError> {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let recorded = lash_protocol_rlm::rlm_session_config(runtime.protocol_turn_options())
            .map_err(|err| {
                RlmSessionConfigError::Session(EmbedError::Session(SessionError::Protocol(
                    err.to_string(),
                )))
            })?;
        let resolved = lash_protocol_rlm::apply_rlm_session_config_post_open(&recorded, &requested)
            .map_err(RlmSessionConfigError::Conflict)?;
        if resolved != recorded {
            let options = lash_protocol_rlm::rlm_session_config_options(&resolved)
                .map_err(|err| RlmSessionConfigError::Session(EmbedError::Session(err)))?;
            runtime.set_protocol_turn_options(options);
            self.runtime.publish_from(&runtime);
        }
        Ok(resolved)
    }
}

// RLM-specific Lashlang host vocabulary. The catalogue-preview, tool-binding,
// and process-input names are single-homed under `lash::tools` and
// `lash::process`; they are not re-exported here.
pub use lash_lashlang_runtime::{
    LASHLANG_SURFACE_EXTENSION_ID, LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment,
    LashlangLanguageFeatures, LashlangProcessEngine, LashlangSurface, LashlangSurfaceContribution,
};
pub use lash_protocol_rlm::{
    ExecutionBounds, InstructionBound, MemoryBound, NamedDataType, RLM_PROTOCOL_PLUGIN_ID,
    RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory, TypeExpr,
    TypeField, UnsetBound, WallClockBound, format_type_expr,
};
/// Projection vocabulary: register lazy host projections on a
/// [`ProjectionRegistry`], bind projected values session-wide via
/// [`rlm_session_projection_extension`], or per turn via
/// [`RlmTurnInputExt::rlm_project`].
pub use lash_protocol_rlm::{
    ProjectionRegistry, RlmProjectedBindings, RlmTurnInputExt, rlm_session_projection_extension,
};
pub use lash_rlm_types::{
    RlmCreateExtras, RlmDialect, RlmFinalAnswerFormat, RlmSessionConfig, RlmSessionConfigConflict,
    RlmTermination,
};

/// The Lashlang compile APIs are operations over an
/// [`RlmProtocolPluginFactory`] and a plugin host; they live in
/// `lash-protocol-rlm` and are re-exported here.
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::{
    LashlangCompileSurface, LashlangCompileSurfaceRequest, LashlangModuleCompileError,
    LashlangModuleCompileRequest, ModuleCompileOutput,
};

#[cfg(feature = "rlm")]
fn rlm_termination(
    mut builder: TurnBuilder,
    termination: lash_rlm_types::RlmTermination,
) -> Result<TurnBuilder> {
    let override_options = ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
        dialect: None,
        termination: Some(termination),
        final_answer_format: None,
    })?;
    let options = builder
        .protocol_turn_options
        .as_ref()
        .map(|current| current.merged_with_override(&override_options))
        .unwrap_or(override_options);
    builder.protocol_turn_options = Some(options);
    Ok(builder)
}
