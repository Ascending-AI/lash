//! A library body's generated plan, run where a native would answer
//! (FIG-6678).
//!
//! The plan performs the body's forms through the machine's own primitives,
//! in the body's order, and holds what the body's frame would: its operands
//! and variables are registers above the caller's, published as execution
//! roots, and what a statement pins is released when the plan ends that
//! statement. Its call's depth unit stays
//! held while it runs, so a call it makes is checked one deeper, as one the
//! body's frame makes is (`K-BND-002`). Its own forms are not charged; the
//! formula is, when the call returns (`K-CHG-003`, `K-CHG-007`).
//!
//! A segment's callback sites yield where the body would stand in a frame
//! (`super::segments`, FIG-6709), its `perform`s among them (FIG-6916).

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::plan::{Cursor as PlanCursor, PlanContext, PlanExit, Test};
use lash_kernel_doc::{NativeError, Retry, TaskId, Value};
use lash_kernel_exec::{Reg as KirReg, RegRange};

use super::loops::Cursor;
use super::ops::Reg;
use super::{Eval, Halt, Interrupt, KernelMachine, fault, raise};
use crate::compile::{Executable, LibId, PreparedPlan};

/// What a statement the plan performs started from: what it pins from
/// then on is its own, released when it ends, as are the operands it puts
/// in the plan registers below `span`.
#[derive(Clone, Copy)]
pub(super) struct Mark {
    pins: usize,
    borrowed: usize,
    span: u32,
}

/// A `for` a plan walks: its cursor, the variable that holds what it
/// walks, and how many passes it has started, as the body's frame counts
/// them.
pub(super) struct Walk {
    pub(super) cursor: Cursor,
    pub(super) held: Reg,
    pub(super) started: u64,
    /// The region the plan runs the loop's iterations as, if any
    /// (`super::plan_regions`).
    pub(super) region: Option<u32>,
}

/// The callback a region's steady callback site applies while the region
/// walks its loop (`super::plan_regions`): the closure in its variable,
/// and the code and cells that closure calls.
pub(super) struct Steady {
    pub(super) closure: ArenaHandle,
    pub(super) code: crate::compile::CodeId,
    pub(super) cells: Vec<ArenaHandle>,
}

