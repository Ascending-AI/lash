//! Test-only facts emitted at actual kernel commits and exits.

use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// A source-certified grouped kernel selected by packet lowering.
pub enum Family {
    /// A fixed-arity scalar packet and its shared destination.
    Scalar,
    /// A source-certified member guard, read and optional consumer.
    GuardedRead,
    /// An owned append preserving the original text until acceptance.
    TextAppend,
    /// A fixed typed repeat graph.
    Repeated,
    /// A member read, scalar consumer and local write.
    ReadUpdate,
    /// Parts construction, join, append and local write.
    FormatAppend,
    /// A stable merge prefix or its post-callback placement.
    Merge,
    /// A list write followed by its source index update.
    Placement,
    /// A cursor item's source hole/branch selection.
    Visit,
    /// A source cursor's item and membership result.
    ReadSlot,
    /// An ordered scalar reduction over source items.
    Reduction,
    /// An item append retaining the source list and member identity.
    CollectionAppend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// An observed kernel commit or recovery edge.
pub enum Exit {
    /// A source operation or group committed.
    Commit,
    /// An input or resident guard missed.
    Guard,
    /// The initial source prefix did not fit.
    Prefix,
    /// A repeated graph exhausted its condition.
    End,
    /// An internal source phase stopped after an earlier phase committed.
    Partial,
    /// A committed operation reached a legal source boundary.
    Boundary,
    /// A committed prefix hands off to its guest callback.
    Callback,
}

/// Actual kernel events, indexed by family and exit.
pub type Counts = BTreeMap<(Family, Exit), u64>;

thread_local! {
    static COUNTS: RefCell<Option<Counts>> = const { RefCell::new(None) };
}

pub(crate) fn begin() {
    COUNTS.with(|counts| *counts.borrow_mut() = Some(Counts::new()));
}

pub(crate) fn take() -> Counts {
    COUNTS.with(|counts| counts.borrow_mut().take().unwrap_or_default())
}

pub(super) fn record(family: Family, exit: Exit) {
    COUNTS.with(|counts| {
        if let Some(counts) = &mut *counts.borrow_mut() {
            *counts.entry((family, exit)).or_default() += 1;
        }
    });
}
