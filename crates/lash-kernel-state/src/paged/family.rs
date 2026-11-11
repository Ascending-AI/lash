//! S8: frozen values and the carried session.
//!
//! A frozen value is immutable structure pages with preorder entity counts:
//! an effect outcome, a terminal result, a saved function's capture. It is
//! never a pointer into a live, mutable directory.

use lash_kernel_doc::heap::{LogicalBytes, PageRef};

/// What a frozen value's root page states, checked when the value entered
/// the trusted domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrozenSummary {
    /// How many mutable entities the tree holds, in preorder.
    pub entities: u64,
    /// The logical cost an instantiation reserves.
    pub cost: LogicalBytes,
    /// No closure, task, ref or handle inside: pure data.
    pub data_only: bool,
}

/// A frozen value: its root page (kind
/// [`PageKind::FROZEN_ROOT`](super::PageKind::FROZEN_ROOT)) and summary.
#[derive(Clone, Copy, Debug)]
pub struct FrozenRef {
    pub root: PageRef,
    pub summary: FrozenSummary,
}

/// A carried session: what a cell starts from and a terminal cell hands
/// on, the bytecode core's run session page
/// ([`PageKind::RUN_SESSION`](super::PageKind::RUN_SESSION), read by
/// [`run::session`](super::run::session)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionRoot(pub(crate) PageRef);
