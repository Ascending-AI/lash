//! The operations: each kernel operation's one description, which the
//! interpreter executes.
//!
//! An operation is one machine method with a register-indexed shape: it
//! reads its operands from registers, writes its result to its result
//! register, and ends in one of four ways ([`Status`]):
//!
//! - `Ok(Flow::Done)`: done, its result, if it has one, in its result
//!   register;
//! - `Ok(Flow::Step)`: it ended the task's step where it left the task: it
//!   entered a block, a loop or a frame, or the task waits;
//! - `Err(Interrupt::Raise(_))`: it raised;
//! - `Err(Interrupt::Halt(_))`: it ended the run.
//!
//! An operation that cannot step returns [`Eval`] instead. The registers
//! are the machine's register stack: the interpreter evaluates an
//! expression into the next register, and the operation over its value
//! names that register. Nothing outside a step holds a register, so a
//! parked run never holds one (`K-STMT-004`).
//!
//! The operations are the member reads and writes, the charge, memory
//! reservation and pin anchors, the library call with its charge (the one
//! entry of every library call, inside an expression or in a frame), and
//! the loop's iteration step, here, and action dispatch,
//! [`KernelMachine::act`].

use std::sync::Arc;

use lash_kernel_doc::{Identity, NativeCall, NativeError, ObjectId, TaskId, Value, WorkCounter};
use num_traits::ToPrimitive;

use super::exec::Call;
use super::{
    Control, Cursor, Eval, Halt, Interrupt, KernelMachine, TaskState, bound, fault, raise,
};
use crate::compile::{Charge, Executable, LibId, LibRun, StmtId};
use crate::functions::MachineFunction;
use crate::heap::{Key, NativeView, Obj, object_bytes, value_bytes, within_depth};
use crate::interface::{Bound, Host};
use crate::observe::Event;

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

