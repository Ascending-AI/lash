//! The first slot heap: the kernel-state [`Arena`]'s slot vector of
//! [`lash_kernel_doc::arena::Body`] values, with its canonical keys and
//! tables, admitted by a logical schedule and collected by mark-sweep from
//! the machine's roots.
//!
//! The schedule here is the skeleton's: `fixed + unit × logical length` per
//! object, independent of capacity and allocation history (contract §2,
//! R7). L2's `heap/meter.rs` fixes the constants.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::Objects;
use lash_kernel_doc::arena::native::{NativeError, NewObject};
use lash_kernel_state::arena::{Arena, ArenaSemantics, Visit};

use super::{Heap, MemoryRefused};

/// Schedule units an object costs before its contents.
const FIXED: u64 = 32;
/// Schedule units per value an object holds.
const PER_VALUE: u64 = 16;

/// Schedule units of a closure over `captures` cells.
pub(crate) fn closure_units(captures: usize) -> u64 {
    FIXED.saturating_add(PER_VALUE.saturating_mul(captures as u64))
}

/// Schedule units of a cell.
pub(crate) const CELL: u64 = FIXED + PER_VALUE;

/// An error value: its kind, message and data (`K-ERR-001`).
pub(crate) const ERROR: u64 = FIXED + 3 * PER_VALUE;

/// The schedule units `object` costs once placed.
pub(crate) fn schedule(object: &NewObject) -> u64 {
    let values = |n: usize| PER_VALUE.saturating_mul(n as u64);
    FIXED.saturating_add(match object {
        NewObject::List(values_) | NewObject::Tuple(values_) | NewObject::Set(values_) => {
            values(values_.len())
        }
        NewObject::Map(pairs) | NewObject::Record(pairs) => values(pairs.len().saturating_mul(2)),
        NewObject::LongText(text) => text.len() as u64,
        NewObject::Bytes(bytes) => bytes.len() as u64,
        NewObject::BigInt(int) | NewObject::Timestamp(int) => int.to_string().len() as u64,
        NewObject::Function(_) => 0,
        NewObject::Error { .. } => values(3),
    })
}

/// A slot heap bounded by `bound` schedule units.
pub(crate) struct SlotHeap {
    arena: Arena,
    /// Each slot's schedule units, by slot index, while its object lives.
    units: Vec<u64>,
    held: u64,
    bound: u64,
}

impl SlotHeap {
    pub(crate) fn new(bound: u64) -> Self {
        Self {
            arena: Arena::new(),
            units: Vec::new(),
            held: 0,
            bound,
        }
    }

    /// Records what slot `handle` costs; its admission came first.
    fn place(&mut self, handle: ArenaHandle, units: u64) {
        let index = handle.index() as usize;
        if self.units.len() <= index {
            self.units.resize(index + 1, 0);
        }
        self.units[index] = units;
    }
}

impl Heap for SlotHeap {
    #[inline]
    fn objects(&self) -> &Objects {
        self.arena.objects()
    }

    #[inline]
    fn semantics(&self) -> ArenaSemantics<'_> {
        self.arena.semantics()
    }

    #[inline]
    fn admit(&mut self, units: u64) -> Result<(), MemoryRefused> {
        match self.held.checked_add(units) {
            Some(held) if held <= self.bound => {
                self.held = held;
                Ok(())
            }
            _ => Err(MemoryRefused {
                held: self.held,
                wanted: units,
            }),
        }
    }

    #[inline]
    fn held(&self) -> u64 {
        self.held
    }

    fn create(&mut self, object: NewObject) -> Result<Value, NativeError> {
        let units = schedule(&object);
        let value = self.arena.create(object)?;
        if let Some(handle) = value.handle() {
            self.place(handle, units);
        }
        Ok(value)
    }

    fn install(&mut self, built: Vec<(ArenaHandle, NewObject)>) -> Result<(), NativeError> {
        let placed: Vec<(ArenaHandle, u64)> = built
            .iter()
            .map(|(handle, object)| (*handle, schedule(object)))
            .collect();
        self.arena.install(built)?;
        for (handle, units) in placed {
            self.place(handle, units);
        }
        Ok(())
    }

    fn error(&mut self, kind: &str, message: &str) -> Result<Value, NativeError> {
        let value = self.arena.error(kind, message, Value::Null)?;
        if let Some(handle) = value.handle() {
            self.place(handle, ERROR);
        }
        Ok(value)
    }

    fn cell(&mut self, value: Value) -> ArenaHandle {
        let handle = self.arena.new_cell(value);
        self.place(handle, CELL);
        handle
    }

    fn cell_get(&self, cell: ArenaHandle) -> Result<Value, NativeError> {
        self.arena.cell(cell)
    }

    fn cell_set(&mut self, cell: ArenaHandle, value: Value) -> Result<(), NativeError> {
        self.arena.cell_set(cell, value)
    }

    fn closure(
        &mut self,
        identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
        captures: Vec<ArenaHandle>,
    ) -> ArenaHandle {
        let units = closure_units(captures.len());
        let handle = self.arena.new_closure(identity, captures);
        self.place(handle, units);
        handle
    }

    #[inline]
    fn arena(&self) -> &Arena {
        &self.arena
    }

    #[inline]
    fn arena_mut(&mut self) -> &mut Arena {
        &mut self.arena
    }

    fn collect(&mut self, roots: &mut dyn FnMut(&mut Visit<'_>)) {
        self.arena.collect_cycles(roots);
        let objects = self.arena.objects();
        let mut held = 0u64;
        for (index, units) in self.units.iter_mut().enumerate() {
            if objects.handle_at(index as u32).is_some() {
                held = held.saturating_add(*units);
            } else {
                *units = 0;
            }
        }
        self.held = held;
    }
}
