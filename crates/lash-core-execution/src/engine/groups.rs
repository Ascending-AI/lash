//! Effect groups on the context: rank cursors, cancel fences and the
//! protected drain, not only open, next and close (ADR 0105 §4).

use serde::{Deserialize, Serialize};

use super::admission::{DriveContext, DriveFence, InheritedAuthority};
use crate::runtime::effect::{
    EffectGroupChildCommitOutcome, GroupChildFinalCommit, RankedGroupSettlement,
};
use crate::{EffectGroupHandle, GroupSettlement, LoserPolicy, RuntimeEffectGroup};

/// The group surface of an engine context, fence-aware.
///
/// **Rank contract.** A settlement's rank is assigned when the engine records
/// it, at the group's single serialization point. On `close(Cancel)`,
/// undecided children are seated in ascending position order. A replay
/// observes the same child at rank n. Ranks are dense per group: a rank is a
/// position in the ascending order of settlement sequences, which may skip.
///
/// The opener-side bookkeeping (reserve and bound, the held list, the
/// incorporation ledger) is a pure fold over recorded outcomes, never a
/// context operation.
pub trait DriveGroups: DriveContext {
    /// Records membership: the retained request (ADR 0099 §3). A reopen is
    /// fenced on shape.
    fn open_group(
        &self,
        f: &DriveFence,
        group: RuntimeEffectGroup,
    ) -> Self::Op<'_, EffectGroupHandle>;

    /// The rank cursor: serves the rank after `handle.consumed()`, then
    /// advances. The handle is the only cursor of record. When this op loses a
    /// race against a turn-cancel gate, the cursor is untouched.
    fn next_settlement<'h>(&'h self, h: &'h mut EffectGroupHandle)
    -> Self::Op<'h, GroupSettlement>;

    /// A cursorless read at one rank (ADR 0099 §8).
    fn read_settlement(
        &self,
        group: &GroupKey,
        rank: u64,
    ) -> Self::Op<'_, Option<RankedGroupSettlement>>;

    /// How many children of `group` have settled.
    fn settled_count(&self, group: &GroupKey) -> Self::Op<'_, u64>;

    /// The cancel fence: the one linearization point per child, from pending
    /// to committed with a commit sequence.
    fn commit_child_final(
        &self,
        a: &InheritedAuthority,
        c: GroupChildFinalCommit,
    ) -> Self::Op<'_, EffectGroupChildCommitOutcome>;

    /// The protected-drain barrier: resolves once no committed sibling below
    /// `commit_seq` still owes its drain.
    fn await_drain_admission(&self, group: &GroupKey, commit_seq: u64) -> Self::Op<'_, ()>;

    /// Close narrows only ([`LoserPolicy::resolve_close`]). Under `Cancel` it
    /// seals cancel decisions before any interrupt, seats cancelled ranks for
    /// undecided children in ascending position order, and excludes children
    /// that are committed but unseated: those drain, then seat their own rank.
    /// Idempotent.
    fn close_group(&self, h: EffectGroupHandle, d: LoserPolicy) -> Self::Op<'_, GroupClosed>;
}

/// A durable effect group's key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupKey(String);

impl GroupKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a [`close_group`](DriveGroups::close_group) applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupClosed {
    /// The disposition the close resolved: the declared one, or a narrower
    /// request.
    pub disposition: LoserPolicy,
}
