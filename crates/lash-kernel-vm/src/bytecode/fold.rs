//! Folding: an inline region's guard paths, each as one guarded op
//! (cutover contract §1, §3).
//!
//! A region still dispatches each op of its body: a list read's two
//! bound tests, its length and its index are a dozen dispatches around
//! one element read. The pass follows each region from its entry along
//! both ways of each branch to its `Op::Leave`s, carrying what each op
//! pushes as a fact about where its value comes from, and records each
//! path that gets there, up to four, as a fold: the native answers (R8) it computes, the branches it
//! takes as guards, its index reads and the value it returns. An answer
//! step holds the operation the native's version declares, resolved
//! once here. A path reads a local only where it was written on the path
//! or the call passed it as an argument, so the fold needs no layout of
//! the body's locals.
//!
//! `Op::Inline` makes the activation as before, its depth checked and
//! its frame admitted, and then runs its folds in turn until one holds.
//! Every step reads and none commits: an answer is a pure native answer,
//! an index read finds an element or declines. Two paths part at a
//! branch, one guarding each way, so at most one holds. Where none does,
//! the activation is laid out, its completion owed, and the region runs
//! from its entry over the same state, so the body's own ops raise, read
//! cold or deoptimize as they would. A fold that holds completes the
//! activation at the region's `Op::Leave` with the value, as the body's
//! return does: the completion billed there, the frame released.
//!
//! The pass folds only ops that bill no event, test no
//! slice and show no point, and native calls that bill no fee, so
//! skipping them moves, merges or drops no charge, step or observation.
//! A fold holds no fact past its own run: nothing it reads is written
//! between its steps, and a write or an unknown call ends the path.
//! Selection follows the region's ops and each native's answer under its
//! declared version, never a name.
//!
//! It runs after validation, on validated code, before fusion.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{InlineOperation, IntegerTextDomain, ScalarDomain};

use super::op::Op;

use super::program::{
    Callee, Constant, FOLD_PATHS, FOLD_STEPS, Fold, Program, Slot, Step, Unit, WordFold,
};

/// Folds the guard path of every inline region of `program`.
pub(crate) fn fold(program: &mut Program) {
    for at in 0..program.units.len() {
        for n in 0..program.units[at].inlines.len() {
            let folds = derive(program, &program.units[at], n as u32);
            let unit = &mut program.units[at];
            let start = unit.folds.len() as u32;
            unit.folds.extend(folds);
            unit.inlines[n].folds = (start, unit.folds.len() as u32);
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

/// The folds of inline site `n` of `unit`: its region's paths that fold,
/// at most [`FOLD_PATHS`] of them.
fn derive(program: &Program, unit: &Unit, n: u32) -> Vec<Fold> {
    let site = unit.inlines[n as usize];
    let argc = unit.calls[site.call as usize].argc;
    let path = Path {
        stack: Vec::new(),
        set: Vec::new(),
        steps: Vec::new(),
        made: 0,
    };
    let mut folds = Vec::new();
    follow(
        program,
        unit,
        n,
        argc,
        site.entry as usize,
        path,
        &mut folds,
    );
    folds
}

/// Adds to `folds` the folds of the paths from `pc` that reach the
/// region's `Op::Leave`, each branch's fall-through before its jump,
/// until [`FOLD_PATHS`] fold. The call passed `argc` arguments, the
/// body's first locals.
fn follow(
    program: &Program,
    unit: &Unit,
    n: u32,
    argc: u16,
    pc: usize,
    path: Path,
    folds: &mut Vec<Fold>,
) {
    if folds.len() < FOLD_PATHS
        && let Some(fold) = walk(program, unit, n, argc, pc, path, folds)
    {
        folds.push(fold);
    }
}

/// The fold of the path from `pc` up to its first branch or its
/// `Op::Leave`: at a branch, both ways are followed into `folds`, and
/// the walk gives none.
fn walk(
    program: &Program,
    unit: &Unit,
    n: u32,
    argc: u16,
    mut pc: usize,
    mut path: Path,
    folds: &mut Vec<Fold>,
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
                    None if local < argc => Slot::Local(local),
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
                if path.guard(true).is_some() {
                    follow(program, unit, n, argc, pc + 1, path, folds);
                }
                if taken.guard(false).is_some() {
                    follow(program, unit, n, argc, to as usize, taken, folds);
                }
                return None;
            }
            Op::Index => {
                let index = path.stack.pop()?;
                let target = path.stack.pop()?;
                let made = path.make(Step::Index { target, index })?;
                path.stack.push(made);
            }
            Op::Call(call) => {
                let call = &unit.calls[call as usize];
                if call.entry.is_some() {
                    return None;
                }
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
                    argc: u8::try_from(call.argc).ok()?,
                })?;
                path.stack.push(made);
            }
            Op::Leave(leaving) if leaving == n => {
                let [mut result] = path.stack[..] else {
                    return None;
                };
                kind_tests(program, &mut path.steps, &mut result);
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
                    words: word_form(&path.steps, result),
                });
            }
            _ => return None,
        }
        pc += 1;
    }
}

