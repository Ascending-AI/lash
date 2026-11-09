//! The core's heap (cutover contract §2): the objects a run holds, the
//! logical memory schedule that admits them, and the collector.
//!
//! Objects are [`lash_kernel_doc::arena::Body`] values in a non-moving slot
//! vector, so natives keep their zero-copy `Args`/`Nursery` views. The
//! collector is mark-sweep from the machine's roots. Owner after handoff
//! H0: L2 (module-map §3).

pub(crate) mod large;
pub(crate) mod meter;
pub(crate) mod open;
pub(crate) mod seal;
pub(crate) mod slots;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NativeError, NewObject};
use lash_kernel_doc::arena::{ArenaHandle, Objects};
use lash_kernel_state::arena::{Arena, ArenaSemantics, Visit};

/// An allocation the memory bound does not admit, after one collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryRefused {
    /// What the run held, and what it asked for, in schedule units.
    pub held: u64,
    pub wanted: u64,
    /// The bound it would pass.
    pub bound: u64,
}

/// What the dispatcher needs of a heap. Every allocation is admitted
/// before it is placed; when admission fails the caller collects from its
/// roots and asks once more before it refuses (contract §2).
pub(crate) trait Heap {
    /// The bodies, as a native call borrows them.
    fn objects(&self) -> &Objects;

    /// The guest semantics over the bodies (`K-VAL-020`, `K-KEY-002`).
    fn semantics(&self) -> ArenaSemantics<'_>;

    /// Admits `units` more of the memory schedule, or refuses.
    fn admit(&mut self, units: u64) -> Result<(), MemoryRefused>;

    /// What the run holds under the schedule.
    fn held(&self) -> u64;

    /// The memory bound, in schedule units (`Bounds::memory`).
    fn bound(&self) -> u64;

    /// Raises the memory bound by `units`, as a memory limiter grew it.
    fn grow(&mut self, units: u64);

    /// Returns `units` admitted for a holding that has ended: a transient,
    /// a frame that left, a raw outcome decoded.
    fn release(&mut self, units: u64);

    /// Settles object `handle` after a write that was admitted for
    /// `admitted` more: the holding becomes exactly what its body holds.
    fn settle(&mut self, handle: ArenaHandle, admitted: u64);

    /// Places `object`, already admitted.
    fn create(&mut self, object: NewObject) -> Result<Value, NativeError>;

    /// Installs what a native call built, in reservation order.
    fn install(&mut self, built: Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError>;

    /// What the error value of `kind`, `message` and `data` holds once
    /// built: its texts, its data object and itself.
    fn error_units(&self, kind: &str, message: &str, data: Value) -> Result<u64, NativeError>;

    /// An error value of `kind`, `message` and `data` (`K-ERR-001`),
    /// already admitted for [`Heap::error_units`].
    fn error(&mut self, kind: &str, message: &str, data: Value) -> Result<Value, NativeError>;

    /// A new cell holding `value` (`K-CLO-001`).
    fn cell(&mut self, value: Value) -> ArenaHandle;

    fn cell_get(&self, cell: ArenaHandle) -> Result<Value, NativeError>;

    fn cell_set(&mut self, cell: ArenaHandle, value: Value) -> Result<(), NativeError>;

    /// A closure of `identity` over `captures` (`K-CLO-001`), admitted.
    fn closure(
        &mut self,
        identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: Vec<ArenaHandle>,
    ) -> ArenaHandle;

    /// The objects, for the member operations and the measures
    /// (`K-CHG-005`) the dispatcher reads in place.
    fn arena(&self) -> &Arena;

    /// The objects, for a member write the dispatcher makes in place.
    fn arena_mut(&mut self) -> &mut Arena;

    /// Frees every object `roots` does not reach.
    fn collect(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>));

    /// Decodes chunk `chunk` of the cold object `object` from the run's
    /// directory, its referents becoming proxies, so the operation that
    /// answered [`lash_kernel_doc::Retry::Cold`] can run again. Admission
    /// collects from `roots` and asks once more before it refuses. Called
    /// with no view of the arena live.
    ///
    /// # Errors
    ///
    /// [`LoadError::Memory`] when the bound does not admit what the object
    /// holds after a collection; [`LoadError::Store`] when its row cannot
    /// be read: an infrastructure failure, never retried.
    fn load(
        &mut self,
        object: ArenaHandle,
        chunk: u32,
        roots: &mut dyn FnMut(&mut Visit<'_>),
    ) -> Result<(), LoadError>;
}

/// Why a cold object could not be loaded.
#[derive(Clone, Debug)]
pub(crate) enum LoadError {
    /// The memory bound, after one collection: the guest's memory refusal.
    Memory(MemoryRefused),
    /// The store, a page's integrity or the lease: `MachineError::Host`.
    Store(lash_kernel_doc::heap::HeapFault),
}
