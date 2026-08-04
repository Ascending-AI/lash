//! Store-agnostic state transitions for AwaitEvent keyed promises.
//!
//! Backends own persistence, authentication, and compare-and-swap mechanics.
//! This module owns the semantic decisions that must remain identical across
//! the inline, SQLite, Postgres, and engine-backed implementations.
//!
//! Promise-key family v3 is a reject-and-recreate cutover from the live-serde
//! v2 preimage. SQLite/Postgres effect schemas are bumped with this change;
//! durable adapters without a Lash schema gate must recreate their promise
//! state before deploying it. Any future projection correction requires a new
//! family version and an explicit old-row policy.

use super::{AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome};
use crate::RuntimeError;

const AWAIT_EVENT_FAMILY_VERSION: u8 = 3;

/// Permanent tag registry for await-event promise identities.
///
/// Execution scopes: 1 turn, 2 process, 3 queue drain, 4 session delete,
/// 5 runtime operation. Wait identities: 1 tool completion, 2 process signal,
/// 3 turn cancel gate, 4 turn terminal, 5 custom. Retired tags remain burned.
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
            identity.string(tool_call_id);
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
    }
    identity.finish()
}

/// Derive the stable promise identity shared by every substrate.
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

/// Canonical bytes authenticated by HMAC-backed durable AwaitEvent adapters.
///
/// The key id remains in the signed material even though it is derived from
/// `scope` and `wait`: authenticating all three serialized key fields prevents
/// backends from accidentally accepting a key whose visible identity and
/// routing identity disagree.
pub fn sign_material(
    scope: &ExecutionScope,
    wait: &AwaitEventWaitIdentity,
    key_id: &str,
) -> Vec<u8> {
    let key_preimage = promise_key_preimage(scope, wait);
    let mut material = crate::stable_identity::IdentityEncoder::new(
        "lash.await-event-auth",
        AWAIT_EVENT_FAMILY_VERSION,
    );
    material.bytes(&key_preimage);
    material.string(key_id);
    material.finish()
}

/// State observed by a backend while holding its promise transition fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromiseState {
    /// No waiter or earlier resolution has materialized a row yet.
    Missing,
    /// A waiter has materialized the promise, but no terminal has won.
    Pending,
    /// A terminal has already won the first-writer-wins race.
    Resolved(Resolution),
    /// The owning session has been durably revoked.
    Revoked,
}

/// Pure decision returned for a proposed terminal transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromiseTransition {
    /// Persist this terminal with a first-writer-wins compare-and-swap.
    Store(Resolution),
    /// A prior terminal remains authoritative.
    AlreadyResolved(Resolution),
    /// The key must use the common non-oracular unknown/revoked result.
    UnknownOrRevoked,
    /// The operation intentionally leaves this promise unchanged.
    Unchanged,
}

impl PromiseTransition {
    /// Convert a committed resolution decision to the public resolver result.
    ///
    /// `Unchanged` is not a resolve result; it is used only by cancel sweeps.
    pub fn resolve_outcome(self) -> Option<ResolveOutcome> {
        match self {
            Self::Store(_) => Some(ResolveOutcome::Accepted),
            Self::AlreadyResolved(terminal) => Some(ResolveOutcome::AlreadyResolved { terminal }),
            Self::UnknownOrRevoked => Some(ResolveOutcome::UnknownOrRevoked),
            Self::Unchanged => None,
        }
    }
}

/// Decide a normal first-writer-wins resolve.
///
/// Missing promises accept the terminal so signal-before-wait is buffered.
pub fn resolve(state: PromiseState, proposed: Resolution) -> PromiseTransition {
    match state {
        PromiseState::Missing | PromiseState::Pending => PromiseTransition::Store(proposed),
        PromiseState::Resolved(terminal) => PromiseTransition::AlreadyResolved(terminal),
        PromiseState::Revoked => PromiseTransition::UnknownOrRevoked,
    }
}

/// Decide the session cancel-sweep transition for one promise.
///
/// Turn-control promises are never swept: cancelling their observation must
/// not manufacture a turn cancellation or terminal publication. Existing
/// terminals are equally immutable.
pub(crate) fn cancel_sweep(
    wait: &AwaitEventWaitIdentity,
    state: PromiseState,
) -> PromiseTransition {
    if wait.is_turn_control() {
        return PromiseTransition::Unchanged;
    }
    match state {
        PromiseState::Missing => PromiseTransition::Unchanged,
        PromiseState::Pending => PromiseTransition::Store(Resolution::Cancelled),
        PromiseState::Resolved(terminal) => PromiseTransition::AlreadyResolved(terminal),
        PromiseState::Revoked => PromiseTransition::UnknownOrRevoked,
    }
}

/// Pure session-tombstone decision shared by in-memory and durable stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRevocationTransition {
    MarkRevoked,
    AlreadyRevoked,
}