/// One running plan: the machine, the caller's arguments and the plan's
/// registers.
pub(super) struct PlanRun<'m, const SHARED: bool> {
    pub(super) machine: &'m mut KernelMachine,
    pub(super) task: TaskId,
    pub(super) exe: &'m Executable,
    pub(super) owner: &'m std::sync::Arc<PreparedPlan>,
    pub(super) prepared: &'m PreparedPlan,
    pub(super) lib: LibId,
    /// The caller's argument registers: the plan's parameters, which a
    /// plan reads where the native path reads them.
    pub(super) args: Reg,
    pub(super) entry: Option<&'m super::operation_entry::OperationEntry>,
    /// The first of the plan's operand registers.
    pub(super) base: Reg,
    /// The first of its variables' registers.
    pub(super) slots: Reg,
    /// The first register it holds: the window released when it ends.
    pub(super) window: Reg,
    /// Each open loop's cursor, and the variable that holds what it walks.
    pub(super) cursors: Vec<Walk>,
    /// How many passes each counted `while` has started, outermost first.
    pub(super) whiles: Vec<u64>,
    /// What the body's frame keeps of the call's arguments.
    pub(super) kept: super::segments::Kept,
    pub(super) statement: Option<Mark>,
    pub(super) halt: Option<Box<Halt>>,
    /// The callback the running region's steady site applies, once
    /// resolved.
    pub(super) steady: Option<Steady>,
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
    fn shared(&self, slot: u32) -> bool {
        SHARED
            && self
                .prepared
                .shared
                .as_ref()
                .is_some_and(|shared| shared[slot as usize])
    }

    fn cell(&self, slot: u32) -> Result<ArenaHandle, Halt> {
        match self.machine.regs[self.slots + slot as usize] {
            Value::ObjectRef(cell) => Ok(cell),
            _ => Err(fault("a plan's captured variable has no cell")),
        }
    }

    pub(super) fn bind_value(&mut self, slot: u32, value: Value) -> Result<Value, PlanExit> {
        let holding = if self.shared(slot) {
            Value::ObjectRef(self.machine.new_cell(value))
        } else {
            value
        };
        self.machine.put(self.slots + slot as usize, holding);
        Ok(value)
    }

    pub(super) fn exit(&mut self, interrupt: Interrupt) -> PlanExit {
        match interrupt {
            Interrupt::Raise(value) => PlanExit::Raise(*value),
            Interrupt::Refuse(refusal) => PlanExit::Raise(*self.machine.thrown(*refusal)),
            Interrupt::Halt(halt) => {
                self.halt = Some(halt);
                PlanExit::Halt
            }
        }
    }

    pub(super) fn check<T>(&mut self, result: Eval<T>) -> Result<T, PlanExit> {
        result.map_err(|interrupt| self.exit(interrupt))
    }

    pub(super) fn held<T>(&mut self, result: Result<T, Halt>) -> Result<T, PlanExit> {
        self.check(result.map_err(Interrupt::from))
    }

    fn range(&self, start: u32, len: u32) -> RegRange {
        RegRange {
            start: KirReg((self.base + start as usize) as u32),
            len,
        }
    }

    /// Ends the open statement, if any: its operands are emptied, and its
    /// pins and write credit released.
    fn end_statement(&mut self) -> Result<(), Halt> {
        let Some(mark) = self.statement.take() else {
            return Ok(());
        };
        let machine = &mut *self.machine;
        machine.pins.values.truncate(mark.pins);
        machine.pins.borrowed.truncate(mark.borrowed);
        machine
            .regs
            .clear_range(self.base..self.base + mark.span as usize);
        machine.heap.execution_roots_moved();
        Ok(())
    }
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
    /// Steps the loop on `cursor` to its next element, counting the pass
    /// it starts, or finds it exhausted.
    pub(super) fn step(&mut self, cursor: PlanCursor) -> Result<Option<Value>, PlanExit> {
        let stepped = loop {
            let Some(walk) = self.cursors.get_mut(cursor.0 as usize) else {
                let missing = Err(fault("a plan stepped a loop it has not opened"));
                return self.held(missing);
            };
            let stepped = walk.cursor.step(&mut self.machine.arena.borrow_mut());
            // What the loop walks is decoded, then stepped again.
            match stepped {
                Err(NativeError::Retry(Retry::Cold { object, chunk })) => {
                    let warmed = self.machine.warm(object, chunk);
                    self.held(warmed)?;
                }
                stepped => {
                    let stepped = stepped.inspect(|element| {
                        walk.started += u64::from(element.is_some());
                    });
                    break stepped.map_err(Interrupt::from);
                }
            }
        };
        self.check(stepped)
    }

    /// Calls library function `lib` on the `count` arguments from register
    /// `at`, its value to `at`, as the plan's general call does.
    pub(super) fn call_at(&mut self, lib: LibId, at: Reg, count: usize) -> Result<(), PlanExit> {
        let function = self.exe.lib(lib);
        // The general call's first answer, taken directly: an unobserved
        // call of every parameter that no admission precedes, which its
        // prepared entry answers at the plan's own depth. One it declines
        // goes on past that entry.
        if count == function.arity
            && !function.operation()
            && !self.machine.charging
            && self.machine.observer.is_none()
            && let Some((entry, _)) = &function.prepared
            && entry.depth() == 0
        {
            let Some(answer) = entry.answer(
                &self.machine.regs[at..at + function.arity],
                self.machine.charging,
            ) else {
                let called = self.machine.call_declined(self.task, self.exe, lib, at);
                return self.check(called);
            };
            let pinned = self.machine.pin_value(&answer.value);
            self.held(pinned)?;
            self.machine.put(at, answer.value);
            return Ok(());
        }
        let called = self
            .machine
            .call_answered(self.task, self.exe, lib, at, count);
        self.check(called)
    }

    /// Opens the next statement, ending the one open.
    pub(super) fn open_statement(&mut self) -> Result<(), PlanExit> {
        self.open_statement_of(self.prepared.plan.registers)
    }

    /// Widens the open statement to all the plan's registers: a form of
    /// it that did not answer as fused puts its operands as the plan's
    /// code puts them.
    #[cold]
    pub(super) fn widen(&mut self) {
        if let Some(mark) = self.statement.as_mut() {
            mark.span = self.prepared.plan.registers;
        }
    }

    /// Opens the next statement, which puts its operands in the plan
    /// registers below `span`, ending the one open.
    #[inline(always)]
    pub(super) fn open_statement_of(&mut self, span: u32) -> Result<(), PlanExit> {
        let machine = &mut *self.machine;
        if let Some(mark) = self.statement.as_mut()
            && mark.span == 0
            && mark.pins == machine.pins.values.len()
            && mark.borrowed == machine.pins.borrowed.len()
        {
            // The open statement put and pinned nothing: its end releases
            // nothing, and the next statement starts where it did.
            machine.heap.execution_roots_moved();
            mark.span = span;
            return Ok(());
        }
        let ended = self.end_statement();
        self.held(ended)?;
        let machine = &*self.machine;
        self.statement = Some(Mark {
            pins: machine.pins.values.len(),
            borrowed: machine.pins.borrowed.len(),
            span,
        });
        Ok(())
    }
}

