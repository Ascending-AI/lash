pub(crate) mod context;
pub(crate) mod transcript;
mod transport;

pub use context::{RLM_PROTOCOL_EVENT_VERSION, is_rlm_protocol_output, rlm_protocol_event};
pub use transport::{RlmSeed, rlm_seed_initial_nodes};

pub use context::decode_rlm_protocol_event;

mod diagnostics;
pub use diagnostics::recorded_extraction_decisions;

#[cfg(test)]
pub(crate) use context::EVENT_DECODES;

mod event_decoder;
pub(crate) use event_decoder::EventDecoder;
