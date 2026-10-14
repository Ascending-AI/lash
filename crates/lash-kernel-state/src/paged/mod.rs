//! The persistent semantic heap's page-level seams (FIG-6195): what the
//! heap, the machine, the accounting, the cell lifecycle and the embedder
//! agree on. See `docs/design/persistent-heap-seams.md`.
//!
//! Declarations only. Each trait names the layer that implements it; no
//! production code calls any of them before the cutover (FIG-6193).
//!
//! - [`source`] (S3): the synchronous embedder port pages are read and
//!   sealed through, and the semantic page kinds.
//! - [`heap`] (S1): [`SemanticHeap`], the one deep module execution and
//!   checkpoints use.
//! - [`root`] (S4): [`CheckpointRoot`] and the persistent execution records.
//! - [`census`] (S7): the certified upper bound, reservation and exact
//!   refinement.
//! - [`family`] (S8): frozen values, lazy identity families and cell carry.

pub mod census;
pub mod family;
pub mod heap;
pub mod root;
pub mod source;

pub use census::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CensusFault, CensusGraph,
    EntityCost, LogicalPermit, MIN_ENTITY_COST, PendingReservation, RefinedDecision,
    ReservationDecision, SweepSet, TransientRoots,
};
pub use family::{
    CarryClassification, CarryRoots, DataOnlyCertificate, FamilyBase, FamilyRecord, FrozenRef,
    FrozenSummary, HeapLifecycle, MembershipRun, SessionRoot,
};
pub use heap::{
    ClosureView, EditEpoch, FieldName, HeapWriteError, NormalKey, ObjectInit, ObjectKind,
    SealBudget, SealedHeap, SemanticHeap, TableCursor,
};
pub use root::{
    BindingRecord, BoundRecord, CheckpointRoot, CommittedOutcomeRecord, EndedRecord, EndedTask,
    EnteredRecord, FinallyRecord, HeapRoot, IncomingRecord, JoinerRecord, KeyedRoot,
    ListJoinRecord, LoopRecord, RootStatus, SavedMeters, SequenceRoot, TaskStateRecord,
    WaitKindRecord, WaitRecord, WaitStateRecord,
};
pub use source::{PageAnswer, PageBody, PageKind, PageSource};

mod codec;
pub mod tree;

pub mod directory;
pub mod heap_impl;
pub mod payload;
pub use heap_impl::PagedHeap;

#[cfg(test)]
mod laws {
    pub mod heap;
    pub mod root;
}

pub use directory::execution::{FrameRecord, TaskRecord};
