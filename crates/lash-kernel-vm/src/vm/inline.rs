//! Inline regions (cutover contract §1, §3): the logical activation of a
//! library body whose guard path runs in its caller's frame.
//!
//! [`Op::Inline`](crate::bytecode::op::Op::Inline) makes the activation a
//! call's entry makes, all but the frame; `Op::Leave` completes it as a
//! return does; `Op::Deopt` pushes the body's frame where the guard path
//! ends; a raise completes it at the body's position before unwinding on.
//! A region's fold (`bytecode::fold`) runs once its activation is made.

use lash_kernel_doc::Value;

use super::frames::{Activation, Frame};
use super::{Flow, Restart, Run, Stop, Vm, fault};
use crate::Bound;
use crate::bytecode::program::{Callee, FOLD_STEPS, Fold, Program, Slot, Step, Unit};
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
        let site = unit.inlines[n as usize];
        let call = &unit.calls[site.call as usize];
        let Callee::Unit { charge, .. } = call.callee else {
            return Err(fault("an inline site of no body call"));
        };
        let argc = usize::from(call.argc);
        let bill = self.owed(charge, argc);
        self.meter.leave();
        self.exact = true;
        if self.cx.frames.len() >= self.call_depth as usize {
            return Err(Flow::from(Stop::bound(
                self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
            )));
        }
        self.admit(self.frames[site.callee as usize])?;
        let locals = self.program.units[site.callee as usize].locals;
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
        Ok(base)
    }

    /// What fold `fold` of the region whose activation's locals start at
    /// `base` returns: each step read with no effect, `None` where a guard
    /// does not hold or a step declines, so that the region runs.
    pub(super) fn folded(&self, program: &Program, fold: &Fold, base: usize) -> Option<Value> {
        let mut made = [Value::Absent; FOLD_STEPS];
        let mut count = 0;
        let read = |made: &[Value; FOLD_STEPS], slot: Slot| match slot {
            Slot::Local(n) => self.cx.stack[base + usize::from(n)],
            Slot::Const(n) => self.constants[n as usize],
            Slot::Made(n) => made[usize::from(n)],
        };
        for &step in &fold.steps[..usize::from(fold.len)] {
            let value = match step {
                Step::Answer { native, args, argc } => {
                    let args = [read(&made, args[0]), read(&made, args[1])];
                    match self.answer(program, native, false, &args[..usize::from(argc)])? {
                        Ok((value, None)) => value,
                        _ => return None,
                    }
                }
                Step::Guard { value, holds } => {
                    if !matches!(read(&made, value), Value::Bool(found) if found == holds) {
                        return None;
                    }
                    continue;
                }
                Step::Index { target, index } => {
                    let arena = self.heap.arena();
                    match (read(&made, target), read(&made, index)) {
                        (Value::List(object) | Value::Tuple(object), Value::Int(at)) => {
                            arena.item(object, u64::try_from(at).ok()?).ok()??
                        }
                        _ => return None,
                    }
                }
            };
            made[count] = value;
            count += 1;
        }
        #[cfg(test)]
        HELD.with(|held| held.set(held.get() + 1));
        Some(read(&made, fold.result))
    }

    /// The inline region returns the top value, as its body's frame
    /// would: the completion read first, so a cold read runs the op again,
    /// then the activation released and its bill settled.
    pub(super) fn leave(&mut self) -> Run<()> {
        self.meter.leave();
        self.exact = true;
        let Some(activation) = self.cx.inline else {
            return Err(fault("a region left with no activation"));
        };
        let [value] = self.operands();
        let completion = self.completion(activation.bill, Some(&value))?;
        self.cx.inline = None;
        self.heap.release(self.frames[activation.callee as usize]);
        self.settle(completion)?;
        self.cx.stack.truncate(activation.base as usize);
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
        self.cx.frames.push(Frame {
            unit: activation.callee,
            pc: to,
            base: activation.base,
            closure: None,
            bill: activation.bill,
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
        let unit = &self.program.units[frame.unit as usize];
        let resume = unit.inlines[activation.site as usize].at + 1;
        self.jump(resume);
        self.heap.release(self.frames[activation.callee as usize]);
        self.settle(completion)?;
        self.cx.stack.truncate(activation.base as usize);
        Ok(())
    }
}
