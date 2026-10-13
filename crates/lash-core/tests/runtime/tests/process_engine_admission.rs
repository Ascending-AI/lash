use super::*;

#[path = "../../runtime_support/payload_gated_engine.rs"]
mod payload_gated_engine;
use payload_gated_engine::{PAYLOAD_GATED_ENGINE_KIND, PayloadGatedEngine};

include!("../../runtime_support/process_start_admission.rs");
