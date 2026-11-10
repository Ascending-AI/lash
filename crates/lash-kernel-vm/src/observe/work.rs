//! Exact counters of a run's work, by the function that did it.
//!
//! Charge and heap insertions are not counted where they happen: each is
//! read from the machine's own meters whenever the function being run
//! changes, and the difference goes to the function that ran in between.
//! The self charges therefore add up to the run's charge exactly, and the
//! counters add nothing to the paths that charge.

/// What a code did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeCounts {
    /// Frames pushed for it.
    pub calls: u64,
    /// Statements it ran.
    pub statements: u64,
    /// Expressions and action operands it evaluated.
    pub expressions: u64,
    /// Statements and expressions it ran while its forms were not charged:
    /// the body of a library function with a native implementation.
    pub uncharged_operations: u64,
    /// Charge while it was the innermost frame and no native call ran.
    pub self_charge: Option<u64>,
    /// Charge from a call's start to its end, callees and the natives it
    /// called included; a recursive call is counted once, by its outermost
    /// frame. Inclusive charges overlap: they never add up to a total.
    pub inclusive_charge: Option<u64>,
    /// Objects the heap took while it ran.
    pub heap_insertions: u64,
}

/// What a library function that runs without a frame did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeCounts {
    pub calls: u64,
    /// Calls its allocation-free fast path answered.
    pub fast_calls: u64,
    /// Calls charged nothing: the call did work the charge does not show.
    /// None when charge metering is Off.
    pub zero_charge_calls: Option<u64>,
    /// Calls made again after a collection, for room.
    pub retries: u64,
    /// The units its general calls counted on their work counter: a
    /// guarded function's guard units.
    pub guard_work: u64,
    /// Its formulas evaluated: charge formulas and guard limits.
    pub formula_evaluations: u64,
    pub self_charge: Option<u64>,
    pub heap_insertions: u64,
}
