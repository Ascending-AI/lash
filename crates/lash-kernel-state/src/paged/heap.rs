//! S1: [`SemanticHeap`], the one deep module guest execution and
//! checkpoints use.
//!
//! The heap owns mutation, page residency, identity and accounting behind
//! this interface. It never lends `&mut` access to an object, a `Vec` of
//! values or a mutating iterator: every edit is a checked operation that
//! updates the collection path, its directory head, its logical-cost delta
//! and its identity-edge summary together, before the edit is readable.
//! It exposes values, collections and identities, never pages or digests
//! (except [`SemanticHeap::seal`]'s root).
//!
//! Reads take `&self`: page residency behind them is interior, single
//! threaded state (the heap is not `Sync`), so a visit can lend a pinned
//! page's elements while the cache stays usable.
//!
//! Error taxonomy, the same for every operation:
//!
//! - a guest error is a `Raised` value the operation's semantics define
//!   (`type_error`, `index_out_of_range`, an illegal key);
//! - a guest bound is [`HeapWriteError::Memory`], only after the census's
//!   exact refinement refused (S7);
//! - an infrastructure failure is a [`HeapFault`]: it ends the activation.

use std::ops::Range;

use super::{NameRef, SiteRef};
use lash_kernel_doc::heap::{
    EntityId, HeapFault, Identity, LogicalBytes, NativeHeap, PageRef, TextRef, TupleRef, Value,
};
pub use lash_kernel_doc::heap::{FieldName, NormalKey};

use super::census::TransientRoots;
use super::root::HeapRoot;
use super::source::PageSource;

/// The kinds of a heap entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    List,
    Map,
    Set,
    Record,
    Closure,
    /// A variable a closure shares with its defining scope (`K-CLO-001`).
    Variable,
}

/// A fresh entity's initial contents. Unlike a native's
/// `lash_kernel_doc::heap::Object`, the machine may allocate closures and
/// variables.
#[derive(Clone, Debug)]
pub enum ObjectInit {
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Set(Vec<Value>),
    Record(Vec<(TextRef, Value)>),
    Closure {
        /// The site of the closure expression: a semantic coordinate, never
        /// a compiled code index.
        site: SiteRef,
        captures: Vec<(NameRef, EntityId)>,
    },
    Variable(Value),
}

/// A closure as the machine reads it.
#[derive(Clone)]
pub struct ClosureView {
    pub site: SiteRef,
    pub captures: super::tree::Tree<(NameRef, EntityId)>,
}

/// A loop's position in a map or a set: the insertion sequence of the
/// entry it last visited (`K-ITER-003`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableCursor(pub u64);

/// The edit epoch of one activation: pages it copied are editable in place
/// until the next seal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditEpoch(pub u64);

/// What a seal may spend before it yields (host work, never guest charge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealBudget {
    pub pages: u64,
    pub bytes: u64,
}

/// A sealed heap: its root, ready to be named by a checkpoint root.
#[derive(Clone, Debug)]
pub struct SealedHeap {
    pub root: HeapRoot,
    /// How many pages this seal wrote.
    pub sealed_pages: u64,
}

