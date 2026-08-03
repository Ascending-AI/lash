use crate::support::*;
use lash_core::facade_support::ProtocolTurnOptionsFacadeOps;

#[cfg(feature = "rlm")]
pub trait RlmTurnBuilderExt: Sized {
    fn require_finish(self) -> Result<Self>;
    fn require_finish_schema(self, schema: serde_json::Value) -> Result<Self>;
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

/// Sugar for setting the RLM final-answer format on a plain [`SessionBuilder`].
///
/// This writes `RlmCreateExtras` into the builder's generic open-time plugin
/// options bag (the same seam every plugin uses), merging with any RLM options
/// already present so it never clobbers a termination the host set through the
/// same key.
#[cfg(feature = "rlm")]
pub trait RlmSessionBuilderExt: Sized {
    fn final_answer_format(self, format: lash_rlm_types::RlmFinalAnswerFormat) -> Result<Self>;
}

#[cfg(feature = "rlm")]
impl RlmSessionBuilderExt for SessionBuilder {
    fn final_answer_format(mut self, format: lash_rlm_types::RlmFinalAnswerFormat) -> Result<Self> {
        let mut extras = self
            .plugin_options
            .decode::<lash_rlm_types::RlmCreateExtras>(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
            .ok()
            .flatten()
            .unwrap_or_default();
        extras.final_answer_format = Some(format);
        self = self.plugin_option(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID, extras)?;
        Ok(self)
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
    NamedDataType, RlmProtocolPluginConfig, RlmProtocolPluginFactory, TypeExpr, TypeField,
    format_type_expr,
};
/// Projection vocabulary: register lazy host projections on a
/// [`ProjectionRegistry`], bind projected values session-wide via
/// [`rlm_session_projection_extension`], or per turn via
/// [`RlmTurnInputExt::rlm_project`].
pub use lash_protocol_rlm::{
    ProjectionRegistry, RlmProjectedBindings, RlmTurnInputExt, rlm_session_projection_extension,
};
pub use lash_rlm_types::RlmFinalAnswerFormat;

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
        termination,
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
