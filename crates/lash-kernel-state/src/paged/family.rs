//! S8: frozen values, lazy identity families and cell carry.
//!
//! A frozen value is immutable structure pages with preorder entity counts:
//! an effect outcome, a terminal result, a saved function's capture. It is
//! never a pointer into a live, mutable directory.
//!
//! Delivering one where the guest expects a fresh mutable graph
//! (`K-EFF-003`) takes a fresh [`FamilyId`]: entity `(family, n)` is the
//! n-th mutable occurrence of the frozen tree in preorder, so equal
//! subtrees at distinct positions are distinct entities, and nothing is
//! instantiated until it is touched. An edit writes an override row in the
//! same directory under the same identity. A family records its extent and
//! its census membership; a swept ordinal stays absent and never falls
//! back to its frozen body. Lookup checks membership first.
//!
//! A session carries its heap root, binding table and function table into
//! the next cell without renaming or copying; a fork shares the immutable
//! pages and owns its own directory version from then on.

use lash_kernel_doc::Name;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, LogicalBytes, PageRef, PageValue};

use super::heap::HeapWriteError;
use super::root::KeyedRoot;

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

/// A run of ordinals with one census membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MembershipRun {
    pub start: u64,
    pub len: u64,
    pub live: bool,
}

/// One lazy family's record in the heap root's family table.
#[derive(Clone, Copy, Debug)]
pub struct FamilyRecord {
    pub family: FamilyId,
    /// The frozen base untouched entities read from.
    pub base: FrozenRef,
    /// Ordinals `0..extent` are allocated.
    pub extent: u64,
    /// [`MembershipRun`]s by start: the swept ranges are not live.
    pub membership: KeyedRoot,
}

/// A carried session: what a cell starts from and a terminal cell hands
/// on (kind [`PageKind::SESSION_ROOT`](super::PageKind::SESSION_ROOT)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionRoot {
    pub(crate) heap: PageRef,
    /// Carried bindings: name to value.
    pub(crate) bindings: KeyedRoot,
    /// Saved functions: name to its function bundle node.
    pub(crate) functions: KeyedRoot,
    pub(crate) certificate: DataOnlyCertificate,
}

/// A summary of the whole census's population, maintained with directory
/// edits: when it shows no closure entity and no task edge anywhere, every
/// binding is data and carry classification reads nothing. It is
/// conservative: a present closure proves no particular binding bad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataOnlyCertificate {
    pub closure_entities: u64,
    pub task_edges: u64,
}

/// What carrying a cell's bindings decides (`K-SES-003`).
#[derive(Clone, Debug)]
pub struct CarryClassification {
    /// Bindings carried as they are.
    pub carried: KeyedRoot,
    /// Bindings dropped because they reach a closure or a task.
    pub dropped: Vec<Name>,
}

/// The roots a terminal cell's carry selects.
#[derive(Clone, Debug)]
pub struct CarryRoots {
    pub bindings: KeyedRoot,
    pub functions: KeyedRoot,
}

/// How the paged heap reads a lazy family's untouched entity.
///
/// Implemented by the lifecycle layer (hp-life, FIG-6192); the paged heap
/// (hp-heap) calls it when a directory lookup finds no override row.
pub trait FamilyBase {
    /// The lazy initial head of a live member. The heap checks its current
    /// membership before calling this; None is corrupt-family state.
    fn base_object(
        &mut self,
        entity: EntityId,
        frozen: &FrozenRef,
        access: &super::tree::Access<'_>,
    ) -> Result<Option<super::directory::ObjectHead>, HeapFault>;
}

/// The paged heap's lifecycle operations.
///
/// Implemented over the paged heap by the lifecycle layer (hp-life,
/// FIG-6192).
pub trait HeapLifecycle {
    /// Freezes `value`'s graph into frozen pages once, at a semantic
    /// boundary (an effect argument, a saved function's capture, a run's
    /// result). Refuses a cycle or a non-data value where `K-EFF-002`
    /// requires data.
    fn freeze(&mut self, value: &PageValue, data_only: bool) -> Result<FrozenRef, HeapWriteError>;
    /// Instantiates a frozen value as a fresh family, reserving its logical
    /// cost and identities without building any entity.
    fn instantiate(&mut self, frozen: &FrozenRef) -> Result<PageValue, HeapWriteError>;
    /// Classifies the bindings for carry in one union traversal with shared
    /// visited state, skipping it when the data-only certificate suffices.
    fn classify_carry(&mut self, bindings: &KeyedRoot) -> Result<CarryClassification, HeapFault>;
    /// Seals the carry a terminal cell selects into a session root.
    fn carry(&mut self, roots: CarryRoots) -> Result<SessionRoot, HeapFault>;
    /// Opens a carried session for the next cell: the same entity
    /// identities, nothing renamed or loaded.
    fn adopt(&mut self, session: &SessionRoot) -> Result<KeyedRoot, HeapFault>;
}
