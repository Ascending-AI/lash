use crate::plugin::PluginError;
pub use lash_core_store::process_identity::{process_wake_turn_cause, process_wake_turn_text};

use super::events::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, ProcessWake, ProcessWakeDelivery};
use super::model::{ProcessId, SessionId};

const PROCESS_WAKE_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for process-wake identities.
///
/// Version 1 has no sum variants: its complete grammar is target session,
/// process id, then event sequence. Retired tags remain burned when variants
/// are introduced in a later family version.
fn process_wake_identity_preimage(
    target_session_id: &SessionId,
    process_id: &ProcessId,
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

fn process_wake_id(target_session_id: &SessionId, process_id: &ProcessId, sequence: u64) -> String {
    crate::stable_identity::rendered_hash(
        "wake",
        PROCESS_WAKE_FAMILY_VERSION,
        &process_wake_identity_preimage(target_session_id, process_id, sequence),
    )
}

pub(super) fn is_process_wake_id(value: &str) -> bool {
    value
        .strip_prefix("wake:v")
        .and_then(|value| value.split_once(":blake3:"))
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
        .map(lash_core_store::process_identity::wake_payload_value_to_string)
        .unwrap_or_else(|| payload.to_string())
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
    pub occurred_at_ms: u64,
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
        occurred_at_ms,
    } = request;
    let wake_id = process_wake_id(&target_session_id, &process_id, sequence);
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
        created_at_ms: occurred_at_ms,
    })
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn process_wake_v1_identity_golden() {
        let preimage = process_wake_identity_preimage(
            &SessionId::from("session\0x"),
            &crate::process_id_for_test("process:λ"),
            42,
        );
        assert_eq!(
            hex(&preimage),
            "6c6173682d737461626c652d6964656e74697479020100000000000000116c6173682e70726f636573732d77616b65000000000000000973657373696f6e00780000000000000022705f3732383666323863306130393737653138633335666635643130373538363633000000000000002a"
        );
        assert_eq!(
            process_wake_id(
                &SessionId::from("session\0x"),
                &crate::process_id_for_test("process:λ"),
                42
            ),
            "wake:v1:blake3:7fe5d63df8c2f43b4c31274fc065b7dd306e42e69abfdb821b9bc73915dac87a"
        );
    }
}
