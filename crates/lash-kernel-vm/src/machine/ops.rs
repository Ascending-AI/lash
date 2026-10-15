//! The operations: each kernel operation's one description, which the
//! stream's ops execute (`super::stream`).
//!
//! An operation is one machine method with a register-indexed shape: it
//! reads its operands from registers, writes its result to its result
//! register, and ends in one of four ways ([`Status`]):
//!
//! - `Ok(Flow::Done)`: done, its result, if it has one, in its result
//!   register;
//! - `Ok(Flow::Step)`: it ended the task's step where it left the task: it
//!   entered a block, a loop or a frame, or the task waits;
//! - `Err(Interrupt::raised(_))`: it raised;
//! - `Err(Interrupt::Halt(_))`: it ended the run.
//!
//! An operation that cannot step returns [`Eval`] instead. The registers
//! are the machine's register stack: a statement's ops name the registers
//! of its window, where each expression's value goes and each operation
//! reads its operands. Nothing outside a step holds a register, so a parked
//! run never holds one (`K-STMT-004`).
//!
//! The operations are the member reads and writes, the charge, memory
//! reservation and pin anchors, the library call with its charge (the one
//! entry of every library call, inside an expression or in a frame), and
//! the loop's iteration step, here, and action dispatch,
//! [`KernelMachine::act`].

use lash_kernel_doc::{EntityId, Identity, NativeCall, NativeError, TaskId, Value, WorkCounter};
use num_traits::ToPrimitive;

use super::exec::Call;
use super::stream::{Ran, Running};
use super::{Control, Cursor, Eval, Halt, Interrupt, KernelMachine, bound, fault, raise};
use crate::compile::{Charge, Executable, Form, LibId, LibRun, StmtId};
use crate::functions::MachineFunction;
use crate::heap::within_depth;
use crate::interface::{Bound, Host};
use crate::observe::Event;
use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::heap::{FieldName, IntRef, LogicalBytes};
use lash_kernel_state::paged::{ObjectInit, SemanticHeap, TaskStateRecord, TransientRoots};

/// A register: a position in the machine's register stack.
pub(super) type Reg = usize;

/// How an operation that did not raise or halt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flow {
    /// Done: the result, if any, is in the result register.
    Done,
    /// The task's step ends where the operation left the task.
    Step,
}

/// How an operation ends (see the module documentation).
pub(super) type Status = Result<Flow, Interrupt>;

/// How a library function's kernel body runs when it is called.
#[derive(Clone, Copy, Debug)]
pub(super) enum Entry {
    /// In a frame of its own, as an action's call or a task's start; the
    /// statement of the caller's frame that waits in the call, if any.
    Frame(Option<StmtId>),
    /// Inside an expression, to its end there (`K-STMT-002`).
    Inline,
}

/// Drops a register's old value, which is mostly one that owns nothing:
/// that one needs no drop.
#[inline(always)]
fn release(value: Value) {
    if matches!(
        value,
        Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_)
    ) {
        std::mem::forget(value);
    }
}

/// The list position a value is: a number with an integral value that is
/// not negative (`K-FORM-006`). `None` is a number that is no position.
fn position(index: &Value) -> Eval<Option<usize>> {
    match index {
        Value::Int(IntRef::Inline(integer)) => Ok(integer.to_usize()),
        Value::Int(IntRef::Paged { .. }) => Ok(None),
        Value::Float(float) => {
            let float = float.get();
            Ok(
                (float.fract() == 0.0 && float >= 0.0 && float < usize::MAX as f64)
                    .then_some(float as usize),
            )
        }
        _ => raise("type_error", "a list is indexed by a number"),
    }
}

impl KernelMachine {
    /// Writes `value` to register `out`, growing the registers to it.
    #[inline(always)]
    pub(super) fn put(&mut self, out: Reg, value: Value) {
        match self.regs.get_mut(out) {
            Some(held) => release(std::mem::replace(held, value)),
            None => self.grow(out, value),
        }
    }

    #[inline(never)]
    fn grow(&mut self, out: Reg, value: Value) {
        self.regs.resize(out, Value::Null);
        self.regs.push(value);
    }

    /// Takes the value out of register `r`, which is left null.
    #[inline(always)]
    pub(super) fn take(&mut self, r: Reg) -> Value {
        std::mem::replace(&mut self.regs[r], Value::Null)
    }

