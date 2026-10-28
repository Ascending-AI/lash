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

pub(crate) mod checkpoint;
mod code;
#[cfg(any(test, feature = "perf-witness"))]
pub use checkpoint::helper_compatibility_work;
pub use checkpoint::{
    Checkpoint, CheckpointHeader, HelperCompatibilityError, HelperPins, HelperUpgradeCheckError,
    check_helper_compatibility, inspect_checkpoint,
};
pub use code::{RetainedCode, checkpoint_code, restore_selection};
pub mod census;
pub(crate) mod census_impl;
pub mod family;
pub(crate) mod family_impl;
pub mod heap;
pub mod root;
pub mod session;
pub mod source;

#[cfg(test)]
#[path = "laws/census.rs"]
mod census_laws;

pub use census::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CensusFault, CensusGraph,
    EntityFacts, SweepSet, TransientRoots,
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
    BindingRecord, BoundRecord, CellTargetKind, Charge, CheckpointRoot, CommittedOutcomeRecord,
    CursorKind, CursorRecord, EndedRecord, EndedTask, EnteredRecord, FinallyRecord, FunctionRecord,
    GuardRecord, HeaderRecord, HeapRoot, IncomingRecord, JoinerRecord, KeyedRoot, ListJoinRecord,
    LoopRecord, MemoryMeter, RootStatus, SavedLimits, SavedMeters, SequenceRoot, TaskStateRecord,
    WaitKindRecord, WaitPhase, WaitRecord,
};
pub use source::{PageAnswer, PageBody, PageKind, PagePortLimits, PageSource};

mod census_graph;
mod codec;
mod equality;
pub(crate) mod logical;
mod publication;
pub(crate) mod resident;
pub(crate) mod stored;
pub use publication::StatementRoots;
pub(crate) mod tree;

pub(crate) mod directory;
pub(crate) mod heap_impl;
pub(crate) mod metadata;
pub use metadata::{NameRef, SiteRef, TaskIdentityRef};
pub(crate) mod payload;
mod working;
pub(crate) use heap_impl::PagedHeap;
pub use heap_impl::heap_accounting;

pub use directory::execution::{
    FrameRecord, IteratedRecord, OccurrenceOwner, OccurrenceRecord, PendingRecord, PhaseRecord,
    ROLE_CHUNK, ROLE_FLAT, RoleChunk, RoleRecord, RolesRecord, TaskRecord,
};

#[cfg(test)]
mod laws {
    pub mod heap;
    #[path = "../family_impl/laws.rs"]
    mod lifecycle;
    #[path = "../logical/laws.rs"]
    mod logical;
    mod metadata;
    mod record_hints;
    pub mod root;
}
