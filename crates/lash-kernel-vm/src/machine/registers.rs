//! Initialized scratch survives a statement; its logical values do not.
use std::ops::Index;
use std::slice::SliceIndex;
mod source;

use super::logical_held::{Held, Projection};
use lash_kernel_doc::{Value, heap::HeapFault};

/// A region's bounded operand stack. Nested calls open disjoint windows;
/// returning (including a guard miss) releases their references. Capacity
/// is inline, never grows, and is reused across statements and iterations.
pub(crate) struct Arguments {
    values: [Value; Self::CAPACITY],
    extent: usize,
}

impl Arguments {
    pub(crate) const CAPACITY: usize = 64;

    pub(crate) fn new() -> Self {
        Self {
            values: [Value::Null; Self::CAPACITY],
            extent: 0,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn is_empty(&self) -> bool {
        self.extent == 0
    }

    /// Releases every window an earlier operation left open, so that a
    /// reused stack starts empty.
    #[inline(always)]
    pub(crate) fn reset(&mut self) {
        if self.extent != 0 {
            self.release(0);
        }
    }

    pub(crate) fn window(&mut self, count: usize) -> Option<std::ops::Range<usize>> {
        let end = self.extent.checked_add(count)?;
        if end > Self::CAPACITY {
            return None;
        }
        let window = self.extent..end;
        self.extent = end;
        Some(window)
    }

    pub(crate) fn put(&mut self, at: usize, value: Value) {
        self.values[at] = value;
    }

    pub(crate) fn release(&mut self, extent: usize) {
        self.values[extent..self.extent].fill(Value::Null);
        self.extent = extent;
    }
}

impl Default for Arguments {
    fn default() -> Self {
        Self::new()
    }
}

impl Index<std::ops::Range<usize>> for Arguments {
    type Output = [Value];

    fn index(&self, range: std::ops::Range<usize>) -> &Self::Output {
        &self.values[range]
    }
}

/// An exclusive bank borrow authenticates the prevalidated register edit until
/// its accepted source journal commits it or abandons it.
pub(super) struct PreparedLeaf<'a> {
    registers: &'a mut Registers,
    leaf: source::Leaf,
}
impl PreparedLeaf<'_> {
    pub(super) fn owner(&self) -> lash_kernel_state::kernel_internals::logical::Owner {
        self.leaf.owner()
    }
    pub(super) fn bytes(&self) -> u64 {
        self.leaf.bytes()
    }
    pub(super) fn publish(self) {
        let (reg, value) = self.leaf.location();
        self.registers.put(reg.ix(), value);
        self.registers.source.publish_leaf(self.leaf);
    }
}

pub(super) struct PreparedConstruction<'a> {
    registers: &'a mut Registers,
    construction: source::Construction,
}
impl PreparedConstruction<'_> {
    pub(super) fn owner(&self) -> lash_kernel_state::kernel_internals::logical::Owner {
        self.construction.owner()
    }
    pub(super) fn owners(&self) -> Vec<lash_kernel_state::kernel_internals::logical::Owner> {
        self.construction.owners()
    }
    pub(super) fn values(&self) -> Vec<Value> {
        self.construction.values()
    }
    pub(super) fn publish(
        self,
        value: Value,
        holding: lash_kernel_state::kernel_internals::logical::Holding,
    ) {
        for reg in self.construction.registers() {
            self.registers.take(reg.ix(), Value::Null);
        }
        let output = self.construction.reg_index();
        self.registers.put(output, value);
        self.registers
            .source
            .publish_construction(self.construction, value, holding);
    }
}

#[derive(Default)]
pub(super) struct Registers {
    values: Vec<Value>,
    written: Vec<bool>,
    extent: usize,
    /// Static registers of the statement being stepped, including its Pin
    /// operands. Computed by lowering, independent of writes at run time.
    kill: usize,
    owns_values: bool,
    source: source::Operands,
    #[cfg(test)]
    pub(super) initializations: usize,
}

/// Values safe to retain physically after the logical window is released:
/// those that name no arena object, which a collection would not see.
#[inline(always)]
fn reusable(value: &Value) -> bool {
    value.handle().is_none()
}

