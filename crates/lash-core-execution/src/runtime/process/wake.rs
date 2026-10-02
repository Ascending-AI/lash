use crate::plugin::PluginError;
pub use lash_core_store::process_identity::{process_wake_turn_cause, process_wake_turn_text};

use super::events::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessWake, ProcessWakeDelivery};
use super::model::{ProcessId, SessionId};

/// Extracts the model-facing wake input from a process wake event payload.
pub fn process_wake_input_from_event_payload(payload: &serde_json::Value) -> String {
    payload
        .pointer("/text")
        .or_else(|| payload.pointer("/value"))
        .map(lash_core_store::process_identity::wake_payload_value_to_string)
        .unwrap_or_else(|| payload.to_string())
}

#[derive(Clone, Debug)]
pub struct ProcessWakeDeliveryRequest {
    pub target_session_id: SessionId,
    pub process_id: ProcessId,
    pub sequence: u64,
    pub event_type: String,
    pub process_caused_by: Option<crate::CausalRef>,
    pub authority: crate::QueuedWorkAuthority,
    pub wake: ProcessWake,
    /// What caused the wake ([`ProcessWakeDelivery::trace_cause`]).
    pub trace_cause: lash_trace::TraceCause,
    pub occurred_at_ms: u64,
    /// `F` the persisting store recorded: the delivery row's `version` stamps
    /// through `writer_version`, never the bare build constant (FIG-3796).
    pub fleet_format: crate::FleetFormat,
}

pub fn process_wake_delivery(
    request: ProcessWakeDeliveryRequest,
) -> Result<ProcessWakeDelivery, PluginError> {
    let ProcessWakeDeliveryRequest {
        target_session_id,
        process_id,
        sequence,
        event_type,
        process_caused_by,
        authority,
        wake,
        trace_cause,
        occurred_at_ms,
        fleet_format,
    } = request;
    Ok(ProcessWakeDelivery {
        version: fleet_format.writer_version(lash_core_store::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        target_session_id,
        process_id,
        sequence,
        event_type,
        process_caused_by,
        authority,
        input: wake.input,
        created_at_ms: occurred_at_ms,
        trace_cause,
    })
}
