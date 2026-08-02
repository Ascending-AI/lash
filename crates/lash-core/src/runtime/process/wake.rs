use std::time::SystemTime;

use crate::plugin::PluginError;

use super::events::{ProcessWake, ProcessWakeDelivery};
use super::model::{ProcessId, SessionId};
use super::time::epoch_ms_from_system_time;

/// Extracts the model-facing wake input from a process wake event payload.
pub fn process_wake_input_from_event_payload(payload: &serde_json::Value) -> String {
    payload
        .pointer("/text")
        .or_else(|| payload.pointer("/value"))
        .map(wake_payload_value_to_string)
        .unwrap_or_else(|| payload.to_string())
}

/// Renders a durable process wake as model-visible chronological context.
pub fn process_wake_turn_text(wake: &ProcessWakeDelivery) -> String {
    // Sender-floor allocation keeps sequences small ordered identifiers, so
    // the model-facing `#<sequence>` remains a useful event label.
    format!(
        "Background process wake\nProcess: {}\nEvent: {} #{}\nWake input:\n{}",
        wake.process_id, wake.event_type, wake.sequence, wake.input
    )
}

pub fn process_wake_turn_cause(wake: &ProcessWakeDelivery) -> crate::TurnCause {
    crate::TurnCause {
        id: wake.wake_id.clone(),
        event_type: wake.event_type.clone(),
        origin: crate::MessageOrigin::Process {
            process_id: wake.process_id.clone(),
            event_type: wake.event_type.clone(),
            sequence: wake.sequence,
            wake_id: Some(wake.wake_id.clone()),
            caused_by: wake.process_caused_by.clone(),
        },
        text: process_wake_turn_text(wake),
    }
}

#[derive(Clone, Debug)]
pub struct ProcessWakeDeliveryRequest {
    pub target_session_id: SessionId,
    pub process_id: ProcessId,
    pub sequence: u64,
    pub event_type: String,
    pub event_invocation: crate::RuntimeInvocation,
    pub process_caused_by: Option<crate::CausalRef>,
    pub wake: ProcessWake,
    pub occurred_at: SystemTime,
}

pub fn process_wake_delivery(
    request: ProcessWakeDeliveryRequest,
) -> Result<ProcessWakeDelivery, PluginError> {
    let ProcessWakeDeliveryRequest {
        target_session_id,
        process_id,
        sequence,
        event_type,
        event_invocation,
        process_caused_by,
        wake,
        occurred_at,
    } = request;
    let wake_id = crate::stable_hash::stable_json_sha256_hex(&(
        target_session_id.as_str(),
        process_id.as_str(),
        sequence,
    ))
    .map_err(|err| {
        PluginError::Session(format!(
            "failed to hash wake delivery for process `{process_id}`: {err}"
        ))
    })?;
    Ok(ProcessWakeDelivery {
        wake_id: format!("wake:{wake_id}"),
        target_session_id,
        process_id,
        sequence,
        event_type,
        event_invocation,
        process_caused_by,
        input: wake.input,
        created_at_ms: epoch_ms_from_system_time(occurred_at),
    })
}

fn wake_payload_value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}
