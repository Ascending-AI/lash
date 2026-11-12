//! The live resume closure (cutover contract §7): the build helpers a
//! parked run can still run.
//!
//! The code a run can still run is what each frame reaches forward from
//! the op it resumes at, and what each installed handler reaches from its
//! target; through every call, closure, spawn stub and function value that
//! code names. A helper only completed calls ran is outside it: a call
//! behind a frame's resume point, outside any loop it stands in, is never
//! made again. A seal records the helpers the closure reaches; an open
//! under another build image walks the same closure over the program it
//! compiled, and refuses if a resume point reaches a helper the image
//! lacks.

use std::collections::{BTreeSet, HashMap};

use lash_kernel_value::FunctionId;

use crate::bytecode::op::Op;
use crate::bytecode::program::{ActionSite, Callee, Constant, Program, UnitId, UnitKind};
use crate::heap::slots::SlotHeap;
use crate::vm::Vm;
use crate::vm::frames::Context;

/// The build helpers the live code of `vm` runs, calls or names, frames'
/// own bodies included.
///
/// # Errors
///
/// The helper an unavailable unit stands for, when a resume point reaches
/// one.
pub(crate) fn live(vm: &Vm<SlotHeap>) -> Result<BTreeSet<FunctionId>, FunctionId> {
    let mut walk = Walk::new(&vm.program);
    let tasks = &vm.tasks;
    for (number, task) in tasks.all.iter().enumerate() {
        let cx: &Context = if tasks.installed && number == tasks.running {
            &vm.cx
        } else {
            &task.cx
        };
        for frame in &cx.frames {
            walk.enter(frame.unit, frame.pc);
        }
        for handler in &cx.handlers {
            if let Some(frame) = cx.frames.get(handler.frame as usize) {
                walk.enter(frame.unit, handler.pc);
            }
        }
    }
    walk.run()?;
    Ok(walk.pins)
}

/// The build helpers stored code names: `code` holds the function of each
/// closure or function value the heap sealed, or a helper already
/// resolved.
pub(crate) fn held(program: &Program, code: &BTreeSet<FunctionId>) -> BTreeSet<FunctionId> {
    let closures: HashMap<FunctionId, UnitId> = closure_units(program);
    code.iter()
        .filter_map(|function| match closures.get(function) {
            Some(unit) => helper(program.unit(*unit)),
            None => (!program.declares(function)).then_some(*function),
        })
        .collect()
}

/// The build helper whose code `unit` runs: a library closure's own
/// function, since one closure can stand in several helpers' bodies and
/// a saved frame names only the closure (contract §7: every FunctionId
/// of a live frame); otherwise the helper the unit compiles.
fn helper(unit: &crate::bytecode::program::Unit) -> Option<FunctionId> {
    match (&unit.kind, unit.owner) {
        (UnitKind::Closure(function), Some(_)) => Some(*function),
        (_, owner) => owner,
    }
}

fn closure_units(program: &Program) -> HashMap<FunctionId, UnitId> {
    let mut closures = program.lib.closures.clone();
    for (at, unit) in program.units.iter().enumerate() {
        if let UnitKind::Closure(function) = unit.kind {
            closures.insert(function, program.own_unit(at));
        }
    }
    closures
}

/// A forward walk over ops, each visited once.
struct Walk<'p> {
    program: &'p Program,
    seen: HashMap<UnitId, Vec<bool>>,
    work: Vec<(UnitId, u32)>,
    pins: BTreeSet<FunctionId>,
}

impl<'p> Walk<'p> {
    fn new(program: &'p Program) -> Self {
        Self {
            program,
            seen: HashMap::new(),
            work: Vec::new(),
            pins: BTreeSet::new(),
        }
    }

    fn enter(&mut self, unit: UnitId, pc: u32) {
        self.work.push((unit, pc));
    }

    /// Pins `function` when the build supplies it.
    fn pin(&mut self, function: FunctionId) {
        if !self.program.declares(&function) {
            self.pins.insert(function);
        }
    }

    fn run(&mut self) -> Result<(), FunctionId> {
        let program = self.program;
        while let Some((id, pc)) = self.work.pop() {
            let unit = program.unit(id);
            let seen = self
                .seen
                .entry(id)
                .or_insert_with(|| vec![false; unit.ops.len()]);
            let Some(seen) = seen.get_mut(pc as usize) else {
                continue;
            };
            if std::mem::replace(seen, true) {
                continue;
            }
            if let Some(function) = helper(unit) {
                self.pins.insert(function);
            }
            // An inline region is its call; a fused run, its first op.
            let Some(op) = unit.original(pc as usize) else {
                continue;
            };
            let next = pc + 1;
            match op {
                Op::Unavailable(_) => {
                    if let Some(owner) = unit.owner {
                        return Err(owner);
                    }
                }
                Op::Return | Op::Fall | Op::Raise | Op::Finish | Op::Fail => {}
                Op::Leave(_) | Op::Deopt(_) => {}
                Op::Jump(to) => self.enter(id, to),
                Op::JumpIfFalse(to) | Op::WhileFalse(to) | Op::TryEnter(to) => {
                    self.enter(id, to);
                    self.enter(id, next);
                }
                Op::ForNext(n) | Op::ProtoMore(n) => {
                    self.enter(id, unit.loops[n as usize].exit);
                    self.enter(id, next);
                }
                Op::ProtoStep(n) => {
                    if let Some(driver) = unit.loops[n as usize].driver {
                        self.enter(id, driver);
                    }
                    self.enter(id, next);
                }
                Op::ProtoGuard(n) => {
                    if let Some(join) = unit.loops[n as usize].join {
                        self.enter(id, join);
                    }
                    self.enter(id, next);
                }
                Op::Call(n) => {
                    match unit.calls[n as usize].callee {
                        Callee::Unit { unit, .. } | Callee::Entry { unit, .. } => {
                            self.enter(unit, 0)
                        }
                        Callee::Native { native, .. } => {
                            let native = program.native(native);
                            self.pin(native.function);
                            if let Some(body) = native.body {
                                self.enter(body, 0);
                            }
                        }
                        Callee::Machine { id, .. } => self.pin(id),
                        Callee::Value => {}
                    }
                    self.enter(id, next);
                }
                Op::MakeClosure(n) => {
                    self.enter(unit.closures[n as usize].unit, 0);
                    self.enter(id, next);
                }
                Op::Const(n) => {
                    if let Constant::Function(callable) = program.constant(n) {
                        self.pin(callable.function);
                        let trampoline = program
                            .functions
                            .iter()
                            .find(|entry| entry.callable == *callable)
                            .map(|entry| entry.unit)
                            .or_else(|| program.lib.trampolines.get(&callable.function).copied());
                        if let Some(trampoline) = trampoline {
                            self.enter(trampoline, 0);
                        }
                    }
                    self.enter(id, next);
                }
                Op::Action(n) => {
                    if let ActionSite::Spawn { unit, .. } = &unit.actions[n as usize] {
                        self.enter(*unit, 0);
                    }
                    self.enter(id, next);
                }
                _ => self.enter(id, next),
            }
        }
        Ok(())
    }
}
