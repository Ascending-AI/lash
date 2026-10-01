//! RLM protocol plugin: a trajectory-shaped driver that uses lashlang as the
//! persistent REPL. Provider reasoning is stored as trajectory reasoning, the
//! host-selected [`Dialect`]'s cells are executed, printed values yield
//! observations, and the dialect's finish form yields the final value.

mod cell_scan;
mod control_tools;
mod dialect;
mod driver;
mod executor;
mod feedback;
mod native;
mod plugin;
pub use native::{
    NATIVE_DRIVER_STATE_VERSION, NATIVE_EXECUTE_TOOL_NAME, NATIVE_TRANSPORT_VERSION,
    RlmNativeToolPlugin,
};
mod projection;
mod protocol;
mod system_prompt;
pub use system_prompt::{RLM_BUILTIN_INTRO, RlmSystemPromptInput, render_rlm_system_prompt};
pub mod render;
pub use render::{BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ResolvedRlmRender};
mod rlm_support;
pub mod scenario_contracts;
mod stream_mask;
#[cfg(test)]
mod testing;
mod tool_catalog;

pub use control_tools::continue_as_tool_definition;
pub use dialect::{
    CellTags, Dialect, DialectPromptVocabulary, DialectRefusal, DialectRefusalKind,
    ExecutionSection, ExecutionSectionRequest, ShapeNotation, TypescriptDialect,
};
pub use driver::{RlmProjectorConfig, build_rlm_preamble};
pub use executor::RLM_SNAPSHOT_VERSION;
#[cfg(feature = "testing")]
pub use executor::RlmCheckpointPerfFixture;
pub use lash_lashlang_runtime::ResolvedToolBinding;
pub use lash_lashlang_runtime::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
};
/// The schema shapes a [`Dialect`] spells: the contract layer's reading of a
/// tool's JSON Schemas.
pub use lash_sansio::{
    ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaShape, ShapeConstraints,
    ShapeField, ShapeKind, ShapeRow,
};
pub use lashlang::{NamedDataType, TypeExpr, TypeField, format_type_expr};
pub use plugin::{
    ExecutionBounds, InstructionBound, LashlangCompileSurface, LashlangCompileSurfaceRequest,
    LashlangModuleCompileError, LashlangModuleCompileRequest, MemoryBound, ModuleCompileOutput,
    RLM_CONFIG_IMPLEMENTATION, RLM_PROTOCOL_PLUGIN_ID, RlmAbilities, RlmChannel, RlmConfigOwner,
    RlmConfigRefusal, RlmCreateConfig, RlmLanguageFeatures, RlmProtocolPluginConfig,
    RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory, RlmRecordedBehaviour,
    RlmRecordedConfig, RlmSessionConfigDecodeError, SetRlmPrompt, SetRlmPromptContext,
    SetRlmRender, UnsetBound, UnsetChannel, rlm_lashlang_surface, rlm_protocol_config,
    rlm_session_config,
};
pub use projection::{
    RlmHistoryProjection, RlmSeed, decode_rlm_protocol_event, rlm_history_projection,
    rlm_protocol_event, rlm_seed_initial_nodes,
};
pub use projection::{RlmProjectedBindings, rlm_session_projection_extension};
#[cfg(feature = "testing")]
pub use protocol::project_conformance_messages_through_rlm_history;
pub use protocol::{RlmDriver, RlmPromptFeatures};
pub use rlm_support::format_budget_suffix;

#[cfg(test)]
mod prompt_contract_tests;
