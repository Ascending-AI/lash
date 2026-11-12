//! Inline regions (cutover contract §1, §3): the logical activation of a
//! library body whose guard path runs in its caller's frame.
//!
//! [`Op::Inline`](crate::bytecode::op::Op::Inline) makes the activation a
//! call's entry makes, all but the frame; `Op::Leave` completes it as a
//! return does; `Op::Deopt` pushes the body's frame where the guard path
//! ends; a raise completes it at the body's position before unwinding on.
//! A region's fold (`bytecode::fold`) runs once its activation is made,
//! and one that holds completes it as the region's `Op::Leave` would.

use std::cell::Cell;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::InlineOperation;
use lash_kernel_doc::plan::Table;
use lash_kernel_state::arena::{Key, PropertySlot};

use super::frames::{Activation, Frame};
use super::{Flow, Restart, Run, Stop, Vm, fault};
use crate::Bound;
use crate::bytecode::program::{Callee, FOLD_STEPS, Fold, Slot, Step, Unit};
use crate::heap::Heap;

#[cfg(test)]
thread_local! {
    /// The folds that held on this thread, for the laws: proof that a
    /// law's run took a fold.
    static HELD: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The folds that held on this thread so far.
#[cfg(test)]
pub(crate) fn held() -> u64 {
    HELD.with(std::cell::Cell::get)
}

impl<H: Heap> Vm<H> {
    /// Enters inline site `n` of `unit`, the running frame's, whose
    /// arguments are the top of the stack: the body's activation as a
    /// call's `enter` makes it, its depth checked at the call, its frame
    /// admitted and its locals laid out, all but the frame. Gives where
    /// its locals start.
    pub(super) fn inline(&mut self, unit: &Unit, n: u32) -> Run<usize> {
        let (charge, argc) = self.activate(unit, n)?;
        Ok(self.lay_out(unit, n, charge, argc))
    }

    /// Enters inline site `n` of `unit`, whose body steps, as the body's
    /// own frame: its region bills nothing, so a slice due in it is due
    /// at the call, and the slice lands at the body's first statement.
    #[cold]
    #[inline(never)]
    pub(super) fn enter_stepped(&mut self, unit: &Unit, n: u32) -> Run<()> {
        self.inline(unit, n)?;
        self.deopt(unit, 0)
    }

    /// Enters inline site `n` of `unit` as [`Vm::inline`] does, and runs
    /// the region's folds `folds`, its guard paths, until one holds: at
    /// most one does, for no two paths take the same branch both ways. A
    /// fold that holds completes the activation where its `Op::Leave`
    /// would, giving `None`:
    /// the frame stands there while its completion is billed, so a cold
    /// read or a refusal finds the region laid out at its leave with the
    /// value on the stack, as the leave's own dispatch would. Otherwise
    /// the activation is laid out and the region runs: gives where its
    /// locals start.
    pub(super) fn inline_folded(
        &mut self,
        unit: &Unit,
        n: u32,
        folds: &[Fold],
    ) -> Run<Option<usize>> {
        if unit.inlines[n as usize].element && self.inline_element_fast(unit, n)? {
            return Ok(None);
        }
        let (charge, argc) = self.activate(unit, n)?;
        let base = self.cx.stack.len() - argc;
        let mut held = None;
        for fold in folds {
            if let Some(value) = self.folded(fold, base) {
                held = Some((fold, value));
                break;
            }
        }
        // A call held to a typed result takes a fold's value only where
        // it plainly fits; otherwise the region runs to its leave, which
        // checks it.
        let held = held.filter(|(_, value)| {
            let call = &unit.calls[unit.inlines[n as usize].call as usize];
            call.checked
                .as_ref()
                .is_none_or(|checked| super::fits::plainly_fits(*value, &checked.signature.result))
        });
        let Some((fold, value)) = held else {
            return Ok(Some(self.lay_out(unit, n, charge, argc)));
        };
        if let Some(frame) = self.cx.frames.last_mut() {
            frame.pc = fold.leave + 1;
        }
        self.meter.leave();
        self.exact = true;
        // The bill reads the arguments where they stand: no step wrote
        // them.
        let units = match charge.filter(|_| !self.off) {
            Some(charge) => match self.formula(charge, &self.cx.stack[base..], Some(&value)) {
                Ok(units) => Some(units),
                Err(flow) => {
                    self.lay_out(unit, n, Some(charge), argc);
                    self.push(value);
                    return Err(flow);
                }
            },
            None => None,
        };
        let callee = unit.inlines[n as usize].callee;
        self.heap.release(self.frame_cost(callee));
        self.cx.stack.truncate(base);
        self.push(value);
        if let Some(units) = units {
            self.fee(units)
                .map_err(|flow| self.billed_in(flow, callee))?;
        }
        Ok(None)
    }

    /// [`Self::inline_folded`] where a fold holds and nothing it does can
    /// refuse, read cold or be observed, as one bill: the activation's
    /// frame admitted and released nets to nothing, and its completion
    /// fee fits the limit, so it is billed whole. Gives whether it ran;
    /// where it did not, nothing is done.
    pub(super) fn inline_fast(&mut self, unit: &Unit, n: u32, folds: &[Fold]) -> Run<bool> {
        self.inline_answer_fast(unit, n, |vm, base| {
            folds
                .iter()
                .find_map(|fold| vm.folded(fold, base).map(|value| (value, fold.leave)))
        })
    }

    fn inline_element_fast(&mut self, unit: &Unit, n: u32) -> Run<bool> {
        let site = unit.inlines[n as usize];
        let fold = &unit.folds[site.folds.0 as usize];
        let Step::Element { target, index } = fold.steps[0] else {
            return Ok(false);
        };
        self.inline_answer_fast(unit, n, |vm, base| {
            let read = |slot: Slot| match slot {
                Slot::Local(n) => vm.cx.stack[base + usize::from(n)],
                Slot::Const(n) => vm.konst(n),
                Slot::Made(_) => Value::Absent,
            };
            vm.element(read(target), read(index))
                .map(|value| (value, fold.leave))
        })
    }

    fn inline_answer_fast(
        &mut self,
        unit: &Unit,
        n: u32,
        answer: impl FnOnce(&Self, usize) -> Option<(Value, u32)>,
    ) -> Run<bool> {
        let site = unit.inlines[n as usize];
        let call = &unit.calls[site.call as usize];
        let Callee::Unit { charge, .. } = call.callee else {
            return Ok(false);
        };
        if self.watch.is_some() || self.depth_full() {
            return Ok(false);
        }
        let cost = self.frame_cost(site.callee);
        if self
            .heap
            .held()
            .checked_add(cost)
            .is_none_or(|held| held > self.heap.bound())
        {
            return Ok(false);
        }
        let base = self.cx.stack.len() - usize::from(call.argc);
        let Some((value, leave)) = answer(self, base) else {
            return Ok(false);
        };
        if let Some(checked) = &call.checked
            && !super::fits::plainly_fits(value, &checked.signature.result)
        {
            return Ok(false);
        }
        let units = match charge.filter(|_| !self.off) {
            Some(charge) => match self.formula(charge, &self.cx.stack[base..], Some(&value)) {
                Ok(units) => Some(units),
                Err(_) => return Ok(false),
            },
            None => None,
        };
        // The block's reservation is spent at the call: the fee is billed
        // on the exact path, where it fits whole.
        if units.is_some_and(|units| !self.meter.fits(units)) {
            return Ok(false);
        }
        if let Some(frame) = self.cx.frames.last_mut() {
            frame.pc = leave + 1;
        }
        self.meter.leave();
        self.exact = true;
        self.cx.stack.truncate(base);
        self.push(value);
        if let Some(units) = units {
            self.fee(units)?;
        }
        Ok(true)
    }

    /// The activation of inline site `n` of `unit` as a call's entry
    /// makes it: its depth checked at the call and its frame admitted.
    /// Gives the call's completion formula and argument count.
    fn activate(&mut self, unit: &Unit, n: u32) -> Run<(Option<u32>, usize)> {
        let site = unit.inlines[n as usize];
        let call = &unit.calls[site.call as usize];
        let Callee::Unit { charge, .. } = call.callee else {
            return Err(fault("an inline site of no body call"));
        };
        self.meter.leave();
        self.exact = true;
        if self.depth_full() {
            return Err(Flow::from(Stop::bound(
                self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
            )));
        }
        self.admit(self.frame_cost(site.callee))?;
        Ok((charge, usize::from(call.argc)))
    }

    /// Lays out the activated inline site `n` of `unit` over its `argc`
    /// arguments: its completion owed and its locals placed where its
    /// frame's would be. Gives where they start.
    fn lay_out(&mut self, unit: &Unit, n: u32, charge: Option<u32>, argc: usize) -> usize {
        let site = unit.inlines[n as usize];
        let bill = self.owed(charge, argc);
        let locals = self.program.unit(site.callee).locals;
        let base = self.cx.stack.len() - argc;
        self.cx
            .stack
            .resize(base + usize::from(locals.max(argc as u16)), Value::Absent);
        self.cx.inline = Some(Activation {
            site: n,
            callee: site.callee,
            base: base as u32,
            bill,
        });
        base
    }

    /// What fold `fold` of the region whose arguments start at `base`
    /// returns: each step read with no effect, `None` where a guard does
    /// not hold or a step declines, so that the region runs. A region
    /// calls only natives with no body, so an answer holds no unit of
    /// depth.
    #[inline(never)]
    fn folded(&self, fold: &Fold, base: usize) -> Option<Value> {
        if let Some(words) = fold.words {
            let read = |slot: Slot| match slot {
                Slot::Local(n) => self.cx.stack[base + usize::from(n)],
                Slot::Const(n) => self.konst(n),
                Slot::Made(_) => Value::Absent,
            };
            if let (Value::Int(a), Value::Int(b)) =
                (read(words.operands[0]), read(words.operands[1]))
            {
                let value = lash_kernel_doc::scalar::ints(words.operation, a, b);
                #[cfg(test)]
                if value.is_some() {
                    HELD.with(|held| held.set(held.get() + 1));
                }
                return value;
            }
        }
        let mut made = [Value::Absent; FOLD_STEPS];
        let mut count = 0;
        let read = |made: &[Value; FOLD_STEPS], slot: Slot| match slot {
            Slot::Local(n) => self.cx.stack[base + usize::from(n)],
            Slot::Const(n) => self.konst(n),
            Slot::Made(n) => made[usize::from(n)],
        };
        for step in &fold.steps[..usize::from(fold.len)] {
            let value = match *step {
                Step::Answer {
                    operation,
                    args,
                    argc,
                } => {
                    let argc = usize::from(argc);
                    let mut values = [Value::Absent; 2];
                    for (value, slot) in values.iter_mut().zip(args).take(argc) {
                        *value = read(&made, slot);
                    }
                    self.immediate(operation, &values[..argc])?
                }
                Step::KindIs { value, kind } => {
                    Value::Bool(InlineOperation::kind_is(&read(&made, value), kind)?)
                }
                Step::Guard { value, holds } => {
                    if !matches!(read(&made, value), Value::Bool(found) if found == holds) {
                        return None;
                    }
                    continue;
                }
                Step::Lookup { target, key } => {
                    let (target, key) = (read(&made, target), read(&made, key));
                    if !InlineOperation::Membership(Table::Map).accepts(&[target, key]) {
                        return None;
                    }
                    let Value::Map(object) = target else {
                        return None;
                    };
                    let arena = self.heap.arena();
                    match key {
                        Value::Text(text) => {
                            let cell = self.lookups.cell(step);
                            let (value, proof) = arena
                                .table_read(object, cell.get(), &Key::text(&text))
                                .ok()?;
                            cell.set(proof);
                            value?
                        }
                        key => arena.table_get(object, &key).ok()??,
                    }
                }
                Step::Index { target, index } => {
                    let arena = self.heap.arena();
                    match (read(&made, target), read(&made, index)) {
                        (Value::List(object) | Value::Tuple(object), Value::Int(at)) => {
                            arena.item(object, u64::try_from(at).ok()?).ok()??
                        }
                        (Value::Map(object), key) => arena.table_get(object, &key).ok()??,
                        _ => return None,
                    }
                }
                Step::Element { target, index } => {
                    self.element(read(&made, target), read(&made, index))?
                }
            };
            made[count] = value;
            count += 1;
        }
        #[cfg(test)]
        HELD.with(|held| held.set(held.get() + 1));
        Some(read(&made, fold.result))
    }

    #[inline]
    fn element(&self, target: Value, index: Value) -> Option<Value> {
        let (Value::List(object), Value::Int(index)) = (target, index) else {
            return None;
        };
        let index = u64::try_from(index).ok()?;
        let arena = self.heap.arena();
        i64::try_from(arena.len(object).ok()?).ok()?;
        arena.item(object, index).ok()?
    }

    /// The inline region returns the top value, as its body's frame
    /// would: the completion read first, so a cold read runs the op again,
    /// then the activation released and its bill settled.
    /// A call held to a typed result raises `entry_type_mismatch` there
    /// instead, from the caller after the call, as a return does.
    pub(super) fn leave(&mut self, unit: &Unit, n: u32) -> Run<()> {
        self.meter.leave();
        self.exact = true;
        let Some(activation) = self.cx.inline else {
            return Err(fault("a region left with no activation"));
        };
        let [value] = self.operands();
        let site = unit.inlines[n as usize];
        let unfit = match &unit.calls[site.call as usize].checked {
            Some(checked) => self.result_unfit(checked, value)?,
            None => None,
        };
        let completion = self.completion(activation.bill, Some(&value))?;
        self.cx.inline = None;
        self.heap.release(self.frame_cost(activation.callee));
        self.settle(completion, activation.callee)?;
        self.cx.stack.truncate(activation.base as usize);
        if let Some(unfit) = unfit {
            self.jump(site.at + 1);
            return Err(self.typed_error("entry_type_mismatch", &unfit));
        }
        self.push(value);
        Ok(())
    }

    /// The inline region leaves its guard path at its body's op `to`: the
    /// body's frame is pushed there over the activation's locals and
    /// operands, its depth checked and frame admitted at the call already,
    /// and the running frame stands after the call.
    pub(super) fn deopt(&mut self, unit: &Unit, to: u32) -> Run<()> {
        let Some(activation) = self.cx.inline.take() else {
            return Err(fault("a region left with no activation"));
        };
        let resume = unit.inlines[activation.site as usize].at + 1;
        self.jump(resume);
        let link = self.link_of(activation.callee, self.running_link());
        let depth = self.cx.depth() + u32::from(self.program.unit(activation.callee).call);
        self.cx.frames.push(Frame {
            unit: activation.callee,
            pc: to,
            base: activation.base,
            closure: None,
            bill: activation.bill,
            link,
            depth,
        });
        Ok(())
    }

    /// A raise in an inline region leaves the body's activation at its
    /// position first, as a raise leaves its frame: its completion billed
    /// with a result of size 0, its frame released. The running frame then
    /// stands after the call. A completion that reads a cold chunk resumes
    /// the unwind with the activation in place.
    pub(super) fn unwind_inline(&mut self, activation: Activation, value: Value) -> Run<()> {
        let completion = match self.completion(activation.bill, None) {
            Ok(completion) => completion,
            Err(flow) => {
                if let Flow::Cold { .. } = flow {
                    self.restart = Some(Restart::Unwind(value));
                }
                return Err(flow);
            }
        };
        self.cx.inline = None;
        let Some(frame) = self.cx.frames.last() else {
            return Err(fault("an inline region with no frame"));
        };
        let unit = self.program.unit(frame.unit);
        let resume = unit.inlines[activation.site as usize].at + 1;
        self.jump(resume);
        self.heap.release(self.frame_cost(activation.callee));
        self.settle(completion, activation.callee)?;
        self.cx.stack.truncate(activation.base as usize);
        Ok(())
    }
}

/// The position proofs of folded short-text key reads, by the read's
/// step: a small table its steps share by their place in the program.
/// Each proof the arena checks whole against the table it reads, its
/// object, revision and key, so a proof another step or table left is
/// only a miss.
#[derive(Default)]
pub(crate) struct Lookups([Cell<Option<PropertySlot>>; LOOKUPS]);

const LOOKUPS: usize = 32;

impl Lookups {
    /// The proof cell of the read at `step`.
    #[inline]
    fn cell(&self, step: &Step) -> &Cell<Option<PropertySlot>> {
        let at = (std::ptr::from_ref(step) as usize).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        &self.0[at >> (usize::BITS - LOOKUPS.trailing_zeros())]
    }
}