impl<const SHARED: bool> PlanContext for PlanRun<'_, SHARED> {
    fn begin(&mut self) -> Result<(), PlanExit> {
        self.open_statement()
    }

    fn param(&mut self, index: u32, dst: u32) -> Result<(), PlanExit> {
        let value = self.machine.regs[self.args + index as usize];
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn adopt(&mut self, index: u32, slot: u32) -> Result<(), PlanExit> {
        // The argument stays: the formula is charged on what was passed, as
        // the body's frame retains it.
        let value = self.machine.regs[self.args + index as usize];
        if self.shared(slot) {
            let admitted = self
                .entry
                .and_then(|entry| entry.shared.get(index as usize))
                .copied()
                .flatten();
            let cell = match admitted {
                Some(cell) => cell,
                None => self.machine.new_cell(value),
            };
            self.machine
                .put(self.slots + slot as usize, Value::ObjectRef(cell));
        } else {
            self.machine.put(self.slots + slot as usize, value);
        }
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
        let value = if self.shared(slot) {
            let cell = self.cell(slot);
            let cell = self.held(cell)?;
            let value = self.machine.in_arena(|arena| arena.cell(cell));
            self.check(value)?
        } else {
            self.machine.regs[self.slots + slot as usize]
        };
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn bind(&mut self, slot: u32, src: u32) -> Result<(), PlanExit> {
        let value = self.machine.take(self.base + src as usize);
        self.bind_value(slot, value)?;
        Ok(())
    }

    fn assign(&mut self, slot: u32, src: u32) -> Result<(), PlanExit> {
        let value = self.machine.take(self.base + src as usize);
        if self.shared(slot) {
            let cell = self.cell(slot);
            let cell = self.held(cell)?;
            let set = self.machine.in_arena(|arena| arena.cell_set(cell, value));
            self.check(set)?;
            self.machine.grown();
            return Ok(());
        }
        self.machine.put(self.slots + slot as usize, value);
        Ok(())
    }

    fn unbind(&mut self, slot: u32) {
        let at = self.slots + slot as usize;
        self.machine.regs.clear_range(at..at + 1);
    }

    fn closure(&mut self, closure: u32, dst: u32) -> Result<(), PlanExit> {
        let code = self.prepared.closures[closure as usize];
        let mut cells = Vec::new();
        for capture in &self.exe.code(code).captures {
            let slot = self
                .prepared
                .locals
                .iter()
                .position(|local| local.is_some_and(|local| local.slot == capture.outer));
            let Some(slot) = slot else {
                return self.held(Err(fault("a plan closure has no captured declaration")));
            };
            let cell = self.cell(slot as u32);
            cells.push(self.held(cell)?);
        }
        let made = self.machine.make_closure(self.exe, code, &cells);
        let value = Value::Closure(self.check(made)?);
        self.machine.put(self.base + dst as usize, value);
        Ok(())
    }

    fn retain(&mut self, value: Value, slot: u32) -> Result<Value, PlanExit> {
        if !self.prepared.plan.slots[slot as usize].name.is_empty() {
            return self.bind_value(slot, value);
        }
        // A pending departure is a borrowed payload, not a new binding.
        // Its private register roots it across finally's statement marks.
        self.machine.put(self.slots + slot as usize, value);
        Ok(value)
    }

    fn scope(&self) -> u32 {
        self.cursors.len() as u32
    }

    fn unwind(&mut self, scope: u32) {
        self.close(PlanCursor(scope));
    }

    fn call(&mut self, callee: u32, args: u32, count: u32) -> Result<(), PlanExit> {
        let lib = self.prepared.callees[callee as usize];
        self.call_at(lib, self.base + args as usize, count as usize)
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
        let region = self.prepared.region(slot);
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
        self.cursors.push(Walk {
            cursor,
            held,
            started: 0,
            region,
        });
        Ok(PlanCursor(at as u32))
    }

    fn next(&mut self, cursor: PlanCursor, dst: u32) -> Result<bool, PlanExit> {
        if let Some(region) = self
            .cursors
            .get(cursor.0 as usize)
            .and_then(|walk| walk.region)
        {
            return self.walk_region(cursor, region);
        }
        match self.step(cursor)? {
            Some(element) => {
                self.machine.put(self.base + dst as usize, element);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn close(&mut self, cursor: PlanCursor) {
        let at = (cursor.0 as usize).min(self.cursors.len());
        for walk in self.cursors.drain(at..) {
            self.machine.regs.clear_range(walk.held..walk.held + 1);
        }
    }

    fn enter_while(&mut self) {
        self.whiles.push(0);
    }

    fn pass_while(&mut self) {
        if let Some(started) = self.whiles.last_mut() {
            *started += 1;
        }
    }

    fn leave_while(&mut self, depth: u32) {
        self.whiles.truncate(depth as usize);
    }

    fn invoke(&mut self, site: u32, callee: u32, args: u32, count: u32) -> Result<(), PlanExit> {
        let lib = self.prepared.callees[callee as usize];
        let at = self.base + args as usize;
        self.offer(site, lib, at, count as usize)
    }

    fn apply(&mut self, site: u32, at: u32, count: u32) -> Result<(), PlanExit> {
        self.apply_at(site, self.base + at as usize, count as usize)
    }

    fn perform(&mut self, site: u32, args: u32, count: u32) -> Result<(), PlanExit> {
        self.perform_at(site, self.base + args as usize, count as usize)
    }

    fn lost(&mut self, entry: u32) -> PlanExit {
        self.halt = Some(Box::new(fault(&format!(
            "a segment resumed at entry {entry}, which it has no site for"
        ))));
        PlanExit::Halt
    }

    fn meter(&mut self, units: u64) -> Result<(), PlanExit> {
        let machine = &mut *self.machine;
        if machine.bounds.charge == crate::Charge::Off {
            return Ok(());
        }
        let charged = super::ops::charge_scheduled(
            &mut machine.charged,
            &mut machine.bounds.charge_limit,
            &mut machine.limiters,
            units,
            machine.observer.as_deref(),
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
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            for _ in 0..holds {
                self.pins.borrowed.push(value);
            }
        }
        Ok(value)
    }

    /// Runs `plan`, library function `lib`'s, with the arguments from
    /// `args`: its result, or the value it raises; `None` when it yielded
    /// at a callback site, its frames to stand there once the caller has
    /// saved where it stands ([`KernelMachine::stand_segments`]).
    #[inline(always)]
    pub(super) fn run_plan(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        plan: &std::sync::Arc<PreparedPlan>,
        args: Reg,
        entry: Option<&super::operation_entry::OperationEntry>,
    ) -> Eval<Option<Result<Value, Value>>> {
        if plan.shared.is_some() {
            self.run_plan_variant::<true>(task, exe, lib, plan, args, entry)
        } else {
            self.run_plan_variant::<false>(task, exe, lib, plan, args, entry)
        }
    }

    fn run_plan_variant<const SHARED: bool>(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        plan: &std::sync::Arc<PreparedPlan>,
        args: Reg,
        entry: Option<&super::operation_entry::OperationEntry>,
    ) -> Eval<Option<Result<Value, Value>>> {
        // The call's depth unit (`K-BND-002`): an operation's admission
        // checked it; any other plan's is checked as its body's frame is at
        // its push, and a refusal here names no function.
        let root = if !plan.operation {
            self.check_call_depth(task)?
        } else if let Some(entry) = entry {
            entry.root
        } else {
            self.task(task)?.frames.is_empty()
        };
        let arity = plan.arity;
        let base = self.regs.extent().max(args + arity);
        let registers = plan.plan.registers as usize;
        let slots = base + registers;
        let end = slots + plan.plan.slots.len();
        self.regs.statement(end as u32);
        let charging = std::mem::replace(&mut self.charging, false);
        let paced = std::mem::replace(&mut self.paced, false);
        // A body's frame keeps its call's arguments for its formula when
        // the call is charged, or roots a task (`push_operation_frame`).
        let keep = charging || root;
        self.plans += 1;
        let mut run = PlanRun::<SHARED> {
            machine: self,
            task,
            exe,
            owner: plan,
            prepared: plan,
            lib,
            args,
            entry,
            base,
            slots,
            window: base,
            cursors: Vec::new(),
            whiles: Vec::new(),
            kept: super::segments::Kept::Window { keep },
            statement: None,
            halt: None,
            steady: None,
        };
        let outcome = (plan.plan.run)(&mut run, 0);
        // The statement the body leaves by stays open: the caller's goes on
        // in its step, as it does when the body's frame returns.
        let halt = run.halt.take();
        self.plans -= 1;
        (self.charging, self.paced) = (charging, paced);
        if matches!(outcome, Err(PlanExit::Yield)) && halt.is_none() {
            // Its registers hold what its frame will: they are released
            // once the frames stand.
            return Ok(None);
        }
        self.regs.release(base);
        self.heap.execution_roots_moved();
        // A frame's return is held as it leaves the frame and again as it
        // arrives in the caller's statement.
        self.plan_outcome(exe, lib, outcome, halt, charging, 2)
            .map(Some)
    }

    /// What a plan of `lib` that ended with `outcome` and `halt` gives its
    /// caller, which charges code when `charging`; a value it returns is
    /// held `returned` times.
    pub(super) fn plan_outcome(
        &mut self,
        exe: &Executable,
        lib: LibId,
        outcome: Result<Value, PlanExit>,
        halt: Option<Box<Halt>>,
        charging: bool,
        returned: usize,
    ) -> Eval<Result<Value, Value>> {
        let outcome = match outcome {
            // A value on its way out is in no variable: the caller holds it
            // as it holds what a frame returns or raises to it.
            Ok(value) => Ok(self.departing(value, returned)?),
            Err(PlanExit::Raise(value)) => Err(PlanExit::Raise(self.departing(value, 1)?)),
            Err(exit) => Err(exit),
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
            (Err(_), None) => Err(fault("a plan halted without a cause").into()),
        }
    }
}
