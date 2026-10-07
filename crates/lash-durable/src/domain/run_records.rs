//! Run records (V0, then L4): the `RunLedger` records as rows, keyed by
//! `(owner, run, ordinal)` (ADR 0132 §5).
//!
//! The fold over these rows stays the authority; resume folds rows into
//! state and calls no producer. The execution record splits into a start,
//! committed before any body runs, and an outcome.

use crate::ids::Epoch;
use lash_sansio::ToolCallId;

use super::keys::{Ordinal, OwnerKey, RunSeq};

/// What a run record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunRecordKind {
    /// A round's admission: membership, pinned policy, limits, waits.
    Admit,
    /// An execution started; committed before its body runs.
    XStart,
    /// An execution's outcome.
    XOutcome,
    /// An execution parked on its waits; its outcome follows.
    XWait,
    /// A coordinator's decision.
    Decide,
    /// A presentation (incorporation) record.
    Present,
    /// A retry with its due time.
    Retry,
}

impl RunRecordKind {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::XStart => "x_start",
            Self::XOutcome => "x_outcome",
            Self::XWait => "x_wait",
            Self::Decide => "decide",
            Self::Present => "present",
            Self::Retry => "retry",
        }
    }

    /// The stored spelling read back.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        [
            Self::Admit,
            Self::XStart,
            Self::XOutcome,
            Self::XWait,
            Self::Decide,
            Self::Present,
            Self::Retry,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == stored)
    }
}

/// One stored run record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRecordRow {
    /// Its owner.
    pub owner: OwnerKey,
    /// Its run.
    pub run: RunSeq,
    /// Its ordinal.
    pub ordinal: Ordinal,
    /// What it is.
    pub kind: RunRecordKind,
    /// The call it concerns, for execution records.
    pub call: Option<ToolCallId>,
    /// The `RunLedger` record body; large material by digest.
    pub record_json: String,
    /// The epoch of the commit that wrote it.
    pub written_epoch: Epoch,
}

/// A run-record write inside an owner commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunRecordWrite {
    /// Append one record. Refused with
    /// [`DomainRefusal::RunOrdinalTaken`](super::DomainRefusal::RunOrdinalTaken)
    /// when its ordinal is taken, with
    /// [`DomainRefusal::RunOrdinalGap`](super::DomainRefusal::RunOrdinalGap)
    /// when the run has no record at the ordinal before it, and an outcome with
    /// [`DomainRefusal::OutcomeExists`](super::DomainRefusal::OutcomeExists)
    /// when its call already has one.
    Append {
        /// The owner.
        owner: OwnerKey,
        /// The run.
        run: RunSeq,
        /// The ordinal.
        ordinal: Ordinal,
        /// What it is.
        kind: RunRecordKind,
        /// The call it concerns.
        call: Option<ToolCallId>,
        /// The record body.
        record_json: String,
    },
    /// Delete `owner`'s records of every run before `before`, past their
    /// retention.
    Prune {
        /// The owner.
        owner: OwnerKey,
        /// The first run kept.
        before: RunSeq,
    },
}

/// One admitted execution's identity: its owner, run and ordinal. A
/// `Repeatable` execution re-runs at its same ordinal after a crash, and a
/// retry takes the next one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AdmittedId {
    /// The owner.
    pub owner: OwnerKey,
    /// The run.
    pub run: RunSeq,
    /// The ordinal of its `x_start`.
    pub ordinal: Ordinal,
}
