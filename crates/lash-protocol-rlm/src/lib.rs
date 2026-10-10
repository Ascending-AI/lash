//! RLM protocol plugin: a trajectory-shaped driver that uses lash_vm as the
//! persistent REPL. Provider reasoning is stored as trajectory reasoning, the
//! host-selected [`Dialect`]'s cells are executed, printed values yield
//! observations, and the dialect's finish form yields the final value.

mod catalogue_preview;
mod cell_scan;
mod cell_value;
mod control_tools;
mod deferred;
mod dialect;
mod driver;
mod driver_state;
pub use driver_state::RLM_DRIVER_STATE_VERSION;
mod executor;
mod feedback;
mod native;
mod plugin;
pub use native::{NATIVE_EXECUTE_TOOL_NAME, RlmNativeToolPlugin};
mod projection;
mod prompt_sections;
mod protocol;
pub use prompt_sections::{section_id, section_keys};
pub mod render;
pub use render::{BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ResolvedRlmRender};
mod rlm_support;
pub mod scenario_contracts;
mod stream_mask;
#[cfg(test)]
mod testing;
mod tool_catalog;

pub use catalogue_preview::{
    CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
    DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, catalogue_preview,
    catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
    catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
};
pub use control_tools::{FINISH_TOOL_NAME, continue_as_tool_definition, finish_tool_definition};
pub use deferred::{
    DeferredResolutionError, DeferredResolveContext, DeferredToolResolver,
    RecordedGrantInstallError, Resolution, SharedDeferredToolResolver, ToolGrant,
};
pub use dialect::{
    CellDialect, CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal,
    DialectRefusalKind, ExecutionSection, ExecutionSectionRequest, PythonPrompts,
    TypescriptPrompts,
};
pub use driver::{RlmProjectorConfig, build_rlm_preamble};
#[cfg(feature = "testing")]
pub use executor::CodeModeCheckpointPerfFixture;
pub use executor::{
    CODEMODE_SNAPSHOT_VERSION, CONTROL_REFUSED, CodeModeSnapshotError, TOOL_ARGUMENTS,
    TOOL_CALL_LIMIT, TOOL_FAILED, UNKNOWN_EFFECT, cell_migration_refusal, cell_snapshot_functions,
    saved_function_pins,
};
/// The kernel's typed snapshot validation causes and fragment roots.
pub mod snapshot {
    pub use lash_kernel_state::{LoadError, Root, SaveError};
}
pub use feedback::{
    CELL_BOUND_EXCEEDED, CELL_DEADLOCK, CELL_TASKS_OUTSTANDING, SESSION_BINDING_NOT_CARRIED,
};
/// The schema shapes a [`DialectPrompts`] spells: the contract layer's reading of a
/// tool's JSON Schemas, also constructed directly by runtime-value inference.
pub use lash_sansio::{
    ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaShape, ShapeConstraints,
    ShapeField, ShapeKind, ShapeRow,
};
pub use lash_vm_runtime::ResolvedToolBinding;
pub use plugin::{
    ExecutionBounds, HelperReleaseGate, InstructionBound, MemoryBound, RLM_PROTOCOL_PLUGIN_ID,
    RlmChannel, RlmConfigOwner, RlmConfigRefusal, RlmCreateConfig, RlmPresentationConfig,
    RlmProtocolPluginConfig, RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory,
    RlmRecordedBehaviour, RlmRecordedConfig, RlmRenderRefusal, RlmRunOptions,
    RlmSessionConfigDecodeError, SetRlmRender, UnsetBound, UnsetChannel, rlm_session_config,
};
pub use projection::{
    CodeModeProjectedBindings, ProjectedBindingError, rlm_session_projection_extension,
};
pub use projection::{
    HISTORY_PROJECTION, RLM_PROTOCOL_EVENT_VERSION, RlmHistoryProjection, RlmSeed,
    is_rlm_protocol_output, rlm_history_projection, rlm_protocol_event, rlm_seed_initial_nodes,
};
// Harnesses read recorded RLM events through the protocol's own decoder; the
// `lash::rlm` facade keeps decoding sealed (FIG-1530).
#[cfg(feature = "testing")]
pub use projection::decode_rlm_protocol_event;
#[cfg(feature = "testing")]
pub use protocol::project_conformance_messages_through_rlm_history;
pub use protocol::{RlmDriver, RlmPromptFeatures};
pub use rlm_support::format_budget_suffix;

#[cfg(test)]
mod prompt_contract_tests;

pub use projection::recorded_extraction_decisions;

mod tool_records;