    /// The charge anchor (`K-CHG-002`): `units` more, unless the running
    /// code is not charged (`K-CHG-007`), ending the run past its bound.
    pub(super) fn charge(&mut self, units: u64) -> Result<(), Halt> {
        if self.charging {
            self.charged = self.charged.saturating_add(units);
            if self.charged > self.bounds.charge {
                return Err(bound(Bound::Charge, self.bounds.charge));
            }
        }
        Ok(())
    }

    /// The memory anchor: accounts `bytes` more memory, collecting first
    /// when the heap is due and ending the run when what is live does not
    /// leave room.
    pub(super) fn reserve(&mut self, bytes: u64) -> Result<(), Halt> {
        self.publish_holdings()?;
        let transient = TransientRoots {
            registers: &self.regs,
            pinned: &self.pins.values,
            native_args: &[],
            fresh: &self.fresh,
            reserved: LogicalBytes(0),
        };
        self.heap
            .reserve_logical(LogicalBytes(bytes), &transient)
            .map_err(|error| self.heap_halt(error))
    }

    pub(super) fn alloc(&mut self, object: ObjectInit) -> Result<EntityId, Halt> {
        self.publish_holdings()?;
        let id = self
            .heap
            .allocate_object(object)
            .map_err(|error| self.heap_halt(error))?;
        self.fresh.push(id);
        Ok(id)
    }

    /// The pin anchor: keeps the value in register `r`, which the
    /// statement made, live until the statement ends, and reserves its
    /// bytes.
    #[inline(always)]
    pub(super) fn pin(&mut self, r: Reg) -> Result<(), Halt> {
        let bytes = self.pins.pin(&self.regs[r]);
        self.reserve(bytes)
    }

    /// The pin anchor on a value the statement holds outside the
    /// registers.
    pub(super) fn pin_value(&mut self, value: &Value) -> Result<(), Halt> {
        let bytes = self.pins.pin(value);
        self.reserve(bytes)
    }

    /// Reads field `field` of the record or the error in `target` into
    /// `out` (`K-FORM-009`).
    pub(super) fn read_field(&mut self, target: Reg, field: &str, out: Reg) -> Eval<()> {
        let value = match &self.regs[target] {
            Value::Record(id) => self
                .heap
                .record_get(*id, FieldName::Static(field))?
                .unwrap_or(Value::Absent),
            Value::Error(error) => match field {
                "kind" => Value::Text(error.kind.clone()),
                "message" => Value::Text(error.message.clone()),
                "data" => self.heap.error_data(error)?,
                _ => Value::Absent,
            },
            _ => return raise("type_error", "only a record or an error has fields"),
        };
        self.put(out, value);
        Ok(())
    }