/// Why a checked edit or allocation did not happen.
#[derive(Clone, Debug, thiserror::Error)]
pub enum HeapWriteError {
    /// A guest error of a kernel kind the operation raises, with null
    /// data; nothing changed.
    #[error("{kind}: {message}")]
    Raised { kind: &'static str, message: String },
    /// The memory bound refused, after exact refinement (S7).
    #[error("the run's memory bound refused an allocation")]
    Memory,
    #[error(transparent)]
    Fault(#[from] HeapFault),
}

/// The paged heap, as the machine drives it. Its native face is
/// [`NativeHeap`].
///
/// Implemented by the paged heap of `lash-kernel-state` (layer hp-heap,
/// FIG-6188); its accounting calls are [`AllocationCensus`](super::AllocationCensus)
/// (hp-memory, FIG-6189) and its lifecycle operations
/// [`HeapLifecycle`](super::HeapLifecycle) (hp-life, FIG-6192).
pub trait SemanticHeap: NativeHeap {
    /// Opens a sealed heap root for an activation: reads its root page and
    /// nothing else. Every later read faults in what it touches.
    ///
    /// # Errors
    ///
    /// A [`HeapFault`] reading the root.
    fn open(
        root: &HeapRoot,
        source: Box<dyn PageSource>,
        epoch: EditEpoch,
    ) -> Result<Self, HeapFault>
    where
        Self: Sized;

    /// An empty heap for a fresh run.
    fn fresh(source: Box<dyn PageSource>, epoch: EditEpoch) -> Self
    where
        Self: Sized;

    /// Places resident immutable descendants in bounded payload pages.
    /// Existing paged descriptors retain their references without reads.
    fn canonical_value(&mut self, value: Value) -> Result<Value, HeapWriteError>;

    fn kind(&self, entity: EntityId) -> Result<Option<ObjectKind>, HeapFault>;
    /// The value that names `entity`, by its kind; `None` for a variable.
    fn value_of(&self, entity: EntityId) -> Result<Option<Value>, HeapFault>;
    fn variable_get(&self, variable: EntityId) -> Result<Value, HeapFault>;
    fn closure(&self, closure: EntityId) -> Result<ClosureView, HeapFault>;
    /// The first entry of a map or a set after `after`, with its cursor
    /// and its first-written key.
    fn table_after(
        &self,
        table: EntityId,
        after: Option<TableCursor>,
    ) -> Result<Option<(TableCursor, Value)>, HeapFault>;
    /// How many live entries sit at or before `last`: a loop's position as
    /// a count (`K-ITER-002`).
    fn table_passed(&self, table: EntityId, last: Option<TableCursor>) -> Result<u64, HeapFault>;

    /// Allocates a fresh entity in the primary family. The census reserves
    /// its logical cost first.
    fn allocate_object(&mut self, init: ObjectInit) -> Result<EntityId, HeapWriteError>;
    /// Writes element `index`; `index == len` appends (`K-FORM-006`).
    fn list_write(
        &mut self,
        list: EntityId,
        index: u64,
        value: Value,
    ) -> Result<(), HeapWriteError>;
    fn list_remove(&mut self, list: EntityId, index: u64) -> Result<Value, HeapWriteError>;
    /// Writes `value` under `key`; an existing key keeps its first spelling
    /// and position (`K-KEY-003`).
    fn map_write(&mut self, map: EntityId, key: Value, value: Value) -> Result<(), HeapWriteError>;
    fn set_write(
        &mut self,
        set: EntityId,
        member: Value,
        present: bool,
    ) -> Result<(), HeapWriteError>;
    /// Removes a map entry or a set member; absence is no error.
    fn table_remove(&mut self, table: EntityId, key: &Value) -> Result<(), HeapWriteError>;
    fn record_write(
        &mut self,
        record: EntityId,
        field: FieldName<'_>,
        value: Value,
    ) -> Result<(), HeapWriteError>;
    fn record_remove(
        &mut self,
        record: EntityId,
        field: FieldName<'_>,
    ) -> Result<(), HeapWriteError>;
    fn variable_write(&mut self, variable: EntityId, value: Value) -> Result<(), HeapWriteError>;

    /// Guest equality of two values (`K-VAL` equality rules); can read
    /// pages.

    /// A legal key's normal form, or `None` when `value` is not a legal key
    /// (`K-KEY-001`).

    /// Every heap entity and task `value` names, through tuples and errors.
    fn edges_of(&self, value: &Value, out: &mut Vec<Identity>) -> Result<(), HeapFault>;
    /// The logical cost of `value` where it is held (the local metric).
    fn cost_of(&self, value: &Value) -> LogicalBytes;
    fn tuple_slice(
        &mut self,
        tuple: &TupleRef,
        range: Range<u64>,
    ) -> Result<TupleRef, HeapWriteError>;

    /// Reserves `bytes` of logical memory for the machine (frames, roots,
    /// a statement's temporaries). `transient` names every value the
    /// running statement holds outside the heap, in case the census must
    /// refine (S7).
    fn reserve_logical(
        &mut self,
        bytes: LogicalBytes,
        transient: &TransientRoots<'_>,
    ) -> Result<(), HeapWriteError>;

    /// Seals every editable page, children before parents, stopping at
    /// sealed links, within `budget`; `None` when the budget ran out
    /// first (call again). The live heap keeps working on the result.
    ///
    /// # Errors
    ///
    /// A [`HeapFault`] from the page port.
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedHeap>, HeapFault>;

    /// Seals and drops resident pages until residency fits the embedder's
    /// budgets: clean pages are dropped, dirty pages sealed first.
    fn evict(&mut self) -> Result<(), HeapFault>;

    /// The page reference of the current heap root, when sealed and
    /// unchanged since.
    fn sealed_root(&self) -> Option<PageRef>;
}
