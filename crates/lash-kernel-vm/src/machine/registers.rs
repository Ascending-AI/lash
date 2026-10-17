//! Initialized scratch survives a statement; its logical values do not.
use std::ops::Index;
use std::slice::SliceIndex;

use lash_kernel_doc::Value;
use lash_kernel_doc::heap::IntRef;
use num_traits::ToPrimitive;

#[derive(Default)]
pub(super) struct Registers {
    values: Vec<Value>,
    written: Vec<bool>,
    extent: usize,
    owns_values: bool,
    #[cfg(test)]
    pub(super) initializations: usize,
}

/// Values safe to retain physically after the logical window is released.
/// In particular a large inline integer still owns a shared allocation.
#[inline(always)]
fn reusable(value: &Value) -> bool {
    match value {
        Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_) => true,
        Value::Int(IntRef::Inline(integer)) => integer.to_i64().is_some(),
        _ => false,
    }
}

impl Registers {
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
        let old = std::mem::replace(&mut self.values[out], value);
        if matches!(
            old,
            Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_)
        ) {
            std::mem::forget(old);
        }
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
                    value.clone()
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
            owns_values,
            #[cfg(test)]
            initializations: 0,
        }
    }
}
