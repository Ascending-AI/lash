//! RLM protocol plugin: a trajectory-shaped driver that uses lashlang as the
//! persistent REPL. Provider reasoning is stored as trajectory reasoning,
//! paired `<lashlang>` blocks are executed, `print` yields observations, and
//! `finish` yields the final value.

mod cell_scan;
mod control_tools;
mod dialect;
mod driver;
mod executor;
mod feedback;
mod plugin;
mod projection;
mod protocol;
mod public_error;
mod rlm_support;
pub mod scenario_contracts;
mod stream_mask;
mod tool_catalog;

pub use control_tools::continue_as_tool_definition;
pub use driver::{RlmProjectorConfig, build_rlm_preamble};
#[cfg(feature = "testing")]
pub use executor::RlmCheckpointPerfFixture;
pub use lash_lashlang_runtime::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
};
pub use lashlang::{NamedDataType, TypeExpr, TypeField, format_type_expr};
pub use plugin::{
    ExecutionBound, ExecutionBounds, LashlangCompileSurface, LashlangCompileSurfaceRequest,
    LashlangModuleCompileError, LashlangModuleCompileRequest, ModuleCompileOutput,
    RLM_PROTOCOL_PLUGIN_ID, RlmAbilities, RlmLanguageFeatures, RlmProtocolPluginConfig,
    RlmProtocolPluginFactory, RlmSessionConfigDecodeError, apply_rlm_session_config_if_unset,
    apply_rlm_session_config_post_open, rlm_lashlang_surface, rlm_protocol_config,
    rlm_session_config, rlm_session_config_options,
};
pub use projection::{
    ProjectionRef, ProjectionRegistry, ProjectionResolveError, ProjectionResolver,
    RlmProjectedBindings, RlmProjectedSeedError, RlmToolResultProjector, RlmTurnInputExt,
    rlm_session_projection_extension,
};
pub use projection::{
    RlmHistoryProjection, RlmSeed, decode_rlm_protocol_event, rlm_history_projection,
    rlm_protocol_event, rlm_seed_initial_nodes,
};
#[cfg(feature = "testing")]
pub use protocol::project_conformance_messages_through_rlm_history;
pub use protocol::{
    RlmDriver, RlmPromptFeatures, contains_lashlang_cell,
    rlm_execution_section_for_host_environment,
};
pub use rlm_support::format_budget_suffix;

#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn capture_scratch_files_for_testing(
    files: Vec<(String, Vec<u8>)>,
) -> Result<lash_core::plugin::HydratedExecutionState, lash_core::SessionError> {
    executor::capture_scratch_files_for_testing(files)
}
