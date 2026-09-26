//! The accepted shape of one durable effect group, as the index object stores
//! it (ADR 0065, ADR 0099 §3).
//!
//! Extracted from the index handlers rather than kept beside them: this is the
//! group's *record*, which the handlers read and the runtime writes, and it now
//! carries the accepted membership as well as the identity.

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
    /// The accepted membership, in child order: the canonical envelope that
    /// rebuilds each child (ADR 0099 §3).
    ///
    /// The Restate tier's equivalent of `runtime_effect_group_child`. Its
    /// durable store is this object's state rather than a table, so the facts
    /// live here; `replay_keys` above is the same identity a SQL membership row
    /// carries as a column, kept for the position lookups every handler already
    /// does without decoding an envelope.
    ///
    /// Required, with no `serde(default)`. An empty membership beside a nonzero
    /// arity is not "recorded before §3" to be tolerated — it is a shape that
    /// cannot reconstruct its own children, and
    /// [`validate_wire`](Self::validate_wire) refuses it like any other
    /// disagreement. There is no in-flight population to protect: ADR 0099
    /// records that no production caller of `open_effect_group` exists, so no
    /// deployment can be holding a group whose state predates this field.
    pub membership: Vec<String>,
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
    /// The shape `opener` opens `group` with.
    pub(crate) fn from_group(
        group: &RuntimeEffectGroup,
        opener: &lash_core::AdmittedScope,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let replay_keys = group
            .children()
            .iter()
            .map(|child| child.invocation.replay_key().to_owned())
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
        Ok(Self {
            wake: group.wake(),
            loser_disposition: group.loser_disposition(),
            replay_keys,
            wait_scope,
            membership,
            opener: opener.clone(),
        })
    }

    /// The declared arity: the write-time expectation, derived from the
    /// retained replay keys rather than stored as a second fact — the SQL
    /// tiers' `expected_children` is the same answer spelled as a column.
    pub(crate) fn children(&self) -> usize {
        self.replay_keys.len()
    }

    /// The invariant `from_group` establishes, re-checked on the way in.
    ///
    /// `replay_keys` and `membership` are two public fields of a public type,
    /// so a shape that arrives over the wire carries whatever the caller put
    /// in it. Every later reader pairs a position drawn from the retained
    /// list with the replay key at that position; a shape whose two halves
    /// disagree is a caller defect that can never become valid by retrying,
    /// so it is refused once, terminally, at the boundary rather than
    /// surviving into stored state where a later handler would meet it.
    pub(crate) fn validate_wire(&self) -> Result<(), TerminalError> {
        // Every disagreement, empty included: a shape that cannot rebuild its
        // own children is a caller defect no retry fixes, refused once here
        // rather than left to a handler that would rebuild the wrong number.
        if self.membership.len() != self.replay_keys.len() {
            return Err(TerminalError::new(format!(
                "effect-group shape declares {} children but retains {} accepted \
                 requests",
                self.replay_keys.len(),
                self.membership.len()
            )));
        }
        Ok(())
    }

    /// The reopen fence, matching the shared `fence_reopen` contract the SQL
    /// tiers apply: arity, wake rule, and declared loser disposition are the
    /// journaled facts a reopen may not restate. `replay_keys` and
    /// `membership` are deliberately absent — they are the *retained* state,
    /// and a reopen is exactly the caller offering children that may disagree
    /// with it; the recorded membership wins (ADR 0099 §3).
    pub(crate) fn fences_equivalent(&self, other: &Self) -> bool {
        self.replay_keys.len() == other.replay_keys.len()
            && self.wake == other.wake
            && self.loser_disposition == other.loser_disposition
    }

    /// The replay key of a child position, as a terminal error when the shape
    /// does not have one.
    pub(crate) fn replay_key(&self, position: usize) -> Result<&str, TerminalError> {
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

    pub(crate) fn digest(&self) -> Result<String, TerminalError> {
        let bytes = serde_json::to_vec(self).map_err(|error| {
            TerminalError::new(format!("serialize effect-group shape: {error}"))
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}
