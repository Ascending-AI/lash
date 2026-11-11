//! A block head's trace (`bytecode::trace`), run on the prepaid path.
//!
//! Each micro-op does what the dispatcher does at its op on a prepaid,
//! unobserved block, where the exact step is skipped. A micro-op that
//! cannot gives the dispatcher its op before any effect, and the
//! dispatcher runs it under every rule. Only a head's micro-op enters a
//! block; a call ends its block, so the op after an inline call is a
//! head, and every other micro-op runs on the reservation its trace's
//! last head made.

use lash_kernel_doc::Value;

use super::{Flow, Run, Vm, fused};
use crate::bytecode::program::{Program, Then, Unit};
use crate::bytecode::trace::{Micro, NONE};
use crate::heap::Heap;

#[cfg(test)]
thread_local! {
    /// The micro-ops run on this thread, for the laws: proof that a law's
    /// run took a trace.
    static RAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The micro-ops run on this thread so far.
#[cfg(test)]
pub(crate) fn ran() -> u64 {
    RAN.with(std::cell::Cell::get)
}

impl<H: Heap> Vm<H> {
    /// Runs trace `n` of `unit`, the running frame's (`top`), whose locals
    /// start at `base`. Gives the op the dispatcher runs next, or `None`
    /// where a frame was entered. An error leaves the frame where the dispatcher's
    /// would.
    #[inline(never)]
    pub(super) fn trace(
        &mut self,
        program: &Program,
        unit: &Unit,
        top: usize,
        n: u32,
        base: usize,
    ) -> Run<Option<usize>> {
        let mut at = unit.traces[n as usize].start as usize;
        loop {
            let micro = unit.micros[at];
            at += 1;
            // Counted as the dispatcher counts its op, once the micro-op
            // has not given it back.
            macro_rules! ran {
                () => {
                    #[cfg(test)]
                    self.witness(unit, micro)
                };
            }
            if !matches!(
                micro,
                Micro::CaptureGet { .. }
                    | Micro::CellGet { .. }
                    | Micro::Fused { .. }
                    | Micro::Inline { .. }
                    | Micro::Exit { .. }
            ) {
                ran!();
            }
            debug_assert!(
                matches!(micro, Micro::Block { .. } | Micro::Exit { .. }) || !self.exact,
                "a trace runs on the prepaid path"
            );
            match micro {
                Micro::Block { pc, block } => {
                    // The block before ended: what it reserved is spent.
                    self.meter.leave();
                    let block = &unit.blocks[block as usize];
                    self.exact = self.watch.is_some() || !self.meter.enter(block.charge);
                    if self.exact {
                        return Ok(Some(pc as usize + 1));
                    }
                }
                Micro::Pass { n, .. } => {
                    let local = &mut self.cx.stack[base + usize::from(n)];
                    if let Value::Int(count) = *local {
                        *local = Value::Int(count.saturating_add(1));
                    }
                }
                Micro::Local { n, .. } => self.push(self.cx.stack[base + usize::from(n)]),
                Micro::Const { n, .. } => self.push(self.konst(n)),
                Micro::SetLocal { n, .. } => {
                    let value = self.pop();
                    self.cx.stack[base + usize::from(n)] = value;
                }
                Micro::Copy { from, to, .. } => {
                    self.cx.stack[base + usize::from(to)] = self.cx.stack[base + usize::from(from)];
                }
                Micro::Pop { .. } => {
                    self.pop();
                }
                Micro::CaptureGet { pc, n } => {
                    let Some(value) = self
                        .capture(n)
                        .ok()
                        .and_then(|cell| self.heap.cell_get(cell).ok())
                    else {
                        return Ok(Some(pc as usize));
                    };
                    ran!();
                    self.push(value);
                }
                Micro::CellGet { pc, n } => {
                    let Some(value) = self
                        .cell_at(n)
                        .ok()
                        .and_then(|cell| self.heap.cell_get(cell).ok())
                    else {
                        return Ok(Some(pc as usize));
                    };
                    ran!();
                    self.push(value);
                }
                Micro::Fused { n, next, .. } => {
                    let fused = &unit.fused[n as usize];
                    debug_assert!(fused.entry.is_none(), "a traced run counts no entry");
                    let len = self.cx.stack.len();
                    let args = self.fused_args(fused, base, len);
                    let answer = match self.fused_words(fused, args) {
                        Some(value) => Ok(Some(value)),
                        None => self.fused_other(program, unit, fused, args),
                    };
                    let value = match answer {
                        Ok(Some(value)) => {
                            ran!();
                            value
                        }
                        // The native's ordinary answer, or its call, from
                        // the run's call on, as the dispatcher runs it.
                        Ok(None) => {
                            ran!();
                            return self.fused_slow(program, unit, n, base, args, len, top);
                        }
                        Err(flow) => {
                            ran!();
                            self.cx.frames[top].pc = match flow {
                                Flow::Cold { .. } => fused::rerun_from(fused),
                                _ => fused.call + 1,
                            };
                            return Err(flow);
                        }
                    };
                    self.cx.stack.truncate(len + usize::from(fused.pushes) - 2);
                    match fused.then {
                        Then::Push => self.push(value),
                        Then::SetLocal(k) => self.cx.stack[base + usize::from(k)] = value,
                        Then::Pop => {}
                        Then::Branch { to, looping } => match value {
                            Value::Bool(true) => {}
                            Value::Bool(false) if next == NONE => {
                                return Ok(Some(to as usize));
                            }
                            Value::Bool(false) => at = next as usize,
                            // The answer and its fee are the run's: the
                            // branch raises after them, as its op does.
                            _ => {
                                self.cx.frames[top].pc = fused.call + 2;
                                return Err(self.condition_error(looping));
                            }
                        },
                    }
                }
                Micro::Inline { pc, n } => {
                    if !self.trace_inline(unit, top, pc, n)? {
                        return Ok(Some(pc as usize));
                    }
                    ran!();
                }
                Micro::Branch {
                    pc,
                    to,
                    looping,
                    next,
                } => match self.pop() {
                    Value::Bool(true) => {}
                    Value::Bool(false) if next == NONE => return Ok(Some(to as usize)),
                    Value::Bool(false) => at = next as usize,
                    _ => {
                        self.cx.frames[top].pc = pc + 1;
                        return Err(self.condition_error(looping));
                    }
                },
                Micro::Jump { to, next, .. } if next == NONE => {
                    return Ok(Some(to as usize));
                }
                Micro::Jump { next, .. } => at = next as usize,
                Micro::Exit { pc } => return Ok(Some(pc as usize)),
            }
        }
    }

    /// Inline call `n` of `unit` at `pc`, in the running frame (`top`),
    /// where a fold holds and is billed whole: gives whether it ran. Any
    /// other call the dispatcher runs whole, from its checks on, so a
    /// region or a fold's general path is entered in its loop alone. Out
    /// of line: what it reads, a trace does not hold through its other
    /// micro-ops.
    #[inline(never)]
    fn trace_inline(&mut self, unit: &Unit, top: usize, pc: u32, n: u32) -> Run<bool> {
        let site = &unit.inlines[n as usize];
        let call = &unit.calls[site.call as usize];
        debug_assert!(call.entry.is_none(), "a traced call counts no entry");
        if site.steps && self.meter.spent() {
            return Ok(false);
        }
        self.cx.frames[top].pc = pc + 1;
        if let Some(checked) = &call.checked {
            self.check_arguments(checked, usize::from(call.argc))?;
        }
        let (start, end) = site.folds;
        self.inline_fast(unit, n, &unit.folds[start as usize..end as usize])
    }

    /// Counts a micro-op for the laws, and the steps its ops start as the
    /// dispatcher counts them.
    #[cfg(test)]
    fn witness(&self, unit: &Unit, micro: Micro) {
        RAN.with(|ran| ran.set(ran.get() + 1));
        let pc = micro.pc() as usize;
        let ops = if let Micro::Copy { .. } = micro { 2 } else { 1 };
        for pc in pc..pc + ops {
            if unit.steps && unit.step[pc] {
                crate::perf_witness::record(|work| work.vm_steps += 1);
            }
        }
    }
}
