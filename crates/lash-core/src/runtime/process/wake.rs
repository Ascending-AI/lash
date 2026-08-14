use std::time::SystemTime;

use crate::plugin::PluginError;

use super::events::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessWake, ProcessWakeDelivery};
use super::model::{ProcessId, SessionId};
use super::time::epoch_ms_from_system_time;

const PROCESS_WAKE_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for process-wake identities.
///
/// Version 1 has no sum variants: its complete grammar is target session,
/// process id, then event sequence. Retired tags remain burned when variants
/// are introduced in a later family version.
fn process_wake_identity_preimage(
    target_session_id: &str,
    process_id: &str,
    sequence: u64,
) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.process-wake",
        PROCESS_WAKE_FAMILY_VERSION,
    );
    identity.string(target_session_id);
    identity.string(process_id);
    identity.u64(sequence);
    identity.finish()
}

fn process_wake_id(target_session_id: &str, process_id: &str, sequence: u64) -> String {
    crate::stable_identity::rendered_hash(
        "wake",
        PROCESS_WAKE_FAMILY_VERSION,
        &process_wake_identity_preimage(target_session_id, process_id, sequence),
    )
}

pub(super) fn is_process_wake_id(value: &str) -> bool {
    value
        .strip_prefix("wake:v")
        .and_then(|value| value.split_once(":sha256:"))
        .is_some_and(|(version, digest)| {
            !version.is_empty()
                && version.bytes().all(|byte| byte.is_ascii_digit())
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

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
    pub authority: crate::QueuedWorkAuthority,
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
        authority,
        wake,
        occurred_at,
    } = request;
    let wake_id = process_wake_id(target_session_id.as_str(), process_id.as_str(), sequence);
    Ok(ProcessWakeDelivery {
        version: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id,
        target_session_id,
        process_id,
        sequence,
        event_type,
        event_invocation,
        process_caused_by,
        authority,
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

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn process_wake_v1_identity_golden() {
        let preimage = process_wake_identity_preimage("session\0x", "process:λ", 42);
        assert_eq!(
            hex(&preimage),
            "6c6173682d737461626c652d6964656e74697479010100000000000000116c6173682e70726f636573732d77616b65000000000000000973657373696f6e0078000000000000000a70726f636573733acebb000000000000002a"
        );
        assert_eq!(
            process_wake_id("session\0x", "process:λ", 42),
            "wake:v1:sha256:d0ffae31aa4049177a4803af2cd3609c766e5d95e841239523f6e942619fac87"
        );
    }
}
