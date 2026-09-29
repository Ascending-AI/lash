//! The accepted shape of one durable effect group, as the index object stores
//! it (ADR 0065, ADR 0099 §3).
//!
//! Extracted from the index handlers rather than kept beside them: this is the
//! group's *record*, which the handlers read and the runtime writes. The
//! accepted membership — every child's canonical envelope — travels beside
//! it, never inside it (FIG-4068): the shape is what every per-child handler
//! reads and every child request carries, so it stays the size of the
//! group's identity, while the membership is read only where children are
//! rebuilt.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use lash_core::{
    ExecutionScope, GroupWakePolicy, LoserPolicy, RuntimeEffectControllerError, RuntimeEffectGroup,
};
use restate_sdk::errors::TerminalError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupShape {
    pub wake: GroupWakePolicy,
    pub loser_disposition: LoserPolicy,
    pub replay_keys: Vec<String>,
    pub wait_scope: ExecutionScope,
    /// The admitted scope of the controller that opened the group: the
    /// authority every child runs under (ADR 0099 §1). A child invocation
    /// mints its controller from its own context, and a timer or durable-wait
    /// child has no request of its own that names an admission, so its
    /// controller is admitted from this record, pinned to the opener's process
    /// incarnation when the opener is a process, before it is routed through
    /// the host's stack (FIG-3780).
    #[serde(with = "lash_core::admitted_scope_wire")]
    pub opener: lash_core::AdmittedScope,
}

impl EffectGroupShape {
    /// The shape `opener` opens `group` with, and the group's accepted
    /// membership beside it.
    pub(crate) fn from_group(
        group: &RuntimeEffectGroup,
        opener: &lash_core::AdmittedScope,
    ) -> Result<(Self, EffectGroupMembership), RuntimeEffectControllerError> {
        let replay_keys = group
            .children()
            .iter()
            .map(|child| child.invocation.effect_replay_key().to_owned())
            .collect();
        let wait_scope = ExecutionScope::runtime_operation(group.group_key());
        let membership = group
            .children()
            .iter()
            .map(|child| {
                serde_json::to_string(child).map_err(|error| {
                    RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::RuntimeEffectGroupShape,
                        format!(
                            "child of durable effect group {} cannot be retained: {error}",
                            group.group_key()
                        ),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            Self {
                wake: group.wake(),
                loser_disposition: group.loser_disposition(),
                replay_keys,
                wait_scope,
                opener: opener.clone(),
            },
            EffectGroupMembership(membership),
        ))
    }

    /// The declared arity: the write-time expectation, derived from the
    /// retained replay keys rather than stored as a second fact — the SQL
    /// tiers' `expected_children` is the same answer spelled as a column.
    pub(crate) fn children(&self) -> usize {
        self.replay_keys.len()
    }

    /// The invariant `from_group` establishes, re-checked on the way in.
    ///
    /// The shape and its membership are two public values that arrive over
    /// the wire side by side, so they carry whatever the caller put in them.
    /// Every later reader pairs a position drawn from the retained list with
    /// the replay key at that position; a membership that disagrees with the
    /// shape's arity is a caller defect that can never become valid by
    /// retrying, so it is refused once, terminally, at the boundary rather
    /// than surviving into stored state where a later handler would meet it.
    pub(crate) fn validate_membership(
        &self,
        membership: &EffectGroupMembership,
    ) -> Result<(), TerminalError> {
        // Every disagreement, empty included: a shape that cannot rebuild its
        // own children is a caller defect no retry fixes, refused once here
        // rather than left to a handler that would rebuild the wrong number.
        if membership.0.len() != self.replay_keys.len() {
            return Err(TerminalError::new(format!(
                "effect-group shape declares {} children but retains {} accepted \
                 requests",
                self.replay_keys.len(),
                membership.0.len()
            )));
        }
        Ok(())
    }

    /// The reopen fence, matching the shared `fence_reopen` contract the SQL
    /// tiers apply: arity, wake rule, and declared loser disposition are the
    /// journaled facts a reopen may not restate. `replay_keys` and
    /// the membership are deliberately absent — they are the *retained* state,
    /// and a reopen is exactly the caller offering children that may disagree
    /// with it; the recorded membership wins (ADR 0099 §3).
    pub(crate) fn fences_equivalent(&self, other: &Self) -> bool {
        self.replay_keys.len() == other.replay_keys.len()
            && self.wake == other.wake
            && self.loser_disposition == other.loser_disposition
    }

    /// The replay key of a child position, as a terminal error when the shape
    /// does not have one.
    pub(crate) fn member_replay_key(&self, position: usize) -> Result<&str, TerminalError> {
        self.replay_keys
            .get(position)
            .map(String::as_str)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "effect-group shape has no replay key for child {position} of {}",
                    self.replay_keys.len()
                ))
            })
    }

    /// The group's identity digest: the shape and the membership it was
    /// opened with.
    pub(crate) fn digest(
        &self,
        membership: &EffectGroupMembership,
    ) -> Result<String, TerminalError> {
        let bytes = serde_json::to_vec(&(self, membership)).map_err(|error| {
            TerminalError::new(format!("serialize effect-group shape: {error}"))
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

/// The accepted membership of one durable effect group, in child order: the
/// canonical envelope that rebuilds each child (ADR 0099 §3).
///
/// The Restate tier's equivalent of `runtime_effect_group_child`. Its durable
/// store is the index object's own state key, written once at open and read
/// only where children are rebuilt — dispatch, a content-checked reopen and a
/// cancel decision — never by the per-child handlers, which read the shape
/// alone. Kept apart because every envelope carries its session's tool
/// surface: a membership inside the shape made each of a width-n group's
/// O(n) per-child handler reads, and each child request, carry all n
/// envelopes, so a group cost O(n²) envelopes (FIG-4068).
///
/// Required wherever a shape is opened. An empty membership beside a nonzero
/// arity is not "recorded before §3" to be tolerated — it is a group that
/// cannot reconstruct its own children, and
/// [`EffectGroupShape::validate_membership`] refuses it like any other
/// disagreement.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectGroupMembership(pub Vec<String>);

impl EffectGroupMembership {
    /// The envelope of `position`, decoded. A member that is missing or does
    /// not decode is corruption the journal itself produced, a protocol
    /// defect rather than a retryable error.
    pub(crate) fn envelope(
        &self,
        group_key: &str,
        position: usize,
    ) -> Result<lash_core::RuntimeEffectEnvelope, TerminalError> {
        let member = self.0.get(position).ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {group_key} retains no membership for child {position}"
            ))
        })?;
        serde_json::from_str(member).map_err(|error| {
            TerminalError::new(format!(
                "retained membership of effect group {group_key} child {position} does not \
                 decode: {error}"
            ))
        })
    }
}
