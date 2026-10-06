pub(crate) mod bindings;
mod context;
pub(crate) mod transcript;
mod transport;

pub use bindings::{RlmProjectedBindings, rlm_session_projection_extension};
pub use context::{
    RLM_PROTOCOL_EVENT_VERSION, RlmHistoryProjection, is_rlm_protocol_output,
    rlm_history_projection, rlm_protocol_event,
};
pub use transport::{RlmSeed, rlm_seed_initial_nodes};

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

pub use context::decode_rlm_protocol_event;

mod diagnostics;
pub use diagnostics::recorded_extraction_decisions;
