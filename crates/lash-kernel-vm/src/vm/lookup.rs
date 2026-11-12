//! A resident map read followed by a word helper, on their original
//! activation, completion and block boundaries (`K-CHG-002`, `K-BND-003`).

use lash_kernel_doc::Value;
use lash_kernel_state::arena::Key;

use super::{Run, Vm};
use crate::bytecode::program::{Unit, WordUpdate};
use crate::heap::Heap;

#[cfg(test)]
thread_local! {
    static RAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FRAME_SIZES: std::cell::Cell<Option<(u64, u64, u64)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn ran() -> u64 {
    RAN.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn frame_sizes() -> Option<(u64, u64, u64)> {
    FRAME_SIZES.with(std::cell::Cell::get)
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn without<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            DISABLED.set(self.0);
        }
    }
    let _restore = Restore(DISABLED.replace(true));
    f()
}

impl<H: Heap> Vm<H> {
    #[inline(never)]
    pub(super) fn lookup_words(&mut self, unit: &Unit, n: u32, top: usize) -> Run<bool> {
        if self.watch.is_some()
            || self.sites
            || self.off
            || self.depth_full()
            || self.value_depth <= 1
        {
            return Ok(false);
        }
        #[cfg(any(test, feature = "testing"))]
        if DISABLED.get() {
            return Ok(false);
        }
        let recipe = unit.lookup_words[n as usize];
        let get_cost = self.frame_cost(recipe.get_callee);
        let word_cost = match recipe.update {
            WordUpdate::Body { callee, .. } => self.frame_cost(callee),
            WordUpdate::Native { native } => {
                if self.short_of(self.program.native(native).elided.max(1)) {
                    return Ok(false);
                }
                0
            }
        };
        #[cfg(test)]
        FRAME_SIZES.with(|sizes| {
            if sizes.get().is_none() {
                sizes.set(Some((self.heap.held(), get_cost, word_cost)));
            }
        });
        let cost = get_cost.max(word_cost);
        if self
            .heap
            .held()
            .checked_add(cost)
            .is_none_or(|held| held > self.heap.bound())
        {
            return Ok(false);
        }
        let base = self.cx.stack.len() - 2;
        let [Value::Int(left), Value::Map(object), Value::Text(key)] = self.cx.stack[base - 1..]
        else {
            return Ok(false);
        };
        if key != recipe.key {
            return Ok(false);
        }
        let lookup = &unit.folds[recipe.lookup as usize];
        let proof = self.lookups.cell(&lookup.steps[0]);
        let Ok((Some(Value::Int(word)), slot)) =
            self.heap
                .arena()
                .table_read(object, proof.get(), &Key::text(&key))
        else {
            return Ok(false);
        };
        proof.set(slot);
        let Some(result) = lash_kernel_doc::scalar::ints(recipe.operation, left, word) else {
            return Ok(false);
        };
        let get_fee = recipe.get_fees[usize::from(word != 0)];
        let large = matches!(result, Value::Int(value) if value != 0);
        let word_fee = recipe.word_fees
            [usize::from(left != 0) | usize::from(word != 0) << 1 | usize::from(large) << 2];
        let after = match recipe.update {
            WordUpdate::Body { after, .. } => after,
            WordUpdate::Native { .. } => 0,
        };
        let total = get_fee
            .checked_add(recipe.between)
            .and_then(|sum| sum.checked_add(word_fee))
            .and_then(|sum| sum.checked_add(after));
        if total.is_none_or(|total| !self.meter.fits_before_slice(total)) {
            return Ok(false);
        }
        // There is no load, allocation or observable step left. Each body
        // frame fits separately; a native answer keeps its depth guard.
        // The two completion fees remain distinct atomic events.
        self.meter.leave();
        self.exact = true;
        self.cx.frames[top].pc = lookup.leave + 1;
        self.fee(get_fee)?;
        let fits = self.meter.enter(Some(recipe.between));
        debug_assert!(fits);
        match recipe.update {
            WordUpdate::Body {
                completion, after, ..
            } => {
                self.meter.leave();
                self.cx.frames[top].pc = completion;
                self.fee(word_fee)?;
                let fits = self.meter.enter(Some(after));
                debug_assert!(fits);
            }
            WordUpdate::Native { .. } => {
                // The call and store share this block with the remaining
                // caller ops. Its reservation must stay outstanding.
                self.cx.frames[top].pc = recipe.resume - 1;
                self.fee(word_fee)?;
            }
        }
        self.exact = false;
        self.cx.stack.truncate(base - 1);
        let base = self.cx.frames[top].base as usize;
        self.cx.stack[base + usize::from(recipe.destination)] = result;
        self.cx.frames[top].pc = recipe.resume;
        #[cfg(test)]
        RAN.with(|ran| ran.set(ran.get() + 1));
        Ok(true)
    }
}