pub(crate) fn revoke_session(already_revoked: bool) -> SessionRevocationTransition {
    if already_revoked {
        SessionRevocationTransition::AlreadyRevoked
    } else {
        SessionRevocationTransition::MarkRevoked
    }
}

/// Whether mint and resolve may proceed for a session.
///
/// Both operations must consult the tombstone before touching promise rows.
pub(crate) fn session_allows_access(revoked: bool) -> bool {
    !revoked
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
                AwaitEventWaitIdentity::tool_completion("x:y"),
            ),
            (
                ExecutionScope::turn("a", "bc"),
                AwaitEventWaitIdentity::tool_completion("x:y"),
            ),
            (
                ExecutionScope::process("zero"),
                AwaitEventWaitIdentity::process_signal("zero", "ready", 1),
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
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e740100000000000000026162000000000000000163010000000000000003783a79",
                "await-event:v3:sha256:0b4e88d662ffcfab2d4dab8cdcbbc4eefce3e03e7b6f1f02a8c2c44f128ef26a",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e740100000000000000016100000000000000026263010000000000000003783a79",
                "await-event:v3:sha256:80a7e9660c1eb5120ff4041d5c07c0c1fdbd9d25ee6973730b34851752ed470d",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e740200000000000000047a65726f0200000000000000047a65726f000000000000000572656164790000000000000001",
                "await-event:v3:sha256:d5f3cca470277324bc667bbc1bde5b1c4956280c098c7cfbbd64561c53fdcce0",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e7403000000000000000473616d65000000000000000473616d6503",
                "await-event:v3:sha256:3c6f999b4ea0442f4bcff923b277e7859b1e1b17a96afc5a1be102cd3f9cc43f",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e7404000000000000000473616d6504",
                "await-event:v3:sha256:f81ce5cfee31d7bbedf9eb7668f2e6675a4356cf26b02e4ab8c8249c99ce431f",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e740500000000000000046f703a3005000000000000000130",
                "await-event:v3:sha256:32eeecf35973d577700d021ab92e5950ad380453f8843b0629e654598db0165c",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }
    }

    #[test]
    fn resolve_buffers_before_wait_and_preserves_the_first_terminal() {
        let first = Resolution::Ok(serde_json::json!("first"));
        assert_eq!(
            resolve(PromiseState::Missing, first.clone()),
            PromiseTransition::Store(first.clone())
        );
        assert_eq!(
            resolve(
                PromiseState::Resolved(first.clone()),
                Resolution::Ok(serde_json::json!("second")),
            ),
            PromiseTransition::AlreadyResolved(first)
        );
    }

    #[test]
    fn cancel_sweep_excludes_turn_control_and_existing_terminals() {
        assert_eq!(
            cancel_sweep(
                &AwaitEventWaitIdentity::TurnCancelGate,
                PromiseState::Pending,
            ),
            PromiseTransition::Unchanged
        );
        assert_eq!(
            cancel_sweep(
                &AwaitEventWaitIdentity::tool_completion("call"),
                PromiseState::Pending,
            ),
            PromiseTransition::Store(Resolution::Cancelled)
        );
        assert_eq!(
            cancel_sweep(
                &AwaitEventWaitIdentity::tool_completion("call"),
                PromiseState::Resolved(Resolution::Timeout),
            ),
            PromiseTransition::AlreadyResolved(Resolution::Timeout)
        );
    }

    #[test]
    fn revoked_sessions_reject_access_and_revoke_idempotently() {
        assert!(session_allows_access(false));
        assert!(!session_allows_access(true));
        assert_eq!(
            revoke_session(false),
            SessionRevocationTransition::MarkRevoked
        );
        assert_eq!(
            revoke_session(true),
            SessionRevocationTransition::AlreadyRevoked
        );
        assert_eq!(
            resolve(PromiseState::Revoked, Resolution::Cancelled),
            PromiseTransition::UnknownOrRevoked
        );
    }

    #[test]
    fn authentication_comparison_covers_content_and_length_mismatches() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sale"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }

    #[test]
    fn signing_material_is_the_stable_framed_key_projection() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");
        let key_id = derive_key_id(&scope, &wait).expect("derive key id");

        assert_eq!(
            hex(&sign_material(&scope, &wait, &key_id)),
            "6c6173682d737461626c652d6964656e74697479010300000000000000156c6173682e61776169742d6576656e742d6175746800000000000000576c6173682d737461626c652d6964656e74697479010300000000000000106c6173682e61776169742d6576656e7401000000000000000773657373696f6e00000000000000047475726e01000000000000000463616c6c000000000000005661776169742d6576656e743a76333a7368613235363a37323934363636313632336137333661643764646466656163653231383265623531643565323566313864346165663138396637396561633438626263666562"
        );
    }

    #[test]
    fn promise_key_family_version_is_explicit_and_stable() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");

        assert_eq!(
            derive_key_id(&scope, &wait).expect("derive versioned key"),
            "await-event:v3:sha256:72946661623a736ad7dddfeace2182eb51d5e25f18d4aef189f79eac48bbcfeb"
        );
    }
}
