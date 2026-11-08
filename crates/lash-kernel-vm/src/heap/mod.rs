//! The core's heap (cutover contract §2): the objects a run holds, the
//! logical memory schedule that admits them, and the collector.
//!
//! Objects are [`lash_kernel_doc::arena::Body`] values in a non-moving slot
//! vector, so natives keep their zero-copy `Args`/`Nursery` views. The
//! collector is mark-sweep from the machine's roots. Owner after handoff
//! H0: L2 (module-map §3).

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

    /// Places `object`, already admitted.
    fn create(&mut self, object: NewObject) -> Result<Value, NativeError>;

    /// Installs what a native call built, in reservation order.
    fn install(&mut self, built: Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError>;

    /// An error value of `kind` and `message` with null data (`K-ERR-001`).
    fn error(&mut self, kind: &str, message: &str) -> Result<Value, NativeError>;

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
}
