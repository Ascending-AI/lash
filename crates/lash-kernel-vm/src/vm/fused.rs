//! A fused run's out-of-line parts (`bytecode::fuse`): its arguments,
//! the condition error its branch raises, and the run from its call on
//! where the dispatch loop's in-place answer does not take it.

use lash_kernel_doc::Value;

use super::{Flow, Run, Vm, fault};
use crate::bytecode::program::{Callee, Fused, Operand, Program, Then, Unit};
use crate::heap::Heap;

/// The frame `pc` a cold read in a fused run's answer leaves: past the
/// run's own op, so the load runs the whole run again. The run reads its
/// pushes in place and never makes them, so its call op alone would run
/// short of its arguments.
#[cold]
pub(super) fn rerun_from(fused: &Fused) -> u32 {
    fused.call - u32::from(fused.pushes) + 1
}

impl<H: Heap> Vm<H> {
    /// A branch's condition is no bool (`K-EVAL-005`).
    #[cold]
    #[inline(never)]
    pub(super) fn condition_error(&mut self, looping: bool) -> Flow {
        let message = if looping {
            "a `while` condition must be a bool"
        } else {
            "an `if` condition must be a bool"
        };
        self.error("type_error", message)
    }

    /// A fused run's two call arguments: its pushes read where they are,
    /// below them what the stack holds.
    #[inline(always)]
    pub(super) fn fused_args(&self, fused: &Fused, base: usize, len: usize) -> [Value; 2] {
        let [first, second] = fused.operands;
        match fused.pushes {
            2 => [self.operand(first, base), self.operand(second, base)],
            1 => [self.cx.stack[len - 1], self.operand(first, base)],
            _ => [self.cx.stack[len - 2], self.cx.stack[len - 1]],
        }
    }

    #[inline(always)]
    fn operand(&self, operand: Operand, base: usize) -> Value {
        match operand {
            Operand::Local(n) => self.cx.stack[base + usize::from(n)],
            Operand::Const(n) => self.konst(n),
        }
    }

    /// A fused run the word path did not take, from its call on: the
    /// native's ordinary answer and its fee, then the op that takes it;
    /// or, where the native declines, the pushes and the call. Gives the
    /// next op, or `None` when a frame was entered. The running frame is
    /// `top`; `len` is the stack's height before the run.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fused_slow(
        &mut self,
        program: &Program,
        unit: &Unit,
        n: u32,
        base: usize,
        args: [Value; 2],
        len: usize,
        top: usize,
    ) -> Run<Option<usize>> {
        let fused = &unit.fused[n as usize];
        let mut pc = fused.call as usize + 1;
        self.cx.frames[top].pc = pc as u32;
        let Callee::Native { native, charged } = unit.calls[fused.site as usize].callee else {
            return Err(fault("a fused call of no native"));
        };
        let words = match (fused.words, args) {
            (Some(form), [Value::Int(a), Value::Int(b)]) => {
                lash_kernel_doc::scalar::ints(form.operation, a, b).map(|value| {
                    let fee = form.fee.filter(|_| !self.off).map(|table| {
                        let large = matches!(value, Value::Int(word) if word != 0);
                        table[usize::from(a != 0)
                            | usize::from(b != 0) << 1
                            | usize::from(large) << 2]
                    });
                    (value, fee)
                })
            }
            _ => None,
        };
        let (value, fee) = match words {
            Some(answered) => answered,
            None => match self.answer(program, native, charged, &args) {
                Some(Err(cold @ Flow::Cold { .. })) => {
                    self.cx.frames[top].pc = rerun_from(fused);
                    return Err(cold);
                }
                Some(answer) => answer?,
                None => {
                    // No inline answer: the pushes, then the call.
                    for &pushed in &fused.operands[..usize::from(fused.pushes)] {
                        self.push(self.operand(pushed, base));
                    }
                    self.call(program, unit, fused.site)?;
                    return Ok(None);
                }
            },
        };
        self.cx.stack.truncate(len + usize::from(fused.pushes) - 2);
        // A fee the block's reservation does not cover is billed with the
        // answer on the stack, where the call left it.
        let value = match fee {
            Some(units) if !self.meter.prepay(units) => {
                self.push(value);
                self.fee(units)?;
                self.pop()
            }
            _ => value,
        };
        match fused.then {
            Then::Push => self.push(value),
            Then::SetLocal(n) => {
                pc += 1;
                self.cx.stack[base + usize::from(n)] = value;
            }
            Then::Pop => pc += 1,
            Then::Branch { to, looping } => {
                pc += 1;
                match value {
                    Value::Bool(true) => {}
                    Value::Bool(false) => pc = to as usize,
                    _ => {
                        self.cx.frames[top].pc = pc as u32;
                        return Err(self.condition_error(looping));
                    }
                }
            }
        }
        Ok(Some(pc))
    }
}
