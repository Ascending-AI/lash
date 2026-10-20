//! A merge pass's step, walked natively (FIG-6948): the region's body as
//! [`crate::compile::plan_regions::merge`] recognised it, its forms
//! compiled with the region's (FIG-6947).
//!
//! The step opens each statement the region's pass opens, in its order,
//! and writes each variable it writes. Its integer bookkeeping and its
//! decisions run here; its item reads and writes, the comparator's
//! application and the coercion of its answer run as the region's own
//! compiled forms, on the plan's registers: a coercion its prepared entry
//! answers is answered as the plan's offer answers it there. So the plan
//! stands, raises and halts exactly where the region's pass would.

use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{PlanContext, PlanExit};

use super::{Operand, Went, operand};
use crate::compile::plan_regions::merge::Merge;
use crate::compile::plan_regions::{Step, StepKind};
use crate::machine::plans::PlanRun;

/// The largest index or length a step keeps native: far from where an
/// integer sum leaves a word.
const NATIVE: i64 = 1 << 48;

/// A merge's element write, `set out[dest] = list.get(sorted, at)`,
/// compiled as the region's member write: its three operands in the plan's
/// order, the target's register, the value's, and whether its operands go
/// to their registers as they are evaluated.
pub(super) struct Placed<const S: bool> {
    order: [Operand<S>; 3],
    target: u32,
    src: u32,
    placed: bool,
}

/// A merge pass compiled for the plan's variant.
pub(super) type Merged<const S: bool> = Merge<Operand<S>, Placed<S>>;

/// `merge`'s forms compiled: `None` where an element write is no member
/// write, for the region's pass to run.
pub(super) fn compiled<const S: bool>(merge: Merge) -> Option<Merged<S>> {
    merge.map(operand::<S>, |step: Step| match step.kind {
        StepKind::Store {
            order,
            target,
            src,
            placed,
        } => Some(Placed {
            order: order.map(operand::<S>),
            target,
            src,
            placed,
        }),
        _ => None,
    })
}

