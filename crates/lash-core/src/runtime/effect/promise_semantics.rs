//! Store-agnostic state transitions for AwaitEvent keyed promises.
//!
//! Backends own persistence, authentication, and compare-and-swap mechanics.
//! This module owns the semantic decisions that must remain identical across
//! the inline, SQLite, Postgres, and engine-backed implementations.

use super::{AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome};
use crate::RuntimeError;

/// Derive the stable promise identity shared by every substrate.
pub fn derive_key_id(
    scope: &ExecutionScope,
    wait: &AwaitEventWaitIdentity,
) -> Result<String, RuntimeError> {
    #[derive(serde::Serialize)]
    struct PromiseKeyWire<'a> {
        version: u8,
        scope: &'a ExecutionScope,
        wait: &'a AwaitEventWaitIdentity,
    }

    scope.validate()?;
    wait.validate()?;
    crate::stable_hash::stable_json_sha256_hex(&PromiseKeyWire {
        version: 2,
        scope,
        wait,
    })
    .map(|digest| format!("v2:{digest}"))
    .map_err(|err| {
        RuntimeError::new(
            "await_event_key_hash",
            format!("failed to hash await-event identity: {err}"),
        )
    })
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
    serde_json::to_vec(&(scope, wait, key_id))
        .expect("await-event signing material contains only infallible JSON values")
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
pub fn cancel_sweep(wait: &AwaitEventWaitIdentity, state: PromiseState) -> PromiseTransition {
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
pub enum SessionRevocationTransition {
    MarkRevoked,
    AlreadyRevoked,
}

pub fn revoke_session(already_revoked: bool) -> SessionRevocationTransition {
    if already_revoked {
        SessionRevocationTransition::AlreadyRevoked
    } else {
        SessionRevocationTransition::MarkRevoked
    }
}

/// Whether mint and resolve may proceed for a session.
///
/// Both operations must consult the tombstone before touching promise rows.
pub fn session_allows_access(revoked: bool) -> bool {
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
    fn signing_material_is_the_canonical_scope_wait_key_tuple() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");
        let key_id = derive_key_id(&scope, &wait).expect("derive key id");

        assert_eq!(
            sign_material(&scope, &wait, &key_id),
            serde_json::to_vec(&(scope, wait, key_id)).expect("serialize tuple")
        );
    }

    #[test]
    fn promise_key_wire_epoch_is_explicit_and_stable() {
        let scope = ExecutionScope::turn("session", "turn");
        let wait = AwaitEventWaitIdentity::tool_completion("call");

        assert_eq!(
            derive_key_id(&scope, &wait).expect("derive versioned key"),
            "v2:bec06c764e9e73f476e273330ac61b9510cba5b51555db23f7b142b3aeeac586"
        );
    }
}
