mod bindings;
mod context;
mod transport;

pub use bindings::{RlmProjectedBindings, rlm_session_projection_extension};
pub use context::{
    RlmHistoryProjection, decode_rlm_protocol_event, is_rlm_protocol_output,
    rlm_history_projection, rlm_protocol_event,
};
pub use transport::{RlmSeed, rlm_seed_initial_nodes};

pub(crate) use bindings::RlmProjectionExtension;
pub(crate) use bindings::{READ_ONLY_VARIABLES_TITLE, read_only_variables_prompt};
pub(crate) use context::projected_bindings;
#[cfg(test)]
pub(crate) use context::projected_index;
#[cfg(test)]
pub(crate) use transport::flow_record_to_tool_args;
pub(crate) use transport::{
    flow_to_json_value, json_to_flow_value, normalize_tool_args_for_projection,
};

#[cfg(test)]
pub(crate) use context::prune_reserved_projected_bindings;
