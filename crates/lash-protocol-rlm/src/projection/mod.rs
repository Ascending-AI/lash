mod bindings;
mod context;
mod transport;

pub use bindings::{
    ProjectionRef, ProjectionRegistry, ProjectionResolveError, ProjectionResolver,
    RlmProjectedBindings, RlmToolResultProjector, RlmTurnInputExt,
    rlm_session_projection_extension,
};
pub use context::{
    RlmHistoryProjection, decode_rlm_protocol_event, rlm_history_projection, rlm_protocol_event,
};
pub use transport::{RlmSeed, rlm_seed_initial_nodes};

pub(crate) use bindings::{RLM_TURN_INPUT_PLUGIN_ID, RlmProjectionExtension};
#[cfg(test)]
pub(crate) use context::projected_index;
pub(crate) use context::{
    projected_bindings, prune_projected_binding_names, prune_protected_bindings,
    prune_reserved_projected_bindings,
};
#[cfg(test)]
pub(crate) use transport::{flow_record_to_json_value, flow_record_to_tool_args};
pub(crate) use transport::{
    flow_to_json_value, format_output_value, json_to_flow_value,
    normalize_tool_args_for_projection, rehydrate_projected_globals,
};