impl Registers {
    pub(super) fn source_context(&mut self, roots: Option<super::logical_held::BankRoots>) {
        self.source.context(roots);
    }
    pub(super) fn source_entry_roots(
        &mut self,
        lifecycle: lash_kernel_state::kernel_internals::logical::Lifecycle,
    ) {
        self.source.entry_roots(lifecycle);
    }
    pub(super) fn take_source_arguments(
        &mut self,
        start: usize,
        count: usize,
    ) -> Vec<lash_kernel_state::kernel_internals::logical::Owner> {
        self.source.take_arguments(start, count)
    }
    /// The statement's static kill set is the dense interval the lowering
    /// gives it. Calls put their live roles in frames before this set dies.
    pub(super) fn statement(&mut self, registers: u32) {
        self.kill = self.kill.max(registers as usize);
        self.window(registers as usize);
    }

    /// Releases the prepared set at every exit of its statement's step.
    #[inline]
    pub(super) fn kill_statement(&mut self) {
        self.source.clear();
        let end = std::mem::take(&mut self.kill).min(self.extent);
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| {
            work.register_slots_cleared += end as u64;
            work.register_slots_initialized += end as u64;
        });
        self.clear_range(0..end);
        self.extent = 0;
        self.owns_values = false;
    }

    /// Exposes the same Null-filled logical extent as a fresh Vec would,
    /// without reinitializing its previously initialized physical slots.
    /// Resume roles may already have been installed before the op loop.
    pub(super) fn window(&mut self, extent: usize) {
        self.extent = self.extent.max(extent);
        if self.values.len() >= extent {
            return;
        }
        #[cfg(test)]
        {
            self.initializations += extent - self.values.len();
        }
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| {
            work.register_slots_initialized += (extent - self.values.len()) as u64
        });
        self.values.resize(extent, Value::Null);
        self.written.resize(extent, false);
    }

    /// Empties a source statement's operand range.
    pub(super) fn clear_range(&mut self, range: std::ops::Range<usize>) {
        let end = range.end.min(self.extent);
        let start = range.start.min(end);
        if start == end {
            return;
        }
        self.values[start..end].fill(Value::Null);
        self.written[start..end].fill(false);
    }

    #[inline(always)]
    pub(super) fn put(&mut self, out: usize, value: Value) {
        if out >= self.extent {
            self.window(out + 1);
        }
        self.written[out] = true;
        if !reusable(&value) {
            self.owns_values = true;
        }
        self.values[out] = value;
    }

    /// Taking an operand leaves a logical Null, with its original cost.
    #[inline(always)]
    pub(super) fn take(&mut self, at: usize, replacement: Value) -> Value {
        std::mem::replace(&mut self.values[at], replacement)
    }

    pub(super) fn get(&self, at: usize) -> Option<&Value> {
        self.values[..self.extent].get(at)
    }

    /// Only publication needs unwritten slots to be materialized. Definite
    /// write verification allows op reads and native argument slices to use
    /// initialized storage directly; no earlier statement's value is read.
    pub(super) fn projection(&self) -> Vec<Value> {
        self.values[..self.extent]
            .iter()
            .enumerate()
            .map(|(at, value)| {
                if self.written[at] {
                    *value
                } else {
                    Value::Null
                }
            })
            .collect()
    }

    pub(super) fn clear(&mut self) {
        self.source.clear();
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::record(|work| work.register_slots_cleared += self.extent as u64);
        // Reference-bearing entries die at the original statement boundary,
        // even when a later statement never overwrites this physical slot.
        if self.owns_values {
            #[cfg(test)]
            {
                self.initializations += self.extent;
            }
            #[cfg(any(test, feature = "perf-witness"))]
            crate::perf_witness::record(|work| {
                work.register_slots_initialized += self.extent as u64
            });
            self.values[..self.extent].fill(Value::Null);
            self.owns_values = false;
        }
        self.written[..self.extent].fill(false);
        self.extent = 0;
        self.kill = 0;
    }

    pub(super) fn source_entry(
        &mut self,
        values: &[Value],
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        self.source.entry(values, projection)?;
        self.window(values.len());
        for (index, value) in values.iter().enumerate() {
            self.put(index, *value);
        }
        self.clear_range(values.len()..self.extent);
        self.extent = values.len();
        Ok(())
    }

    pub(super) fn prepare_source_construction(
        &mut self,
        stream: &crate::compile::Stream,
        pc: u32,
    ) -> Result<Option<PreparedConstruction<'_>>, HeapFault> {
        let Some(reg) = stream.ops.get(pc as usize).and_then(|op| op.writes()) else {
            return Ok(None);
        };
        if reg.ix() >= self.extent {
            return Err(HeapFault::Malformed(
                "source construction output is outside its admitted register window".into(),
            ));
        }
        let Some(construction) = self.source.prepare_construction(stream, pc)? else {
            return Ok(None);
        };
        Ok(Some(PreparedConstruction {
            registers: self,
            construction,
        }))
    }

    pub(super) fn prepare_source_leaf(
        &mut self,
        stream: &crate::compile::Stream,
        pc: u32,
        value: Value,
        projection: &Projection<'_>,
    ) -> Result<Option<PreparedLeaf<'_>>, HeapFault> {
        let Some(reg) = stream.ops.get(pc as usize).and_then(|op| op.writes()) else {
            return Ok(None);
        };
        if reg.ix() >= self.extent {
            return Err(HeapFault::Malformed(
                "source leaf register is outside its admitted window".into(),
            ));
        }
        let Some(leaf) = self.source.prepare_leaf(stream, pc, value, projection)? else {
            return Ok(None);
        };
        Ok(Some(PreparedLeaf {
            registers: self,
            leaf,
        }))
    }

    pub(super) fn source_begin(
        &mut self,
        stream: &crate::compile::Stream,
        stmt: &crate::compile::Stmt,
        pc: u32,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        self.source
            .begin(stream, stmt, pc, &self.values, projection)
    }

    pub(super) fn source_atoms_begin(
        &mut self,
        site: &lash_kernel_value::sites::Site,
    ) -> Result<(), HeapFault> {
        self.source.begin_atoms(site)
    }

    pub(super) fn source_atom(
        &mut self,
        index: usize,
        value: Value,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        self.source.atom(index, value, projection)
    }

    pub(super) fn source_complete_joins(
        &mut self,
        stream: &crate::compile::Stream,
        pc: u32,
    ) -> Result<(), HeapFault> {
        self.source.complete_joins(stream, pc)
    }

    pub(super) fn source_complete(
        &mut self,
        stream: &crate::compile::Stream,
        pc: u32,
        projection: &Projection<'_>,
    ) -> Result<(), HeapFault> {
        self.source.complete(stream, pc, &self.values, projection)
    }

    pub(super) fn take_source_role(
        &mut self,
        reg: lash_kernel_exec::Reg,
        site: &lash_kernel_value::sites::Site,
        projection: &Projection<'_>,
    ) -> Result<Held, HeapFault> {
        if self.source.active {
            self.source.take(reg, site)
        } else {
            let value =
                self.values.get(reg.ix()).copied().ok_or_else(|| {
                    HeapFault::Malformed("continuation register is missing".into())
                })?;
            projection.hold(value)
        }
    }

    pub(super) fn source_bytes(
        &self,
        arena: &lash_kernel_state::arena::Arena,
    ) -> Result<u64, HeapFault> {
        if self.source.owns_values() {
            return self.source.bytes();
        }
        self.values[..self.extent]
            .iter()
            .zip(&self.written)
            .filter(|(_, written)| **written)
            .try_fold(0_u64, |sum, (value, _)| {
                sum.checked_add(arena.logical_local(value)?)
                    .ok_or_else(|| HeapFault::Malformed("entry holding cost overflow".into()))
            })
    }

    /// Active stream operands are source occurrences. Outside a stream, an
    /// action may still be assembling arguments in explicitly written slots.
    pub(super) fn source_projection(&self) -> Vec<Value> {
        if self.source.owns_values() {
            let mut values = Vec::new();
            self.source.each_value(&mut |value| values.push(*value));
            values
        } else {
            self.values[..self.extent]
                .iter()
                .zip(&self.written)
                .filter_map(|(value, written)| written.then_some(*value))
                .collect()
        }
    }

    pub(super) fn each_source_value(&self, visit: &mut dyn FnMut(&Value)) {
        self.source.each_value(visit);
    }
}

