//! K6: a physical cut transfers the complete logical Run (FIG-4881,
//! FIG-4739, FIG-4890 and FIG-4889 implement it).
//!
//! A cut moves through RequestCut, Quiescing and Capturable. RequestCut
//! stops new admission. Quiescing keeps polling until every issued local
//! attempt is durably acknowledged; a Deferred source does not hold the cut,
//! its subscription transfers instead. Capturable then transfers, in one
//! ownership move, every Run event, retained material, pending source,
//! owed launch and cancel, the state frontier, the held capacity and the
//! protocol's VM continuation. No native future, socket or borrowed context
//! transfers. Turn and process segments share this record and its laws and
//! keep their own lifecycle transactions.
//!
//! The transfer is bound to its logical Run's owner (FIG-4861): a fresh Run
//! that executes identical source adopts nothing from it, and a terminal
//! Run's transfer is adopted by nobody.

use lash_sansio::BoundaryReason;
use serde::{Deserialize, Serialize};

use super::retention::{MaterialHolder, RetainedBundle};
use super::run_event::{RunEventOrdinal, RunLifecycle, SegmentOrdinal};
use super::source_seal::SourceSubscription;
use super::state_command::StateFrontier;
use crate::effect_opener::EffectOpener;
use crate::process_identity::StartKey;

/// Where a requested cut stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutPhase {
    /// Admission is stopped; issued work continues.
    RequestCut,
    /// Waiting for the durable acknowledgement of issued local attempts.
    Quiescing,
    /// Nothing local is unacknowledged; the transfer may be captured.
    Capturable,
}

/// A requested physical cut of a logical Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    pub reason: BoundaryReason,
    pub phase: CutPhase,
}

impl Cut {
    /// A cut just requested for `reason`.
    #[must_use]
    pub fn request(reason: BoundaryReason) -> Self {
        Self {
            reason,
            phase: CutPhase::RequestCut,
        }
    }

    /// Whether the Run may admit new work: never once a cut is requested.
    #[must_use]
    pub const fn admits_new_work(&self) -> bool {
        false
    }

    /// The phase after observing `unacknowledged_local` issued local
    /// attempts without a durable acknowledgement. Deferred sources are not
    /// counted: they transfer pending.
    #[must_use]
    pub fn observe(self, unacknowledged_local: usize) -> Self {
        Self {
            phase: if unacknowledged_local == 0 {
                CutPhase::Capturable
            } else {
                CutPhase::Quiescing
            },
            ..self
        }
    }
}

/// Everything a cut hands its successor segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTransfer {
    /// The logical Run's owner address.
    pub owner: EffectOpener,
    pub reason: BoundaryReason,
    pub from: SegmentOrdinal,
    /// Every event below this ordinal transfers; the successor appends at
    /// it.
    pub events: RunEventOrdinal,
    /// Every unconsumed payload, in bundles retained under the transferring
    /// segment's lease before publication. The successor acquires its own
    /// lease on each before the predecessor releases.
    pub material: Vec<RetainedBundle>,
    /// Pending sources, rebound to the successor on adoption.
    pub subscriptions: Vec<SourceSubscription>,
    /// Declared starts admitted and not yet launched.
    pub owed_starts: Vec<StartKey>,
    /// Cancels owed to launched starts.
    pub owed_cancels: Vec<StartKey>,
    pub state: StateFrontier,
    /// Tool-call capacity the Run still holds, including settled work a
    /// consumer still needs.
    pub reserved_calls: u32,
    /// Whether the protocol committed a VM continuation with the cut.
    pub vm_continuation: bool,
}

/// Why a transfer cannot be captured or adopted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationRefusal {
    #[error("the cut is not capturable yet")]
    NotQuiescent,
    #[error("material is journal-local and cannot cross segments")]
    UnretainedMaterial,
    #[error("retained material is not held by the transferring segment's lease")]
    UnleasedMaterial,
    #[error("a subscription is not held by the transferring segment")]
    ForeignSubscription,
    #[error("the transfer belongs to another logical Run")]
    ForeignOwner,
    #[error("the logical Run is terminal; its transfer is void")]
    OwnerTerminal,
    #[error("segment {found} is not the successor of segment {from}")]
    NotSuccessor { from: u32, found: u32 },
}

impl RunTransfer {
    /// Check a transfer captured under `cut`.
    ///
    /// # Errors
    ///
    /// [`ContinuationRefusal`] for a cut still quiescing, material outside
    /// a retained bundle, a bundle the transferring segment holds no lease
    /// on, or a subscription another segment holds.
    pub fn check_capture(&self, cut: &Cut) -> Result<(), ContinuationRefusal> {
        if cut.phase != CutPhase::Capturable {
            return Err(ContinuationRefusal::NotQuiescent);
        }
        if !self.material.iter().all(RetainedBundle::is_retained) {
            return Err(ContinuationRefusal::UnretainedMaterial);
        }
        let holder = self.holder();
        if self.material.iter().any(|bundle| bundle.holder != holder) {
            return Err(ContinuationRefusal::UnleasedMaterial);
        }
        if self.subscriptions.iter().any(|subscription| {
            subscription.owner != self.owner || subscription.segment != self.from
        }) {
            return Err(ContinuationRefusal::ForeignSubscription);
        }
        Ok(())
    }

    /// The lease holder of the transferring segment.
    #[must_use]
    pub fn holder(&self) -> MaterialHolder {
        MaterialHolder::Segment {
            opener: self.owner.clone(),
            segment: self.from,
        }
    }

    /// Adopt the transfer into segment `successor` of the Run `owner`,
    /// whose recorded lifecycle is `lifecycle`; the subscriptions move to the
    /// successor.
    ///
    /// # Errors
    ///
    /// [`ContinuationRefusal::ForeignOwner`] for a fresh or different Run,
    /// [`ContinuationRefusal::OwnerTerminal`] once the Run settled or was
    /// cancelled, and [`ContinuationRefusal::NotSuccessor`].
    pub fn adopt(
        mut self,
        owner: &EffectOpener,
        lifecycle: RunLifecycle,
        successor: SegmentOrdinal,
    ) -> Result<Self, ContinuationRefusal> {
        if &self.owner != owner {
            return Err(ContinuationRefusal::ForeignOwner);
        }
        if lifecycle == RunLifecycle::Settled {
            return Err(ContinuationRefusal::OwnerTerminal);
        }
        if self.from.0.checked_add(1) != Some(successor.0) {
            return Err(ContinuationRefusal::NotSuccessor {
                from: self.from.0,
                found: successor.0,
            });
        }
        for subscription in &mut self.subscriptions {
            subscription.segment = successor;
        }
        self.state.owner_segment = successor;
        Ok(self)
    }
}
