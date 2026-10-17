//! A library body's generated plan, run where a native would answer
//! (FIG-6678).
//!
//! The plan performs the body's forms through the machine's own primitives,
//! in the body's order, and holds what the body's frame would: its operands
//! and variables are registers above the caller's, published as execution
//! roots, each variable held as the body binds it, and what a statement
//! pins is released when the plan ends that statement. Its call's depth unit stays
//! held while it runs, so a call it makes is checked one deeper, as one the
//! body's frame makes is (`K-BND-002`). Its own forms are not charged; the
//! formula is, when the call returns (`K-CHG-003`, `K-CHG-007`).

use std::sync::Arc;

use lash_kernel_doc::plan::{Cursor as PlanCursor, PlanContext, PlanExit, Test};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::{Reg as KirReg, RegRange};
use lash_kernel_state::paged::SemanticHeap;

use super::loops::Cursor;
use super::ops::Reg;
use super::{Eval, Halt, Interrupt, KernelMachine, fault, raise};
use crate::compile::{Executable, LibId, PreparedPlan};
use crate::heap::value_bytes;

/// What a statement the plan performs started from: what it pins and
/// holds from then on is its own, released when it ends.
#[derive(Clone, Copy)]
struct Mark {
    pins: usize,
    borrowed: usize,
    bytes: u64,
    fresh: usize,
    reserved: u64,
}

/// One running plan: the machine, the caller's arguments and the plan's
/// registers.
struct PlanRun<'m> {
    machine: &'m mut KernelMachine,
    task: TaskId,
    exe: &'m Executable,
    prepared: &'m PreparedPlan,
    /// The caller's argument registers: the plan's parameters, which a
    /// plan reads where the native path reads them.
    args: Reg,
    /// The first of the plan's operand registers.
    base: Reg,
    /// The first of its variables' registers.
    slots: Reg,
    /// Each open loop's cursor, and the variable that holds what it walks.
    cursors: Vec<(Cursor, Reg)>,
    statement: Option<Mark>,
    halt: Option<Box<Halt>>,
}

impl PlanRun<'_> {
    fn exit(&mut self, interrupt: Interrupt) -> PlanExit {
        match interrupt {
            Interrupt::Raise(value) => PlanExit::Raise(*value),
            Interrupt::Halt(halt) => {
                self.halt = Some(halt);
                PlanExit::Halt
            }
        }
    }

    fn check<T>(&mut self, result: Eval<T>) -> Result<T, PlanExit> {
        result.map_err(|interrupt| self.exit(interrupt))
    }

    fn held<T>(&mut self, result: Result<T, Halt>) -> Result<T, PlanExit> {
        self.check(result.map_err(Interrupt::from))
    }

    fn range(&self, start: u32, len: u32) -> RegRange {
        RegRange {
            start: KirReg((self.base + start as usize) as u32),
            len,
        }
    }

    /// Ends the open statement, if any: its operands are emptied, and its
    /// pins, fresh objects and write credit released.
    fn end_statement(&mut self) -> Result<(), Halt> {
        let Some(mark) = self.statement.take() else {
            return Ok(());
        };
        let machine = &mut *self.machine;
        machine.pins.values.truncate(mark.pins);
        machine.pins.borrowed.truncate(mark.borrowed);
        machine.pins.bytes = machine.pins.bytes.min(mark.bytes);
        machine.fresh.truncate(mark.fresh);
        machine.heap.release_reserved_to(mark.reserved)?;
        let registers = self.prepared.plan.registers as usize;
        machine.regs.clear_range(self.base..self.base + registers);
        machine.heap.execution_roots_moved();
        Ok(())
    }
}