impl<I: SliceIndex<[Value]>> Index<I> for Registers {
    type Output = I::Output;

    fn index(&self, index: I) -> &Self::Output {
        // The verified op stream reads only values written in this step.
        // The physical slice is initialized even outside the logical extent.
        &self.values[index]
    }
}

impl From<Vec<Value>> for Registers {
    fn from(values: Vec<Value>) -> Self {
        let extent = values.len();
        let owns_values = values.iter().any(|value| !reusable(value));
        Self {
            values,
            written: vec![true; extent],
            extent,
            kill: 0,
            owns_values,
            source: source::Operands::default(),
            #[cfg(test)]
            initializations: 0,
        }
    }
}

#[cfg(test)]
mod laws {
    use crate::laws::embedder::{Embedder, Setup};
    use crate::{Layout, Machine};
    use lash_kernel_doc::TaskId;

    /// K-BND-003: nested operand windows cannot overwrite their caller or
    /// grow past admitted capacity, and released holdings cannot survive
    /// into the next call or a collection.
    #[test]
    fn argument_windows_are_bounded_and_release_their_holdings() {
        use super::Arguments;
        use lash_kernel_doc::{Value, arena::ArenaHandle};

        let mut args = Arguments::new();
        let outer = args.window(2).unwrap();
        let held = Value::List(ArenaHandle::new(7, 3));
        args.put(outer.start, held);
        args.put(outer.start + 1, Value::Int(42));
        let inner = args.window(Arguments::CAPACITY - 2).unwrap();
        args.put(inner.start, held);
        assert!(args.window(1).is_none());
        assert!(args.window(usize::MAX).is_none());
        assert!(
            matches!(&args[outer.clone()], [Value::List(handle), Value::Int(42)] if *handle == ArenaHandle::new(7, 3))
        );
        args.release(inner.start);
        assert!(
            matches!(&args[outer], [Value::List(handle), Value::Int(42)] if *handle == ArenaHandle::new(7, 3))
        );
        let reused = args.window(Arguments::CAPACITY - 2).unwrap();
        assert!(
            args[reused]
                .iter()
                .all(|value| matches!(value, Value::Null))
        );
        args.release(0);
        assert!(args.values.iter().all(|value| value.handle().is_none()));
        assert_eq!(
            args.window(Arguments::CAPACITY),
            Some(0..Arguments::CAPACITY)
        );
    }

