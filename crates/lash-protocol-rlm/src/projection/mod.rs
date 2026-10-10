pub(crate) mod bindings;
mod context;
pub(crate) mod transcript;
mod transport;

pub use bindings::{
    CodeModeProjectedBindings, ProjectedBindingError, rlm_session_projection_extension,
};
pub use context::HISTORY_PROJECTION;
pub use context::{
    RLM_PROTOCOL_EVENT_VERSION, RlmHistoryProjection, is_rlm_protocol_output,
    rlm_history_projection, rlm_protocol_event,
};
pub use transport::{RlmSeed, rlm_seed_initial_nodes};

pub(crate) use bindings::{READ_ONLY_VARIABLES_TITLE, read_only_variables_prompt};
pub(crate) use context::{HostBindingsError, cell_host_bindings};
pub(crate) use transport::{normalize_tool_args_for_projection, plain_json_for_transport};

pub use context::decode_rlm_protocol_event;

mod diagnostics;
pub use diagnostics::recorded_extraction_decisions;