    pub(super) fn read_index(&mut self, target: Reg, index: Reg, out: Reg) -> Eval<()> {
        let index = &self.regs[index];
        let value = match &self.regs[target] {
            Value::Tuple(tuple) => {
                let element = match position(index)? {
                    Some(position) => self.heap.tuple_get(tuple, position as u64)?,
                    None => None,
                };
                match element {
                    Some(value) => value,
                    None => {
                        return raise(
                            "index_out_of_range",
                            format!("no element there; the length is {}", tuple.summary.len),
                        );
                    }
                }
            }
            Value::List(id) => {
                let element = match position(index)? {
                    Some(position) => self.heap.list_get(*id, position as u64)?,
                    None => None,
                };
                match element {
                    Some(value) => value,
                    None => {
                        return raise(
                            "index_out_of_range",
                            format!("no element there; the length is {}", self.heap.len(*id)?),
                        );
                    }
                }
            }
            Value::Record(id) => {
                let Value::Text(field) = index else {
                    return raise("type_error", "a record index must be text");
                };
                self.heap
                    .record_get(*id, FieldName::Text(field))?
                    .unwrap_or(Value::Absent)
            }
            Value::Map(id) => {
                self.key(index)?;
                match self.heap.map_get(*id, index)? {
                    Some(value) => value,
                    None => return raise("key_missing", "the map has no entry under that key"),
                }
            }
            Value::Set(id) => {
                self.key(index)?;
                Value::Bool(self.heap.set_contains(*id, index)?)
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a tuple, a record, a map or a set is read by index",
                );
            }
        };
        self.put(out, value);
        Ok(())
    }

    pub(super) fn key(&self, value: &Value) -> Eval<()> {
        if self.heap.normal_key(value)?.is_some() {
            Ok(())
        } else {
            raise(
                "invalid_key",
                "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
            )
        }
    }

    pub(super) fn write_field(&mut self, target: Reg, field: &str, value: Reg) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to assign");
        };
        let value = self.take(value);
        self.heap
            .record_write(record, FieldName::Static(field), value)?;
        Ok(())
    }

    pub(super) fn write_index(&mut self, target: Reg, index: Reg, value: Reg) -> Eval<()> {
        match self.regs[target] {
            Value::List(list) => {
                let length = self.heap.len(list)?;
                let position =
                    position(&self.regs[index])?.filter(|position| *position as u64 <= length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} is assigned at 0 to {length}"),
                    );
                };
                let value = self.take(value);
                self.heap.list_write(list, position as u64, value)?;
            }
            Value::Record(record) => {
                let Value::Text(field) = self.take(index) else {
                    return raise("type_error", "a record index must be text");
                };
                let value = self.take(value);
                self.heap
                    .record_write(record, FieldName::Text(&field), value)?;
            }
            Value::Map(map) => {
                self.key(&self.regs[index])?;
                let key = self.take(index);
                let value = self.take(value);
                self.heap.map_write(map, key, value)?;
            }
            Value::Set(set) => {
                self.key(&self.regs[index])?;
                let Value::Bool(present) = self.regs[value] else {
                    return raise(
                        "type_error",
                        "a set's member is assigned `true` to add it or `false` to remove it",
                    );
                };
                let member = self.take(index);
                self.heap.set_write(set, member, present)?;
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a record, a map or a set is assigned by index",
                );
            }
        }
        Ok(())
    }

    pub(super) fn remove_field(&mut self, target: Reg, field: &str) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to remove");
        };
        self.heap.record_remove(record, FieldName::Static(field))?;
        Ok(())
    }

    pub(super) fn remove_index(&mut self, target: Reg, index: Reg) -> Eval<()> {
        match self.regs[target] {
            Value::List(list) => {
                let length = self.heap.len(list)?;
                let position =
                    position(&self.regs[index])?.filter(|position| (*position as u64) < length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} has no element there to remove"),
                    );
                };
                self.heap.list_remove(list, position as u64)?;
            }
            Value::Record(record) => {
                let Value::Text(field) = &self.regs[index] else {
                    return raise("type_error", "a record index must be text");
                };
                self.heap.record_remove(record, FieldName::Text(field))?;
            }
            Value::Map(table) | Value::Set(table) => {
                self.key(&self.regs[index])?;
                self.heap.table_remove(table, &self.regs[index])?;
            }
            _ => {
                return raise(
                    "type_error",
                    "only a list, a record, a map or a set has an index to remove",
                );
            }
        }
        Ok(())
    }

    /// Calls library function `lib` with the `count` registers from `args`
    /// as its arguments: the one entry of every library call. A native
    /// implementation or a machine function answers at once, charged by
    /// its formula: `Done`, its value in `out`. A function that runs its
    /// kernel body has that body's frame pushed, its arguments moved in, as
    /// `entry` says: `Step`.
    #[expect(
        clippy::too_many_arguments,
        reason = "a call is its function, its argument registers, its result register and its entry"
    )]
    pub(super) fn call_lib(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        count: usize,
        out: Reg,
        entry: Entry,
    ) -> Status {
        let function = exe.lib(lib);
        if self
            .answer(task, exe, lib, args, count, out)
            .map_err(|interrupt| interrupt.in_function(&function.definition.name))?
        {
            return Ok(Flow::Done);
        }
        let LibRun::Body(code) = &function.run else {
            return Err(fault("a function with no body was run as one").into());
        };
        // An argument the call leaves out is absent: the body binds every
        // parameter.
        let call = Call::new(*code, args, function.arity).of_library(lib);
        match entry {
            Entry::Frame(awaiting) => self.push_frame(task, exe, call.awaited_by(awaiting))?,
            Entry::Inline => {
                if self.inline_depth >= self.limits.inline_depth() {
                    return Err(
                        bound(Bound::CallDepth, u64::from(self.limits.inline_depth())).into(),
                    );
                }
                self.push_frame(task, exe, call.inline())
                    .map_err(|halt| halt.in_function(&function.definition.name))?;
            }
        }
        Ok(Flow::Step)
    }

    /// Answers a call of a library function that needs no frame: a native
    /// implementation or one of the machine's own, its value in `out`.
    /// `false` leaves the arguments from `args`, one per parameter, for a
    /// function that runs its kernel body.
    fn answer(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        given: usize,
        out: Reg,
    ) -> Eval<bool> {
        let function = exe.lib(lib);
        if given > function.arity {
            return raise(
                "arity",
                format!(
                    "`{}` takes {} argument(s); {} given",
                    function.definition.name, function.arity, given
                ),
            );
        }
        for absent in args + given..args + function.arity {
            self.put(absent, Value::Absent);
        }
        let end = args + function.arity;
        // A function that runs its body is observed by its frame; one that
        // does not, from here to its charge. A halt ends the run and its
        // observation with it.
        if self.observer.is_some() && !matches!(function.run, LibRun::Body(_)) {
            self.observe(Event::NativeEnter { task, lib });
        }
        let result = match &function.run {
            LibRun::Body(_) => return Ok(false),
            LibRun::Machine(MachineFunction::Deref) => match self.regs.get(args) {
                Some(Value::Ref(Identity::Task(task))) => Ok(Value::Task(*task)),
                Some(Value::Ref(Identity::Object(object))) => self
                    .heap
                    .value_of(*object)?
                    .ok_or_else(|| super::error("type_error", "the ref names no object")),
                _ => Err(super::error("type_error", "`deref` takes a ref")),
            },
            LibRun::Machine(MachineFunction::TasksUnfinished) => {
                let mut unfinished = Vec::new();
                self.tasks.visit(&mut |id, other| {
                    if id != task && !matches!(other.state, TaskStateRecord::Ended(_)) {
                        unfinished.push(Value::Task(id));
                    }
                    true
                })?;
                Ok(Value::List(self.alloc(ObjectInit::List(unfinished))?))
            }
            LibRun::Native(native) => {
                self.publish_holdings()?;
                if let Some((prepared, fast)) = &function.prepared {
                    // A result too deep to keep is the general call's to raise.
                    if let Some(value) = (prepared.entry)(&self.regs[args..end])
                        .filter(|value| within_depth(value, self.limits.value_depth()))
                    {
                        // What the general call does with its result: keep
                        // it, then charge the formula.
                        self.observe(Event::NativeFast);
                        self.pin_value(&value)?;
                        let units = if self.charging && function.native {
                            match fast.charge(&function.charge, &self.regs[args..end], &value) {
                                Charge::Units(units) => units,
                                Charge::Plan(plan) => self.in_formula(lib, true, |machine| {
                                    machine.formula(plan, &machine.regs[args..end], Some(&value))
                                })?,
                            }
                        } else {
                            0
                        };
                        self.charge_call(exe, lib, units)?;
                        self.observe(Event::NativeExit);
                        self.put(out, value);
                        return Ok(true);
                    }
                }
                let limit = function
                    .limit
                    .as_ref()
                    .map(|limit| {
                        self.in_formula(lib, true, |machine| {
                            machine.formula(limit, &machine.regs[args..end], None)
                        })
                    })
                    .transpose()?;
                let mut counter = WorkCounter::new(limit);
                let answer = native.call(NativeCall {
                    args: &self.regs[args..end],
                    heap: &mut self.heap,
                    counter: &mut counter,
                });
                self.observe(Event::GuardWork(counter.spent()));
                match answer {
                    Ok(value) => {
                        if !within_depth(&value, self.limits.value_depth()) {
                            Err(super::error(
                                "too_deep",
                                format!(
                                    "a value exceeds the configured {}-level value depth",
                                    self.limits.value_depth()
                                ),
                            ))
                        } else {
                            self.pin_value(&value)?;
                            Ok(value)
                        }
                    }
                    Err(NativeError::Raised(error)) => Err(Value::Error(error)),
                    Err(NativeError::Guard(guard)) => {
                        return Err(bound(
                            Bound::Guard {
                                function: function.id,
                            },
                            guard.limit,
                        )
                        .into());
                    }
                    Err(NativeError::Memory) => {
                        return Err(bound(Bound::Memory, self.bounds.memory).into());
                    }
                    Err(NativeError::Heap(error)) => return Err(Halt::Heap(error).into()),
                }
            }
        };
        let evaluates = self.charging && function.native;
        let units = self.in_formula(lib, evaluates, |machine| {
            machine.call_units(exe, lib, &machine.regs[args..end], result.as_ref().ok())
        })?;
        self.charge_call(exe, lib, units)?;
        self.observe(Event::NativeExit);
        match result {
            Ok(value) => {
                self.put(out, value);
                Ok(true)
            }
            Err(value) => Err(Interrupt::raised(value)),
        }
    }

    /// Runs the kernel body [`Self::call_lib`] entered inside an expression
    /// to its end, its value in `out`. Such a body stands beside a native
    /// implementation and cannot wait (`K-LIB-004`).
    pub(super) fn run_inline(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        lib: LibId,
        out: Reg,
    ) -> Eval<()> {
        let name = &exe.lib(lib).definition.name;
        self.inline_depth += 1;
        // The body's statements run their windows from the call's value on,
        // above every register the caller's statement still holds.
        let top = std::mem::replace(&mut self.top, out);
        let result = loop {
            if let Some(result) = self.inline_result.take() {
                break result;
            }
            let outcome = self.advance(task, host, exe);
            let settled = self.settle(task, host, exe, outcome);
            if let Err(halt) = settled {
                self.inline_depth -= 1;
                self.top = top;
                return Err(halt.in_function(name).into());
            }
            if self.current != Some(task) {
                self.inline_depth -= 1;
                self.top = top;
                return Err(fault("a library body waited inside an expression").into());
            }
        };
        self.inline_depth -= 1;
        self.top = top;
        self.refresh_charging(task, exe)?;
        match result {
            Ok(value) => {
                self.put(out, value);
                self.pin(out).map_err(|halt| halt.in_function(name))?;
                Ok(())
            }
            Err(value) => Err(Interrupt::raised(value)),
        }
    }

    /// Starts a `for` over the value in `iterable` (`K-ITER-002`): raises
    /// `type_error` unless it is a list, a tuple, a map or a set, then takes
    /// the first iteration step.
    pub(super) fn for_start(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: StmtId,
        iterable: Reg,
    ) -> Status {
        let cursor = match self.take(iterable) {
            Value::List(id) => Cursor::List(id, 0),
            Value::Tuple(items) => Cursor::Tuple(items, 0),
            value @ (Value::Map(_) | Value::Set(_)) => Cursor::Table(value, None),
            _ => {
                return raise(
                    "type_error",
                    "`for` iterates a list, a tuple, a map or a set",
                );
            }
        };
        self.frame(task)?.control.push(Control::For {
            stmt,
            cursor,
            started: 0,
        });
        self.next_iteration(task, host, exe)
    }

    /// The iteration step: tests the innermost loop's continuation and
    /// starts its next iteration, `Step`, or ends it, `Done` (`K-ITER-002`,
    /// `K-ITER-003`, `K-FORM-013`).
    pub(super) fn next_iteration(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Status {
        self.charge(self.costs.loop_test)?;
        // Borrowed by field, so that the loop's position can be read
        // against the heap.
        let Some(frame) = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("a running task is missing"))?
            .frames
            .last_mut()?
        else {
            return Err(fault("a running task has no frame").into());
        };
        match frame.control.last_mut() {
            Some(Control::For {
                stmt,
                cursor,
                started,
            }) => {
                let Form::For { binding, body, .. } = &exe.stmt(*stmt).form else {
                    return Err(fault("a `for` loop is not at a `for`").into());
                };
                let element = match cursor {
                    Cursor::List(list, position) => {
                        let element = self.heap.list_get(*list, *position as u64)?;
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Tuple(items, position) => {
                        let element = self.heap.tuple_get(items, *position as u64)?;
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Table(table, last) => {
                        let next = self.heap.table_after(
                            table.object().ok_or_else(|| {
                                lash_kernel_doc::heap::HeapFault::Malformed(
                                    "table cursor has no identity".into(),
                                )
                            })?,
                            *last,
                        )?;
                        next.map(|(sequence, key)| {
                            *last = Some(sequence);
                            key
                        })
                    }
                };
                match element {
                    Some(element) => {
                        *started += 1;
                        self.push_block(task, *body)?;
                        self.bind(task, binding.at, binding.shared, element)?;
                        Ok(Flow::Step)
                    }
                    None => {
                        frame.control.pop();
                        Ok(Flow::Done)
                    }
                }
            }
            Some(Control::While { stmt, .. }) => {
                let running = Running::new(task, exe, *stmt);
                let Form::While { test, body, .. } = &running.stmt.form else {
                    return Err(fault("a `while` loop is not at a `while`").into());
                };
                let tested = self.run(host, &running, test.start as usize, self.top)?;
                let Ran::Tested(value) = tested else {
                    return Err(fault("a `while` condition gave no value").into());
                };
                match value {
                    Value::Bool(true) => {
                        if let Some(Control::While { started, .. }) =
                            self.frame(task)?.control.last_mut()
                        {
                            *started += 1;
                        }
                        self.push_block(task, *body)?;
                        Ok(Flow::Step)
                    }
                    Value::Bool(false) => {
                        self.frame(task)?.control.pop();
                        Ok(Flow::Done)
                    }
                    _ => raise("type_error", "a `while` condition must be a bool"),
                }
            }
            _ => Err(fault("no loop is innermost").into()),
        }
    }
}
