//! RLM protocol plugin: a trajectory-shaped driver that uses lash_vm as the
//! persistent REPL. Provider reasoning is stored as trajectory reasoning, the
//! host-selected [`Dialect`]'s cells are executed, printed values yield
//! observations, and the dialect's finish form yields the final value.

#![deny(clippy::future_not_send)]

mod call_feedback;
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
/// A cell host's checkpoint graph nodes (FIG-5887): the call summary, events,
/// host root, envelope and end. The integrator wires them into the cell's
/// park and end commits.
#[doc(hidden)]
pub mod host_graph {
    pub use crate::executor::envelope::{CellEnvelope, EffectTable, RecordedEffect};
    pub use crate::executor::host_graph::*;
}
mod feedback;
mod native;
mod plugin;
pub use native::NATIVE_EXECUTE_TOOL_NAME;
mod projection;
mod prompt_sections;
mod protocol;
pub use prompt_sections::{RlmPromptFacts, section_id, section_keys};
pub mod render;
pub use render::{BuiltinCodeRenderer, CodeRenderer, CodeRendererSlot, ResolvedRlmRender};
mod rlm_support;
pub mod scenario_contracts;
mod session_globals;
mod settlement;
pub use session_globals::{
    GlobalsPage, GlobalsPageView, SessionGlobal, SessionGlobalKind, SessionGlobals,
    StaleGlobalsPage,
};
mod stream_mask;
#[cfg(test)]
mod testing;
mod tool_catalog;
pub use tool_catalog::{
    ToolCatalogueDisclosure, discovered_tool_doc, discovered_tool_signature, discovery_text,
    loaded_tool_signature, tool_catalogue_disclosure,
};

pub use catalogue_preview::{
    CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
    DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, catalogue_preview,
    catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
    catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
};
pub use control_tools::{
    FINISH_TOOL_NAME, continue_as_tool_definition, finish_tool_definition,
    read_output_tool_definition,
};
pub use deferred::DeferredResolutionError;
pub use dialect::{
    CellDialect, CellTags, DialectPromptVocabulary, DialectPrompts, DialectRefusal,
    DialectRefusalKind, ExecutionSection, ExecutionSectionRequest, PythonPrompts,
    TypescriptPrompts,
};
pub use driver::{RlmProjectorConfig, build_rlm_preamble};
pub use executor::{
    CODEMODE_SNAPSHOT_VERSION, CONTROL_REFUSED, SessionOpenError, TOOL_ARGUMENTS, TOOL_CALL_LIMIT,
    TOOL_FAILED, UNKNOWN_EFFECT, cell_migration_refusal, cell_snapshot_functions,
    saved_function_pins,
};
#[cfg(feature = "testing")]
pub use executor::{CodeModeCheckpointPerfFixture, CodeModeCheckpointPerfPublisher};
/// The kernel's typed snapshot validation causes.
pub mod snapshot {
    pub use lash_core_store::checkpoint_graph::{
        GraphRefusal, KernelRestoreRefusal, NodeId, RootSource, roots::RootField,
    };
    pub use lash_kernel_doc::heap::HeapFault;
}
pub use call_feedback::{CallFeedbackPolicy, CallFeedbackPolicyError};
pub use feedback::{
    CELL_BOUND_EXCEEDED, CELL_DEADLOCK, CELL_TASKS_OUTSTANDING, SESSION_BINDING_NOT_CARRIED,
};
/// The schema shapes a [`DialectPrompts`] and a tool presenter spell: the
/// shared reading of a tool's JSON Schemas, also constructed directly by
/// runtime-value inference.
pub use lash_tool_shape::{
    ExtraKeys, ObjectShape, ProcessParamShape, ProcessShape, SchemaShape, ShapeConstraints,
    ShapeField, ShapeKind, ShapeRow,
};
/// The tool presenters a [`CellDialect`] ships with, and their building
/// blocks for a host presenter that spells the same way.
pub mod tool_presenters {
    pub use lash_tool_presenter_python as python;
    pub use lash_tool_presenter_typescript as typescript;
}
pub use lash_vm_runtime::ResolvedToolBinding;
pub use plugin::{
    Charge, ExecutionBounds, HelperReleaseGate, InstructionBound, MemoryBound,
    RLM_PROTOCOL_PLUGIN_ID, RLM_RUN_OPTIONS, RlmChannel, RlmConfigOwner, RlmConfigRefusal,
    RlmCreateConfig, RlmDiscovery, RlmPresentationConfig, RlmProtocolPluginConfig,
    RlmProtocolPluginConfigBuilder, RlmProtocolPluginFactory, RlmRecordedBehaviour,
    RlmRecordedConfig, RlmRenderRefusal, RlmRunOptions, RlmSessionConfigDecodeError, SetRlmRender,
    UnsetBound, UnsetChannel, rlm_session_config,
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

fn head_tail_truncate(value: &str, max_chars: usize) -> (String, usize) {
    let raw_len = value.chars().count();
    if max_chars == 0 || raw_len <= max_chars {
        return (value.to_string(), raw_len);
    }
    let head_len = max_chars / 2;
    let tail_len = max_chars.saturating_sub(head_len);
    let head = value.chars().take(head_len).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    let omitted = raw_len.saturating_sub(head_len + tail_len);
    (
        format!("{head}\n\n... ({omitted} characters omitted) ...\n\n{tail}"),
        raw_len,
    )
}