/// Makes each same answer of a kind answer and a kind's name one
/// `Step::KindIs`, which declines where the kind answer would, and drops
/// each kind answer so replaced that no step reads any more. A Python
/// operator helper tests its operands' kinds this way before each
/// answer.
fn kind_tests(program: &Program, steps: &mut Vec<Step>, result: &mut Slot) {
    let makes = |steps: &[Step]| -> Vec<usize> {
        (0..steps.len())
            .filter(|&at| !matches!(steps[at], Step::Guard { .. }))
            .collect()
    };
    let made = makes(steps);
    let mut replaced = Vec::new();
    for at in 0..steps.len() {
        let Step::Answer {
            operation: InlineOperation::Same,
            args,
            argc: 2,
        } = steps[at]
        else {
            continue;
        };
        for (answer, name) in [(args[0], args[1]), (args[1], args[0])] {
            let (Slot::Made(k), Slot::Const(id)) = (answer, name) else {
                continue;
            };
            let Step::Answer {
                operation: InlineOperation::Kind,
                args: [value, _],
                argc: 1,
            } = steps[made[usize::from(k)]]
            else {
                continue;
            };
            let Some(Constant::Immediate(Value::Text(text))) = program.get_constant(id) else {
                continue;
            };
            let Some(kind) = InlineOperation::kind_named(text) else {
                continue;
            };
            steps[at] = Step::KindIs { value, kind };
            replaced.push(k);
            break;
        }
    }
    replaced.sort_unstable();
    replaced.dedup();
    for &k in replaced.iter().rev() {
        let mut read = *result == Slot::Made(k);
        for step in steps.iter_mut() {
            slots(step, |slot| read |= *slot == Slot::Made(k));
        }
        if read {
            continue;
        }
        steps.remove(makes(steps)[usize::from(k)]);
        let mut renumber = |slot: &mut Slot| {
            if let Slot::Made(m) = slot
                && *m > k
            {
                *m -= 1;
            }
        };
        renumber(result);
        for step in steps.iter_mut() {
            slots(step, &mut renumber);
        }
    }
}

/// Runs `each` over every slot step `step` reads.
fn slots(step: &mut Step, mut each: impl FnMut(&mut Slot)) {
    match step {
        Step::Answer { args, argc, .. } => args[..usize::from(*argc)].iter_mut().for_each(each),
        Step::KindIs { value, .. } | Step::Guard { value, .. } => each(value),
        Step::Index { target, index } => {
            each(target);
            each(index);
        }
    }
}

/// Whether op `pc` bills, tests and shows nothing.
fn quiet(unit: &Unit, pc: usize) -> bool {
    unit.event[pc].total() == 0
        && !unit.step[pc]
        && !unit.points.iter().any(|&(at, _)| at as usize == pc)
}

/// The inline answer (`Vm::immediate`) native `native` gives over `argc`
/// arguments: the operation its version declares, where the core
/// answers it.
fn answers(program: &Program, native: u32, argc: usize) -> Option<InlineOperation> {
    let entry = program.native(native);
    let operation = entry.scalar.filter(|_| usize::from(entry.params) == argc)?;
    let arity = match operation {
        InlineOperation::Scalar { .. } | InlineOperation::Same | InlineOperation::Membership(_) => {
            2
        }
        InlineOperation::IntegerText(IntegerTextDomain::RadixArgument) => 2,
        InlineOperation::Kind
        | InlineOperation::Not
        | InlineOperation::Length(_)
        | InlineOperation::IntegerText(IntegerTextDomain::Decimal) => 1,
        _ => return None,
    };
    (arity == argc).then_some(operation)
}

/// The word form of a fold of `steps` returning `result` (`Fold::words`):
/// int kind tests of the two operands, guards that each test holds, and
/// last the word-domain scalar answer over the operands, which the fold
/// returns. Over two word ints every test is true and every guard holds,
/// so the fold answers what the scalar answer does.
fn word_form(steps: &[Step], result: Slot) -> Option<WordFold> {
    let (&last, before) = steps.split_last()?;
    let Step::Answer {
        operation:
            InlineOperation::Scalar {
                operation,
                domain: ScalarDomain::Words | ScalarDomain::WordsOrFloats,
            },
        args: operands @ [a, b],
        argc: 2,
    } = last
    else {
        return None;
    };
    let operand = |slot: Slot| matches!(slot, Slot::Local(_) | Slot::Const(_));
    if !operand(a) || !operand(b) {
        return None;
    }
    // Only the kind tests make values before the answer.
    let mut made = 0u8;
    for &step in before {
        match step {
            Step::KindIs {
                value,
                kind: lash_kernel_doc::ValueKind::Int,
            } if value == a || value == b => made += 1,
            Step::Guard {
                value: Slot::Made(test),
                holds: true,
            } if test < made => {}
            _ => return None,
        }
    }
    (result == Slot::Made(made)).then_some(WordFold {
        operation,
        operands,
    })
}
