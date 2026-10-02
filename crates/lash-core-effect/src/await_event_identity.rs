//! Stable AwaitEvent key identity.
//!
//! These bytes are shared by effect hosts. The family version and golden
//! preimages are preserved across the engine cutover.

use super::{AwaitEventWaitIdentity, ExecutionScope};
use crate::{RuntimeError, RuntimeErrorCode};

/// version_guard(
///     items(promise_key_preimage),
/// )
const AWAIT_EVENT_FAMILY_VERSION: u8 = 1;

/// Permanent tag registry for await-event promise identities.
///
/// Execution scopes: 1 turn, 2 process, 3 session operation, 4 session delete,
/// 5 runtime operation.
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
        ExecutionScope::SessionOperation {
            session_id,
            operation_id,
        } => {
            identity.tag(3);
            identity.string(session_id);
            identity.string(operation_id);
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
        AwaitEventWaitIdentity::SessionCommandCancelSignal => identity.tag(7),
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
                ExecutionScope::session_operation("same", "same"),
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
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e74010000000000000002616200000000000000016301000000000000004374635f61303964393132656639303638393066323434643833346665383639656362353666633061363833393930623037343038343535346634663463646634366638",
                "await-event:v1:blake3:0d005a66f2ce577179eef20aa3d1ff17ba5da7486e9556b9010b711a77f9032b",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e74010000000000000001610000000000000002626301000000000000004374635f61303964393132656639303638393066323434643833346665383639656362353666633061363833393930623037343038343535346634663463646634366638",
                "await-event:v1:blake3:90583ad37089187bb23b5530f04084f0a8dfa1f16fde51f8bf5e6a657a9a1675",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e74020000000000000022705f3639633135363366643637353732373762383036653938663666376262663762020000000000000022705f3639633135363366643637353732373762383036653938663666376262663762000000000000000572656164790000000000000001",
                "await-event:v1:blake3:c2be604b51d59df696908fc17609b49f5a7308502a4b3f290a3f08e7bc741d1a",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e7403000000000000000473616d65000000000000000473616d6503",
                "await-event:v1:blake3:2c31e4daecaddbbecf5abb5a46d95ad53ea7010687cf6b1f9dbdaf9d128b7cae",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e7404000000000000000473616d6504",
                "await-event:v1:blake3:3a3852f14750eee1dc76aea3f281ee819270167fcf6c1393e48c002fb35fc057",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479020100000000000000106c6173682e61776169742d6576656e740500000000000000046f703a3005000000000000000130",
                "await-event:v1:blake3:7b5a0712e850db4093ba5ba8df55d57393057fa54ba5afc1800a3e683177ea14",
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
            "await-event:v1:blake3:1045c0033ecb42fcdd63373e548477bea72bd7423c6e83a9f7774dde9eda3766"
        );
    }

    #[test]
    fn promise_key_family_version_is_explicit_and_stable() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait =
            AwaitEventWaitIdentity::tool_completion(lash_sansio::ToolCallId::fixture("call"));

        assert_eq!(
            derive_key_id(&scope, &wait).expect("derive versioned key"),
            "await-event:v1:blake3:1045c0033ecb42fcdd63373e548477bea72bd7423c6e83a9f7774dde9eda3766"
        );
    }
}
