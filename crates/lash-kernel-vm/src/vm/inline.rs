//! Inline regions (cutover contract §1, §3): the logical activation of a
//! library body whose guard path runs in its caller's frame.
//!
//! [`Op::Inline`](crate::bytecode::op::Op::Inline) makes the activation a
//! call's entry makes, all but the frame; `Op::Leave` completes it as a
//! return does; `Op::Deopt` pushes the body's frame where the guard path
//! ends; a raise completes it at the body's position before unwinding on.

use lash_kernel_doc::Value;

use super::frames::{Activation, Frame};
use super::{Flow, Restart, Run, Stop, Vm, fault};
use crate::Bound;
use crate::bytecode::program::{Callee, Unit};
use crate::heap::Heap;

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