/// The list position a value is: a number with an integral value that is
/// not negative (`K-FORM-006`). `None` is a number that is no position.
fn position(index: &Value) -> Eval<Option<usize>> {
    match index {
        Value::Int(integer) => Ok(integer.to_usize()),
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

/// The key a value is, or a raise of `invalid_key` (`K-KEY-001`).
pub(super) fn key(value: &Value) -> Eval<Key> {
    match Key::of(value) {
        Some(key) => Ok(key),
        None => raise(
            "invalid_key",
            "a key is null, a bool, a number, a text, bytes, a timestamp, a ref, or a tuple of these",
        ),
    }
}

impl KernelMachine {
    /// Writes `value` to register `out`: the next register, or one the
    /// stack holds.
    #[inline(always)]
    pub(super) fn put(&mut self, out: Reg, value: Value) {
        match self.regs.get_mut(out) {
            Some(held) => *held = value,
            None => self.regs.push(value),
        }
    }

    /// Takes the value out of register `r`, which is left null.
    #[inline(always)]
    pub(super) fn take(&mut self, r: Reg) -> Value {
        std::mem::replace(&mut self.regs[r], Value::Null)
    }

    /// Takes the value out of register `r`, removing it when it is the top
    /// one.
    #[inline(always)]
    pub(super) fn take_top(&mut self, r: Reg) -> Value {
        if r + 1 == self.regs.len()
            && let Some(value) = self.regs.pop()
        {
            return value;
        }
        self.take(r)
    }

    /// Takes the value out of the top register.
    #[inline(always)]
    pub(super) fn pop(&mut self) -> Result<Value, Halt> {
        self.regs
            .pop()
            .ok_or_else(|| fault("an expression left no value"))
    }

    /// Drops the registers from `len` up.
    #[inline(always)]
    pub(super) fn cut(&mut self, len: usize) {
        while self.regs.len() > len {
            self.regs.pop();
        }
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
        if self.heap.memory.saturating_add(bytes) > self.heap.collect_at {
            self.collect();
            if self.heap.memory.saturating_add(bytes) > self.bounds.memory {
                return Err(bound(Bound::Memory, self.bounds.memory));
            }
        }
        self.heap.memory = self.heap.memory.saturating_add(bytes);
        Ok(())
    }

    /// Allocates an object and keeps it live until the statement ends.
    pub(super) fn alloc(&mut self, object: Obj) -> Result<ObjectId, Halt> {
        let bytes = object_bytes(&object);
        let id = self.heap.insert(object);
        self.fresh.push(id);
        self.reserve(bytes)?;
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
            Value::Record(record) => self
                .heap
                .record(*record)
                .and_then(|fields| fields.iter().find(|(name, _)| name == field))
                .map_or(Value::Absent, |(_, value)| value.clone()),
            Value::Error(error) => match field {
                "kind" => Value::text(error.kind.as_str()),
                "message" => Value::text(error.message.as_str()),
                "data" => error.data.clone(),
                _ => Value::Absent,
            },
            _ => return raise("type_error", "only a record or an error has fields"),
        };
        self.put(out, value);
        Ok(())
    }

    /// Reads what `target` holds at `index` into `out` (`K-FORM-010`).
    pub(super) fn read_index(&mut self, target: Reg, index: Reg, out: Reg) -> Eval<()> {
        let index = &self.regs[index];
        let element = |items: &[Value]| match position(index)? {
            Some(position) if position < items.len() => Ok(items[position].clone()),
            _ => raise(
                "index_out_of_range",
                format!("no element there; the length is {}", items.len()),
            ),
        };
        let value = match &self.regs[target] {
            Value::Tuple(items) => element(items)?,
            Value::List(list) => element(self.heap.list(*list).map_or(&[], Vec::as_slice))?,
            Value::Record(record) => {
                let Value::Text(field) = index else {
                    return raise("type_error", "a record index must be text");
                };
                self.heap
                    .record(*record)
                    .and_then(|fields| fields.iter().find(|(name, _)| name == field.as_ref()))
                    .map_or(Value::Absent, |(_, value)| value.clone())
            }
            Value::Map(map) => {
                let key = key(index)?;
                match self.heap.table(*map).and_then(|table| table.get(&key)) {
                    Some(value) => value.clone(),
                    None => return raise("key_missing", "the map has no entry under that key"),
                }
            }
            Value::Set(set) => {
                let key = key(index)?;
                Value::Bool(
                    self.heap
                        .table(*set)
                        .is_some_and(|table| table.contains(&key)),
                )
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

    /// Writes the value in `value` to field `field` of the record in
    /// `target` (`K-FORM-005`).
    pub(super) fn write_field(&mut self, target: Reg, field: &str, value: Reg) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to assign");
        };
        let value = self.take(value);
        self.write_record_field(record, field, value)
    }

    /// Writes the value in `value` to what `target` holds at `index`
    /// (`K-FORM-006`).
    pub(super) fn write_index(&mut self, target: Reg, index: Reg, value: Reg) -> Eval<()> {
        match self.regs[target] {
            Value::List(list) => {
                let length = self.heap.list(list).map_or(0, Vec::len);
                let position = position(&self.regs[index])?.filter(|position| *position <= length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} is assigned at 0 to {length}"),
                    );
                };
                let value = self.take(value);
                self.reserve(value_bytes(&value))?;
                if let Some(Obj::List(items)) = self.heap.get_mut(list) {
                    match items.get_mut(position) {
                        Some(held) => *held = value,
                        None => items.push(value),
                    }
                }
            }
            Value::Record(record) => {
                let Value::Text(field) = self.take(index) else {
                    return raise("type_error", "a record index must be text");
                };
                let value = self.take(value);
                self.write_record_field(record, &field, value)?;
            }
            Value::Map(map) => {
                let key = key(&self.regs[index])?;
                let (index, value) = (self.take(index), self.take(value));
                self.reserve(value_bytes(&value).saturating_add(value_bytes(&index)))?;
                if let Some(Obj::Map(table)) = self.heap.get_mut(map) {
                    table.insert(key, index, value);
                }
            }
            Value::Set(set) => {
                let key = key(&self.regs[index])?;
                let Value::Bool(member) = self.regs[value] else {
                    return raise(
                        "type_error",
                        "a set's member is assigned `true` to add it or `false` to remove it",
                    );
                };
                let index = self.take(index);
                self.reserve(value_bytes(&index))?;
                if let Some(Obj::Set(table)) = self.heap.get_mut(set) {
                    if member {
                        table.insert(key, index, Value::Null);
                    } else {
                        table.remove(&key);
                    }
                }
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

    /// Field and text-index assignment share a record's insertion order.
    fn write_record_field(&mut self, record: ObjectId, field: &str, value: Value) -> Eval<()> {
        self.reserve(value_bytes(&value).saturating_add(field.len() as u64))?;
        if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
            match fields.iter_mut().find(|(name, _)| name == field) {
                Some(held) => held.1 = value,
                None => fields.push((field.to_string(), value)),
            }
        }
        Ok(())
    }

    /// Removes field `field` of the record in `target` (`K-FORM-007`).
    pub(super) fn remove_field(&mut self, target: Reg, field: &str) -> Eval<()> {
        let Value::Record(record) = self.regs[target] else {
            return raise("type_error", "only a record has fields to remove");
        };
        if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
            fields.retain(|(name, _)| name != field);
        }
        Ok(())
    }

    /// Removes what `target` holds at `index` (`K-FORM-007`).
    pub(super) fn remove_index(&mut self, target: Reg, index: Reg) -> Eval<()> {
        let index = &self.regs[index];
        match self.regs[target] {
            Value::List(list) => {
                let length = self.heap.list(list).map_or(0, Vec::len);
                let position = position(index)?.filter(|position| *position < length);
                let Some(position) = position else {
                    return raise(
                        "index_out_of_range",
                        format!("a list of {length} has no element there to remove"),
                    );
                };
                if let Some(Obj::List(items)) = self.heap.get_mut(list) {
                    items.remove(position);
                }
            }
            Value::Record(record) => {
                let Value::Text(field) = index else {
                    return raise("type_error", "a record index must be text");
                };
                let field = Arc::clone(field);
                if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
                    fields.retain(|(name, _)| name != field.as_ref());
                }
            }
            Value::Map(table) | Value::Set(table) => {
                let key = key(index)?;
                if let Some(Obj::Map(table) | Obj::Set(table)) = self.heap.get_mut(table) {
                    table.remove(&key);
                }
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

    /// Calls library function `lib` with the registers from `args` up as
    /// its arguments: the one entry of every library call. A native
    /// implementation or a machine function answers at once, charged by
    /// its formula: `Done`, its value in `out`. A function that runs its
    /// kernel body has that body's frame pushed, its arguments moved in, as
    /// `entry` says: `Step`.
    pub(super) fn call_lib(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        out: Reg,
        entry: Entry,
    ) -> Status {
        let function = exe.lib(lib);
        if self
            .answer(task, exe, lib, args, out)
            .map_err(|interrupt| interrupt.in_function(&function.definition.name))?
        {
            return Ok(Flow::Done);
        }
        let LibRun::Body(code) = &function.run else {
            return Err(fault("a function with no body was run as one").into());
        };
        let call = Call::new(*code, args).of_library(lib);
        match entry {
            Entry::Frame(awaiting) => self.push_frame(task, exe, call.awaited_by(awaiting))?,
            Entry::Inline => {
                if self.inline_depth >= self.heap.inline_depth {
                    return Err(bound(Bound::CallDepth, u64::from(self.heap.inline_depth)).into());
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
        out: Reg,
    ) -> Eval<bool> {
        let function = exe.lib(lib);
        let given = self.regs.len() - args;
        if given > function.arity {
            return raise(
                "arity",
                format!(
                    "`{}` takes {} argument(s); {} given",
                    function.definition.name, function.arity, given
                ),
            );
        }
        if given < function.arity {
            self.regs.resize(args + function.arity, Value::Absent);
        }
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
                    .value_of(*object)
                    .ok_or_else(|| super::error("type_error", "the ref names no object")),
                _ => Err(super::error("type_error", "`deref` takes a ref")),
            },
            LibRun::Machine(MachineFunction::TasksUnfinished) => {
                let unfinished = self
                    .tasks
                    .iter()
                    .enumerate()
                    .filter(|(index, other)| {
                        *index as u64 != task.0 && !matches!(other.state, TaskState::Ended(_))
                    })
                    .map(|(index, _)| Value::Task(TaskId(index as u64)))
                    .collect();
                Ok(Value::List(self.alloc(Obj::List(unfinished))?))
            }
            LibRun::Native(native) => {
                let end = self.regs.len();
                if let Some((prepared, fast)) = &function.prepared {
                    // A result too deep to keep is the general call's to raise.
                    if let Some(value) = (prepared.entry)(&self.regs[args..end])
                        .filter(|value| within_depth(value, self.heap.value_depth))
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
                                }),
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
                let limit = function.limit.as_ref().map(|limit| {
                    self.in_formula(lib, true, |machine| {
                        machine.formula(limit, &machine.regs[args..end], None)
                    })
                });
                let mut attempt = 0;
                loop {
                    let mut counter = WorkCounter::new(limit);
                    let mut view = NativeView {
                        heap: &mut self.heap,
                        bound: self.bounds.memory,
                        reserved: 0,
                    };
                    let call = NativeCall {
                        args: &self.regs[args..end],
                        heap: &mut view,
                        counter: &mut counter,
                    };
                    let answer = native.call(call);
                    self.observe(Event::GuardWork(counter.spent()));
                    match answer {
                        Ok(value) => {
                            if !within_depth(&value, self.heap.value_depth) {
                                break Err(super::error(
                                    "too_deep",
                                    format!(
                                        "a value exceeds the configured {}-level value depth",
                                        self.heap.value_depth
                                    ),
                                ));
                            }
                            // What the call reserved was room the bound
                            // had; this accounts the result itself.
                            self.pin_value(&value)?;
                            break Ok(value);
                        }
                        Err(NativeError::Raised(raised)) => {
                            break Err(Value::Error(Arc::new(raised)));
                        }
                        Err(NativeError::Guard(guard)) => {
                            let function = Bound::Guard {
                                function: function.id,
                            };
                            return Err(bound(function, guard.limit).into());
                        }
                        // A native function is a function of its arguments
                        // (`K-LIB-006`): once the heap is collected the call
                        // is made again, and fails only if what is live
                        // leaves it no room.
                        Err(NativeError::Memory) if attempt == 0 => {
                            attempt += 1;
                            self.observe(Event::NativeRetry);
                            self.collect();
                        }
                        Err(NativeError::Memory) => {
                            return Err(bound(Bound::Memory, self.bounds.memory).into());
                        }
                    }
                }
            }
        };
        let end = self.regs.len();
        let evaluates = self.charging && function.native;
        let units = self.in_formula(lib, evaluates, |machine| {
            machine.call_units(exe, lib, &machine.regs[args..end], result.as_ref().ok())
        });
        self.charge_call(exe, lib, units)?;
        self.observe(Event::NativeExit);
        match result {
            Ok(value) => {
                self.put(out, value);
                Ok(true)
            }
            Err(value) => Err(Interrupt::Raise(value)),
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
        let top = self.regs.len();
        let result = loop {
            if let Some(result) = self.inline_result.take() {
                break result;
            }
            let outcome = self.advance(task, host, exe);
            let settled = self.settle(task, host, exe, outcome);
            self.cut(top);
            if let Err(halt) = settled {
                self.inline_depth -= 1;
                return Err(halt.in_function(name).into());
            }
            if self.current != Some(task) {
                self.inline_depth -= 1;
                return Err(fault("a library body waited inside an expression").into());
            }
        };
        self.inline_depth -= 1;
        self.refresh_charging(task, exe);
        match result {
            Ok(value) => {
                self.put(out, value);
                self.pin(out).map_err(|halt| halt.in_function(name))?;
                Ok(())
            }
            Err(value) => Err(Interrupt::Raise(value)),
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
            Value::Map(id) | Value::Set(id) => Cursor::Table(id, None),
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
            .get_mut(task.0 as usize)
            .and_then(|task| task.frames.last_mut())
        else {
            return Err(fault("a running task has no frame").into());
        };
        match frame.control.last_mut() {
            Some(Control::For {
                stmt,
                cursor,
                started,
            }) => {
                let crate::compile::Stmt::For { binding, body, .. } = exe.stmt(*stmt) else {
                    return Err(fault("a `for` loop is not at a `for`").into());
                };
                let element = match cursor {
                    Cursor::List(list, position) => {
                        let element = self
                            .heap
                            .list(*list)
                            .and_then(|items| items.get(*position))
                            .cloned();
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Tuple(items, position) => {
                        let element = items.get(*position).cloned();
                        *position += usize::from(element.is_some());
                        element
                    }
                    Cursor::Table(table, last) => {
                        let next = self
                            .heap
                            .table(*table)
                            .and_then(|table| table.after(*last))
                            .map(|(sequence, key)| (sequence, key.clone()));
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
                        self.bind(task, *binding, element)?;
                        Ok(Flow::Step)
                    }
                    None => {
                        frame.control.pop();
                        Ok(Flow::Done)
                    }
                }
            }
            Some(Control::While { stmt, .. }) => {
                let crate::compile::Stmt::While {
                    condition, body, ..
                } = exe.stmt(*stmt)
                else {
                    return Err(fault("a `while` loop is not at a `while`").into());
                };
                match self.eval_value(task, host, exe, condition)? {
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