impl PlanContext for PlanRun<'_> {
    fn begin(&mut self) -> Result<(), PlanExit> {
        let ended = self.end_statement();
        self.held(ended)?;
        let machine = &*self.machine;
        self.statement = Some(Mark {
            pins: machine.pins.values.len(),
            borrowed: machine.pins.borrowed.len(),
            bytes: machine.pins.bytes,
            fresh: machine.fresh.len(),
            reserved: machine.heap.statement_reserved(),
        });
        Ok(())
    }

    fn param(&mut self, index: u32, dst: u32) -> Result<(), PlanExit> {
        let value = self.machine.regs[self.args + index as usize].clone();
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn adopt(&mut self, index: u32, slot: u32) -> Result<(), PlanExit> {
        // The argument stays: the formula is charged on what was passed, as
        // the body's frame retains it.
        let value = self.machine.regs[self.args + index as usize].clone();
        self.machine.put(self.slots + slot as usize, value);
        Ok(())
    }

    fn literal(&mut self, literal: u32, dst: u32) -> Result<(), PlanExit> {
        let value = self
            .machine
            .literal(&self.prepared.literals[literal as usize]);
        let value = self.check(value)?;
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn local(&mut self, slot: u32, dst: u32) -> Result<(), PlanExit> {
        let value = self.machine.regs[self.slots + slot as usize].clone();
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn bind(&mut self, slot: u32, src: u32) -> Result<(), PlanExit> {
        let value = self.machine.take(self.base + src as usize);
        let value = self.machine.canonical_binding(value);
        let value = self.held(value)?;
        let reserved = self.machine.reserve_held(value_bytes(&value));
        self.held(reserved)?;
        self.machine.put(self.slots + slot as usize, value);
        Ok(())
    }

    fn assign(&mut self, slot: u32, src: u32) -> Result<(), PlanExit> {
        let value = self.machine.take(self.base + src as usize);
        let at = self.slots + slot as usize;
        let growth = value_bytes(&value).saturating_sub(value_bytes(&self.machine.regs[at]));
        if growth != 0 {
            // The variable's old holding already pays for that much.
            let reserved = self.machine.reserve_held(growth);
            self.held(reserved)?;
        }
        self.machine.put(at, value);
        Ok(())
    }

    fn unbind(&mut self, slot: u32) {
        let at = self.slots + slot as usize;
        self.machine.regs.clear_range(at..at + 1);
    }

    fn call(&mut self, callee: u32, args: u32, count: u32) -> Result<(), PlanExit> {
        let lib = self.prepared.callees[callee as usize];
        let called = self.machine.call_answered(
            self.task,
            self.exe,
            lib,
            self.base + args as usize,
            count as usize,
        );
        self.check(called)
    }

    fn field(&mut self, at: u32, field: u32) -> Result<(), PlanExit> {
        let at = self.base + at as usize;
        let read = self
            .machine
            .read_field(at, &self.prepared.fields[field as usize], at);
        self.check(read)
    }

    fn index(&mut self, at: u32) -> Result<(), PlanExit> {
        let at = self.base + at as usize;
        let read = self.machine.read_index(at, at + 1, at, None);
        self.check(read)
    }

    fn tuple(&mut self, start: u32, len: u32) -> Result<(), PlanExit> {
        let made = self.machine.make_tuple(self.range(start, len));
        self.check(made)
    }

    fn list(&mut self, start: u32, len: u32) -> Result<(), PlanExit> {
        let made = self.machine.make_list(self.range(start, len));
        self.check(made)
    }

    fn set(&mut self, start: u32, len: u32) -> Result<(), PlanExit> {
        let made = self.machine.make_set(self.range(start, len));
        self.check(made)
    }

    fn map(&mut self, start: u32, len: u32) -> Result<(), PlanExit> {
        let made = self.machine.make_map(self.range(start, len));
        self.check(made)
    }

    fn record(&mut self, record: u32, start: u32) -> Result<(), PlanExit> {
        let plan = self.prepared.plan;
        let fields = plan.records[record as usize];
        let names = fields.iter().map(|field| plan.fields[*field as usize]);
        let range = self.range(start, fields.len() as u32);
        let made = self.machine.make_record(names, range);
        self.check(made)
    }

    fn pin(&mut self, src: u32) -> Result<(), PlanExit> {
        let pinned = self.machine.pin(self.base + src as usize);
        self.held(pinned)
    }

    fn set_field(&mut self, target: u32, field: u32, src: u32) -> Result<(), PlanExit> {
        let name = self.prepared.plan.fields[field as usize];
        let written =
            self.machine
                .write_field(self.base + target as usize, name, self.base + src as usize);
        self.check(written)
    }

    fn set_index(&mut self, target: u32, src: u32) -> Result<(), PlanExit> {
        let target = self.base + target as usize;
        let written = self
            .machine
            .write_index(target, target + 1, self.base + src as usize);
        self.check(written)
    }

    fn remove_field(&mut self, target: u32, field: u32) -> Result<(), PlanExit> {
        let name = self.prepared.plan.fields[field as usize];
        let removed = self.machine.remove_field(self.base + target as usize, name);
        self.check(removed)
    }

    fn remove_index(&mut self, target: u32) -> Result<(), PlanExit> {
        let target = self.base + target as usize;
        let removed = self.machine.remove_index(target, target + 1);
        self.check(removed)
    }

    fn test(&mut self, cond: u32, test: Test) -> Result<bool, PlanExit> {
        match self.machine.take(self.base + cond as usize) {
            Value::Bool(value) => Ok(value),
            _ => {
                let raised = match test {
                    Test::If => raise("type_error", "an `if` condition must be a bool"),
                    Test::While => raise("type_error", "a `while` condition must be a bool"),
                };
                self.check(raised)
            }
        }
    }

    fn open(&mut self, iterable: u32, slot: u32) -> Result<PlanCursor, PlanExit> {
        let value = self.machine.take(self.base + iterable as usize);
        let Ok(cursor) = Cursor::over(value, false) else {
            let raised = raise(
                "type_error",
                "`for` iterates a list, a tuple, a map or a set",
            );
            return self.check(raised);
        };
        let held = self.slots + slot as usize;
        if let Some(walked) = cursor.walked() {
            self.machine.put(held, walked);
        }
        let at = self.cursors.len();
        self.cursors.push((cursor, held));
        Ok(PlanCursor(at as u32))
    }

    fn next(&mut self, cursor: PlanCursor, dst: u32) -> Result<bool, PlanExit> {
        let Some((walking, _)) = self.cursors.get_mut(cursor.0 as usize) else {
            let missing = Err(fault("a plan stepped a loop it has not opened"));
            return self.held(missing);
        };
        let stepped = walking.step(&self.machine.heap);
        match self.held(stepped.map_err(Halt::from))? {
            Some(element) => {
                self.machine.put(self.base + dst as usize, element);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn close(&mut self, cursor: PlanCursor) {
        let at = cursor.0 as usize;
        for (_, held) in self.cursors.drain(at..) {
            self.machine.regs.clear_range(held..held + 1);
        }
    }

    fn meter(&mut self, units: u64) -> Result<(), PlanExit> {
        let machine = &mut *self.machine;
        let charged = super::ops::charge_scheduled(
            &mut machine.charged,
            machine.bounds.charge,
            units,
            machine
                .observer
                .as_deref()
                .and_then(|observer| observer.charges.as_ref()),
        );
        self.held(charged)
    }

    fn take(&mut self, src: u32) -> Value {
        self.machine.take(self.base + src as usize)
    }
}

impl KernelMachine {
    /// A value leaving a plan, as one leaving a frame is held
    /// ([`KernelMachine::complete`]): `holds` times, until the step ends.
    fn departing(&mut self, value: Value, holds: usize) -> Result<Value, Halt> {
        let value = self.heap_write(|heap| {
            heap.canonical_value(value.clone())
                .map_err(super::write_halt)
        })?;
        for _ in 0..holds {
            self.pins.borrowed.push(value.clone());
        }
        Ok(value)
    }

    /// Runs `plan`, library function `lib`'s, with the arguments from
    /// `args`: its result, or the value it raises.
    pub(super) fn run_plan(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        plan: &Arc<PreparedPlan>,
        args: Reg,
    ) -> Eval<Result<Value, Value>> {
        // The call's depth unit (`K-BND-002`): an operation's admission
        // checked it; any other plan's is checked as its body's frame is at
        // its push, and a refusal here names no function.
        if !exe.lib(lib).operation() {
            self.check_call_depth(task)?;
        }
        let arity = exe.lib(lib).arity;
        let base = self.regs.extent().max(args + arity);
        let registers = plan.plan.registers as usize;
        let slots = base + registers;
        let end = slots + plan.plan.slots as usize;
        self.regs.window(end);
        let charging = std::mem::replace(&mut self.charging, false);
        self.plans += 1;
        let mut run = PlanRun {
            machine: self,
            task,
            exe,
            prepared: plan,
            args,
            base,
            slots,
            cursors: Vec::new(),
            statement: None,
            halt: None,
        };
        let outcome = (plan.plan.run)(&mut run);
        // The statement the body leaves by stays open: the caller's goes on
        // in its step, as it does when the body's frame returns.
        let halt = run.halt.take();
        self.plans -= 1;
        self.charging = charging;
        self.regs.release(base);
        self.heap.execution_roots_moved();
        let outcome = match outcome {
            // A value on its way out is in no variable: the caller holds it
            // as it holds what a frame returns or raises to it.
            // A frame's return is held as it leaves the frame and again as
            // it arrives in the caller's statement.
            Ok(value) => Ok(self.departing(value, 2)?),
            Err(PlanExit::Raise(value)) => Err(PlanExit::Raise(self.departing(value, 1)?)),
            Err(PlanExit::Halt) => Err(PlanExit::Halt),
        };
        match (outcome, halt) {
            (_, Some(mut halt)) => {
                // A bound passed under an operation called from charged code
                // is that operation's, as when it passes in the body's
                // frames (`K-LIB-006`).
                if charging && let Halt::Bound(exceeded) = halt.as_mut() {
                    exceeded.function = Some(exe.lib(lib).definition.name.clone());
                }
                Err(Interrupt::Halt(halt))
            }
            (Ok(value), None) => Ok(Ok(value)),
            (Err(PlanExit::Raise(value)), None) => Ok(Err(value)),
            (Err(PlanExit::Halt), None) => Err(fault("a plan halted without a cause").into()),
        }
    }
}