/// The merge's integer variables, as the step reads them on entry.
struct Counters {
    dest: i64,
    end: i64,
    mid: i64,
    left: i64,
    right: i64,
    width: i64,
    n: i64,
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
    /// One step of `merge`'s loop, the element already bound: `None`,
    /// having done nothing, when its variables are not what the native
    /// step takes, for the region's pass to run it.
    pub(super) fn merge_step(&mut self, merge: &Merged<SHARED>) -> Result<Option<Went>, PlanExit> {
        let Some(mut at) = self.counters(merge) else {
            return Ok(None);
        };
        // if same(dest, end) { ... }
        self.open_statement_of(0)?;
        if at.dest == at.end {
            at.mid = (at.dest + at.width).min(at.n);
            self.open_statement_of(0)?;
            self.write_int(merge.mid, at.mid);
            at.end = (at.mid + at.width).min(at.n);
            self.open_statement_of(0)?;
            self.write_int(merge.end, at.end);
            at.left = at.dest;
            self.open_statement_of(0)?;
            self.write_int(merge.left, at.left);
            at.right = at.mid;
            self.open_statement_of(0)?;
            self.write_int(merge.right, at.right);
        }
        // let from_right = false
        self.open_statement_of(0)?;
        self.bind_value(merge.from_right, Value::Bool(false))?;
        // if num.lt(left, mid) { if num.lt(right, end) { ... } } else { ... }
        self.open_statement_of(0)?;
        let from_right = if at.left < at.mid {
            self.open_statement_of(0)?;
            if at.right < at.end {
                self.compare(merge, [at.left, at.right])?
            } else {
                false
            }
        } else {
            self.open_statement_of(0)?;
            self.machine
                .put(self.slots + merge.from_right as usize, Value::Bool(true));
            true
        };
        // if from_right { set out[dest] = ... } else { ... }
        self.open_statement_of(0)?;
        let (place, index, slot) = if from_right {
            (&merge.places[0], &mut at.right, merge.right)
        } else {
            (&merge.places[1], &mut at.left, merge.left)
        };
        self.open_statement_of(0)?;
        self.place(place, (merge.out, at.dest), (merge.sorted, *index))?;
        *index += 1;
        let next = *index;
        self.open_statement_of(0)?;
        self.write_int(slot, next);
        // set dest = num.add(dest, 1)
        self.open_statement_of(0)?;
        self.write_int(merge.dest, at.dest + 1);
        for slot in merge.declared.1.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Some(Went::Through))
    }

    /// The comparison: the two items, the comparator's application, the
    /// coercion of its answer and the test of the order. Whether the right
    /// item goes first.
    fn compare(&mut self, merge: &Merged<SHARED>, reads: [i64; 2]) -> Result<bool, PlanExit> {
        // let first = list.get(sorted, left); let second = ... right
        for ((slot, item), read) in merge.operands.into_iter().zip(&merge.items).zip(reads) {
            self.open_statement_of(0)?;
            let value = match self.read(merge.sorted, read)? {
                Some(value) => value,
                None => item.get(self)?,
            };
            self.bind_value(slot, value)?;
        }
        // let output = apply callback(context, <operands>)
        let (span, output, applied) = &merge.apply;
        self.open_statement_of(*span)?;
        let answer = applied.get(self)?;
        self.bind_value(*output, answer)?;
        // let order = invoke coerce(output)
        let (span, order, invoked) = &merge.coerce;
        self.open_statement_of(*span)?;
        let order_value = invoked.get(self)?;
        self.bind_value(*order, order_value)?;
        // if num.lt(0, order) { set from_right = true }
        self.open_statement_of(0)?;
        let positive = match order_value {
            Value::Float(order) => 0.0 < order.get(),
            Value::Int(order) => 0 < order,
            _ => match merge.positive.get(self)? {
                Value::Bool(holds) => holds,
                other => return Err(self.not_bool(merge.positive.reg, other)),
            },
        };
        if positive {
            self.open_statement_of(0)?;
            self.machine
                .put(self.slots + merge.from_right as usize, Value::Bool(true));
        }
        for slot in merge.declared.0.iter().rev() {
            self.unbind(*slot);
        }
        Ok(positive)
    }

    /// The element write `place` of the merge, `set target[index] =
    /// list.get(source, at)`, as the region's pass writes it: the item
    /// read from the arena at the read's verdict points, and written in
    /// place where the target is a resident list with that position or
    /// the next; otherwise by the region's own form.
    fn place(
        &mut self,
        place: &Placed<SHARED>,
        (target, index): (u32, i64),
        (source, at): (u32, i64),
    ) -> Result<(), PlanExit> {
        let written = self.machine.regs[self.slots + target as usize];
        if let Some(item) = self.read(source, at)?
            && self.write_list(written, Value::Int(index), item)?
        {
            return Ok(());
        }
        self.store(&place.order, (place.target, place.src), place.placed)
    }

    /// `list.get(source, at)` of the list in plan variable `source` where
    /// the arena holds it resident with that position, at the read's
    /// verdict points: `None`, having done nothing, otherwise, for the
    /// region's own read. What it reads stays held by the list.
    #[inline(always)]
    fn read(&mut self, source: u32, at: i64) -> Result<Option<Value>, PlanExit> {
        let Value::List(list) = self.machine.regs[self.slots + source as usize] else {
            return Ok(None);
        };
        let item = self
            .machine
            .arena
            .borrow()
            .item(list, at as u64)
            .ok()
            .flatten();
        if item.is_some() {
            let verdicts = self.machine.verdict_points();
            self.held(verdicts)?;
        }
        Ok(item)
    }

    /// Writes integer `value` to plan variable `slot`.
    fn write_int(&mut self, slot: u32, value: i64) {
        self.machine
            .put(self.slots + slot as usize, Value::Int(value));
    }

    /// The merge's integer variables, when each is one the native step
    /// keeps and its lists are lists.
    fn counters(&self, merge: &Merged<SHARED>) -> Option<Counters> {
        let regs = &self.machine.regs;
        let int = |slot: u32| match regs[self.slots + slot as usize] {
            Value::Int(value) if (0..NATIVE).contains(&value) => Some(value),
            _ => None,
        };
        let list = |slot: u32| matches!(regs[self.slots + slot as usize], Value::List(_));
        (list(merge.sorted) && list(merge.out)).then_some(())?;
        Some(Counters {
            dest: int(merge.dest)?,
            end: int(merge.end)?,
            mid: int(merge.mid)?,
            left: int(merge.left)?,
            right: int(merge.right)?,
            width: int(merge.width)?,
            n: int(merge.n)?,
        })
    }
}