    /// Seam E / K-BND-003: each statement's step relinquishes its
    /// register holdings before the driver observes the step's result.
    #[test]
    fn a_statement_releases_its_register_holdings_on_every_exit() {
        for layout in [Layout(0), Layout(1), Layout(3)] {
            for body in [
                "let held = ([1, 2], [3, 4])",
                "print ([1, 2], [3, 4])",
                "let held = (perform 0([1, 2]) as Any, [3, 4])",
                "let held = [1, 2][false]",
            ] {
                let mut embedder = Embedder::with(
                    &format!("main {{ {body} return null }}"),
                    Setup {
                        layout,
                        ..Setup::default()
                    },
                );
                let machine = &mut embedder.machine;
                let exe = std::sync::Arc::clone(&machine.exe);
                // Bypass run_slice's later cleanup: the step owns its kill
                // set, including raise and park exits.
                machine.step(TaskId(0), &mut embedder.world, &exe).unwrap();
                assert!(
                    machine.regs.projection().is_empty(),
                    "register holdings survived {body:?} under {layout:?}"
                );
                assert!(
                    machine.regs.source.is_empty(),
                    "source operands survived a statement exit"
                );
                assert!(
                    machine
                        .regs
                        .values
                        .iter()
                        .all(|value| value.handle().is_none())
                );
                assert!(machine.pins.values.is_empty());
                let _ = machine.meters();
            }
        }
    }
}
