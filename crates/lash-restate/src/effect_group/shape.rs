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
    pub children: usize,
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
    /// `#[serde(default)]` is the one concession to an object written by an
    /// older deployment: its state predates this field, and a group already
    /// open when a deployment rolls cannot be re-accepted. An empty membership
    /// therefore means "recorded before §3", which
    /// [`validate_wire`](Self::validate_wire) distinguishes from a membership
    /// that disagrees with the arity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub membership: Vec<String>,
}

impl EffectGroupShape {
    pub(crate) fn from_group(
        group: &RuntimeEffectGroup,
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
            children: group.children().len(),
            wake: group.wake(),
            loser_disposition: group.loser_disposition(),
            replay_keys,
            wait_scope,
            membership,
        })
    }

    /// The invariant `from_group` establishes, re-checked on the way in.
    ///
    /// `children` and `replay_keys` are two public fields of a public type, so
    /// a shape that arrives over the wire carries whatever the caller put in
    /// it. Every later reader pairs a position drawn from `children` with the
    /// replay key at that position; a shape whose two halves disagree is a
    /// caller defect that can never become valid by retrying, so it is refused
    /// once, terminally, at the boundary rather than surviving into stored
    /// state where a later handler would meet it.
    pub(crate) fn validate_wire(&self) -> Result<(), TerminalError> {
        if self.children != self.replay_keys.len() {
            return Err(TerminalError::new(format!(
                "effect-group shape declares {} children but carries {} replay keys",
                self.children,
                self.replay_keys.len()
            )));
        }
        // Empty is "recorded before the membership existed" and is passed
        // through; any other disagreement is a caller defect that no retry
        // fixes, refused here rather than left to a handler that would rebuild
        // the wrong number of children.
        if !self.membership.is_empty() && self.membership.len() != self.children {
            return Err(TerminalError::new(format!(
                "effect-group shape declares {} children but retains {} accepted \
                 requests",
                self.children,
                self.membership.len()
            )));
        }
        Ok(())
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
                    self.children
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
