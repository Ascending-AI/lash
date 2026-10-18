//! Initialized scratch survives a statement; its logical values do not.
use std::ops::Index;
use std::slice::SliceIndex;

use lash_kernel_doc::Value;

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

impl Index<std::ops::Range<usize>> for Arguments {
    type Output = [Value];

    fn index(&self, range: std::ops::Range<usize>) -> &Self::Output {
        &self.values[range]
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
    /// The statement's static kill set is the dense interval the lowering
    /// gives it. Calls put their live roles in frames before this set dies.
    pub(super) fn statement(&mut self, registers: u32) {
        self.kill = self.kill.max(registers as usize);
        self.window(registers as usize);
    }

    /// Releases the prepared set at every exit of its statement's step.
    pub(super) fn kill_statement(&mut self) {
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

    /// Where the logical window ends: a plan's registers start here.
    pub(super) fn extent(&self) -> usize {
        self.extent
    }

    /// Ends the logical window at `extent`, dropping what the registers
    /// past it hold: a plan's registers when it returns.
    pub(super) fn release(&mut self, extent: usize) {
        if extent >= self.extent {
            return;
        }
        self.values[extent..self.extent].fill(Value::Null);
        self.written[extent..self.extent].fill(false);
        self.extent = extent;
    }

    /// Empties registers `range`: a plan's operands when its statement
    /// ends, as a body's are.
    pub(super) fn clear_range(&mut self, range: std::ops::Range<usize>) {
        let end = range.end.min(self.extent);
        let start = range.start.min(end);
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
                "let held = (perform echo([1, 2]) as Any, [3, 4])",
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
