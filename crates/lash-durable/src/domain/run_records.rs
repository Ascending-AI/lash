//! Run facts (L4): the `RunLedger` records as rows, keyed by `(owner, run,
//! ordinal)` (ADR 0132 §5), each a RoundFact checkpoint graph node
//! ([`RunFactRow`](super::RunFactRow)).
//!
//! The fold over these rows stays the authority; resume folds rows into
//! state and calls no producer. The execution record splits into a start,
//! committed before any body runs, and an outcome.

use super::keys::{Ordinal, OwnerKey, RunSeq};

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
