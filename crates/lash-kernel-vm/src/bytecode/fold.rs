//! Folding: an inline region's guard path as one guarded op (cutover
//! contract §1, §3).
//!
//! A region still dispatches each op of its body: a list read's two
//! bound tests, its length and its index are a dozen dispatches around
//! one element read. The pass follows each region from its entry along
//! the branches of one path to an `Op::Leave`, carrying what each op
//! pushes as a fact about where its value comes from, and records the
//! path as a fold: the native answers (R8) it computes, the branches it
//! takes as guards, its index reads and the value it returns. An answer
//! step holds the operation the native's version declares, resolved
//! once here. A path reads a local only where it was written on the path
//! or the call passed it as an argument, so the fold needs no layout of
//! the body's locals.
//!
//! `Op::Inline` makes the activation as before, its depth checked and
//! its frame admitted, and then runs the fold. Every step reads and none
//! commits: an answer is a pure native answer, an index read finds an
//! element or declines. Where a guard does not hold or a step declines,
//! the activation is laid out, its completion owed, and the region runs
//! from its entry over the same state, so the body's own ops raise, read
//! cold or deoptimize as they would. A fold that holds completes the
//! activation at the region's `Op::Leave` with the value, as the body's
//! return does: the completion billed there, the frame released.
//!
//! The pass folds only ops that bill no event, pace no unit, test no
//! slice and show no point, and native calls that bill no fee, so
//! skipping them moves, merges or drops no charge, step or observation.
//! A fold holds no fact past its own run: nothing it reads is written
//! between its steps, and a write or an unknown call ends the path.
//! Selection follows the region's ops and each native's answer under its
//! declared version, never a name.
//!
//! It runs after validation, on validated code, before fusion.

use lash_kernel_doc::arena::native::InlineOperation;

use super::op::Op;
use super::program::{Callee, FOLD_STEPS, Fold, Program, Slot, Step, Unit};

/// Folds the guard path of every inline region of `program`.
pub(crate) fn fold(program: &mut Program) {
    for at in 0..program.units.len() {
        for n in 0..program.units[at].inlines.len() {
            let Some(fold) = derive(program, &program.units[at], n as u32) else {
                continue;
            };
            let unit = &mut program.units[at];
            unit.inlines[n].fold = Some(unit.folds.len() as u32);
            unit.folds.push(fold);
        }
    }
}

/// What a path holds at an op: where each operand above the body's
/// locals comes from, the locals the path wrote, and its steps so far.
#[derive(Clone)]
struct Path {
    stack: Vec<Slot>,
    set: Vec<(u16, Slot)>,
    steps: Vec<Step>,
    made: u8,
}

impl Path {
    fn make(&mut self, step: Step) -> Option<Slot> {
        if self.steps.len() == FOLD_STEPS || usize::from(self.made) == FOLD_STEPS {
            return None;
        }
        self.steps.push(step);
        self.made += 1;
        Some(Slot::Made(self.made - 1))
    }

    fn guard(&mut self, holds: bool) -> Option<()> {
        if self.steps.len() == FOLD_STEPS {
            return None;
        }
        let value = self.stack.pop()?;
        self.steps.push(Step::Guard { value, holds });
        Some(())
    }
}

/// The fold of inline site `n` of `unit`, `None` when no path of its
/// region folds.
fn derive(program: &Program, unit: &Unit, n: u32) -> Option<Fold> {
    let site = unit.inlines[n as usize];
    let argc = unit.calls[site.call as usize].argc;
    let path = Path {
        stack: Vec::new(),
        set: Vec::new(),
        steps: Vec::new(),
        made: 0,
    };
    follow(program, unit, n, argc, site.entry as usize, path)
}

/// The fold of the first path from `pc` that reaches the region's
/// `Op::Leave`, the branch's fall-through tried before its jump. The
/// call passed `argc` arguments, the body's first locals.
fn follow(
    program: &Program,
    unit: &Unit,
    n: u32,
    argc: u8,
    mut pc: usize,
    mut path: Path,
) -> Option<Fold> {
    let site = unit.inlines[n as usize];
    loop {
        if !(site.entry as usize..site.end as usize).contains(&pc) || !quiet(unit, pc) {
            return None;
        }
        match unit.ops[pc] {
            Op::Block(_) | Op::Nop => {}
            Op::Const(k) => path.stack.push(Slot::Const(k)),
            Op::Local(local) => {
                let slot = match path.set.iter().rev().find(|(set, _)| *set == local) {
                    Some(&(_, slot)) => slot,
                    None if local < u16::from(argc) => Slot::Local(local),
                    None => return None,
                };
                path.stack.push(slot);
            }
            Op::SetLocal(local) => {
                let slot = path.stack.pop()?;
                path.set.push((local, slot));
            }
            Op::Pop => {
                path.stack.pop()?;
            }
            Op::Jump(to) => {
                pc = to as usize;
                continue;
            }
            Op::JumpIfFalse(to) => {
                let mut taken = path.clone();
                path.guard(true)?;
                if let Some(fold) = follow(program, unit, n, argc, pc + 1, path) {
                    return Some(fold);
                }
                taken.guard(false)?;
                path = taken;
                pc = to as usize;
                continue;
            }
            Op::Index => {
                let index = path.stack.pop()?;
                let target = path.stack.pop()?;
                let made = path.make(Step::Index { target, index })?;
                path.stack.push(made);
            }
            Op::Call(call) => {
                let call = &unit.calls[call as usize];
                let Callee::Native {
                    native,
                    charged: false,
                } = call.callee
                else {
                    return None;
                };
                let operation = answers(program, native, usize::from(call.argc))?;
                let mut args = [Slot::Const(0); 2];
                for at in (0..usize::from(call.argc)).rev() {
                    args[at] = path.stack.pop()?;
                }
                let made = path.make(Step::Answer {
                    operation,
                    args,
                    argc: call.argc,
                })?;
                path.stack.push(made);
            }
            Op::Leave(leaving) if leaving == n => {
                let [result] = path.stack[..] else {
                    return None;
                };
                let mut steps = [Step::Guard {
                    value: result,
                    holds: true,
                }; FOLD_STEPS];
                steps[..path.steps.len()].copy_from_slice(&path.steps);
                return Some(Fold {
                    steps,
                    len: path.steps.len() as u8,
                    result,
                    leave: pc as u32,
                });
            }
            _ => return None,
        }
        pc += 1;
    }
}

/// Whether op `pc` bills, paces, tests and shows nothing.
fn quiet(unit: &Unit, pc: usize) -> bool {
    unit.event[pc].total() == 0
        && unit.pace[pc] == 0
        && !unit.step[pc]
        && !unit.points.iter().any(|&(at, _)| at as usize == pc)
}

/// The inline answer (`Vm::immediate`) native `native` gives over `argc`
/// arguments: the operation its version declares, where the core
/// answers it.
fn answers(program: &Program, native: u32, argc: usize) -> Option<InlineOperation> {
    let entry = &program.natives[native as usize];
    let operation = entry.scalar.filter(|_| usize::from(entry.params) == argc)?;
    let arity = match operation {
        InlineOperation::Scalar { .. } | InlineOperation::Same | InlineOperation::Membership(_) => {
            2
        }
        InlineOperation::Kind | InlineOperation::Not | InlineOperation::Length(_) => 1,
        _ => return None,
    };
    (arity == argc).then_some(operation)
}
