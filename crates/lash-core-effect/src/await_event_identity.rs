//! Stable AwaitEvent key identity.
//!
//! These bytes are shared by effect hosts. The family version and golden
//! preimages are preserved across the engine cutover.

use super::{AwaitEventWaitIdentity, ExecutionScope};
use crate::{RuntimeError, RuntimeErrorCode};

const AWAIT_EVENT_FAMILY_VERSION: u8 = 3;

/// Permanent tag registry for await-event promise identities.
///
/// Execution scopes: 1 turn, 2 process, 3 queue drain, 4 session delete, 5 runtime operation.
/// Retired tags remain burned.
fn promise_key_preimage(scope: &ExecutionScope, wait: &AwaitEventWaitIdentity) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.await-event",
        AWAIT_EVENT_FAMILY_VERSION,
    );
    match scope {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => {
            identity.tag(1);
            identity.string(session_id);
            identity.string(turn_id);
        }
        ExecutionScope::Process { process_id } => {
            identity.tag(2);
            identity.string(process_id);
        }
        ExecutionScope::QueueDrain {
            session_id,
            drain_id,
        } => {
            identity.tag(3);
            identity.string(session_id);
            identity.string(drain_id);
        }
        ExecutionScope::SessionDelete { session_id } => {
            identity.tag(4);
            identity.string(session_id);
        }
        ExecutionScope::RuntimeOperation { operation_id } => {
            identity.tag(5);
            identity.string(operation_id);
        }
    }
    match wait {
        AwaitEventWaitIdentity::ToolCompletion { tool_call_id } => {
            identity.tag(1);
            identity.string(tool_call_id.as_str());
        }
        AwaitEventWaitIdentity::ProcessSignal {
            process_id,
            signal_name,
            ordinal,
        } => {
            identity.tag(2);
            identity.string(process_id);
            identity.string(signal_name);
            identity.u64(*ordinal);
        }
        AwaitEventWaitIdentity::TurnCancelGate => identity.tag(3),
        AwaitEventWaitIdentity::TurnTerminal => identity.tag(4),
        AwaitEventWaitIdentity::Custom { key } => {
            identity.tag(5);
            identity.string(key);
        }
        AwaitEventWaitIdentity::TurnCancelEscalation => identity.tag(6),
    }
    identity.finish()
}

pub fn derive_key_id(
    scope: &ExecutionScope,
    wait: &AwaitEventWaitIdentity,
) -> Result<String, RuntimeError> {
    scope.validate()?;
    wait.validate()?;
    let preimage = promise_key_preimage(scope, wait);
    Ok(crate::stable_identity::rendered_hash(
        "await-event",
        AWAIT_EVENT_FAMILY_VERSION,
        &preimage,
    ))
}

/// The typed refusal a completion delivered after its owning group child's
/// cancel decision earns (ADR 0099 §4, W17): the same
/// `RuntimeEffectGroupChildCancelDecided` a late final record earns, because
/// both are completions reaching a child whose cancel disposition already won
/// the linearization point.
pub fn cancel_decided_refusal() -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
        "the group child that owns this completion key is cancel-decided; ADR 0099 §4 \
         refuses a completion delivered after its cancel decision, and nothing was written",
    )
}

/// Compare authentication bytes without branching on their contents.
///
/// Length is folded into the result and the loop covers the longer input, so
/// malformed signatures use the same comparison shape as valid-length ones.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn await_event_identity_golden_corpus() {
        let vectors = [
            (
                ExecutionScope::turn("ab", "c"),
                AwaitEventWaitIdentity::tool_completion(lash_sansio::ToolCallId::fixture("x:y")),
            ),
            (
                ExecutionScope::turn("a", "bc"),
                AwaitEventWaitIdentity::tool_completion(lash_sansio::ToolCallId::fixture("x:y")),
            ),
            (
                ExecutionScope::process(lash_sansio::ProcessId::fixture("zero")),
                AwaitEventWaitIdentity::process_signal(
                    lash_sansio::ProcessId::fixture("zero"),
                    "ready",
                    1,
                ),
            ),
            (
                ExecutionScope::queue_drain("same", "same"),
                AwaitEventWaitIdentity::TurnCancelGate,
            ),
            (
                ExecutionScope::session_delete("same"),
                AwaitEventWaitIdentity::TurnTerminal,
            ),
            (
                ExecutionScope::runtime_operation("op:0"),
                AwaitEventWaitIdentity::Custom {
                    key: "0".to_string(),
                },
            ),
        ];
        let actual = vectors
            .iter()
            .map(|(scope, wait)| {
                (
                    hex(&promise_key_preimage(scope, wait)),
                    derive_key_id(scope, wait).expect("derive golden key"),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e74010000000000000002616200000000000000016301000000000000004374635f61303964393132656639303638393066323434643833346665383639656362353666633061363833393930623037343038343535346634663463646634366638",
                "await-event:v3:blake3:5eb5501356a7018068f6f8ba6cb1b4f510e44a71701a462e36d0e6675c1e6285",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e74010000000000000001610000000000000002626301000000000000004374635f61303964393132656639303638393066323434643833346665383639656362353666633061363833393930623037343038343535346634663463646634366638",
                "await-event:v3:blake3:4908b563d81a7e63b03556a1c39024946a0e54e48b4a69f38721300902890a68",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e74020000000000000022705f3639633135363366643637353732373762383036653938663666376262663762020000000000000022705f3639633135363366643637353732373762383036653938663666376262663762000000000000000572656164790000000000000001",
                "await-event:v3:blake3:0087a0f8ff0e6e7ab2432ca14a6ca3cf0f1e750cc13d8e86dd31d06a879c0af2",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e7403000000000000000473616d65000000000000000473616d6503",
                "await-event:v3:blake3:c3ed7c11a0887edd943270a758f5c690531389ade0cc3be73a6fb10c6dee7c4a",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e7404000000000000000473616d6504",
                "await-event:v3:blake3:c32e554a1bd97ab0e0a3971df7d97017bd643bdb16aba613e829ebfe3970919a",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020300000000000000106c6173682e61776169742d6576656e740500000000000000046f703a3005000000000000000130",
                "await-event:v3:blake3:a37c2870bbf5a1f30607f40ed75e1162f8f9cfc1a29d5adca9a4d75f7fec7ffe",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }
    }

    #[test]
    fn authentication_comparison_covers_content_and_length_mismatches() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sale"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    #[test]
    fn key_derivation_is_the_stable_public_hash() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait =
            AwaitEventWaitIdentity::tool_completion(lash_sansio::ToolCallId::fixture("call"));

        assert_eq!(
            derive_key_id(&scope, &wait).expect("derive key id"),
            "await-event:v3:blake3:682389333e0eb5acb2f3b43cf7d51cf2ab82f559e2a684281b31146107b1057e"
        );
    }

    #[test]
    fn promise_key_family_version_is_explicit_and_stable() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait =
            AwaitEventWaitIdentity::tool_completion(lash_sansio::ToolCallId::fixture("call"));

        assert_eq!(
            derive_key_id(&scope, &wait).expect("derive versioned key"),
            "await-event:v3:blake3:682389333e0eb5acb2f3b43cf7d51cf2ab82f559e2a684281b31146107b1057e"
        );
    }
}
