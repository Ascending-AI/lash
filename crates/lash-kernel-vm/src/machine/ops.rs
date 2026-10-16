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
//! hold the running statement's values: each expression's value goes to
//! one and each operation reads its operands from them. No register lives
//! past its step: a statement that stands at an action nested in its
//! expression keeps its operands as its continuation's roles
//! (`K-EVAL-008`).
//!
//! The operations are the member reads and writes, the charge, memory
//! reservation and pin anchors, the library call with its charge (the one
//! entry of every library call, inside an expression or in a frame), and
//! the loop's iteration step, here, and action dispatch,
//! [`KernelMachine::act`].

use lash_kernel_doc::{EntityId, Identity, NativeCall, NativeError, TaskId, Value, WorkCounter};
use num_traits::ToPrimitive;

use super::exec::Call;
use super::{Eval, Halt, Interrupt, KernelMachine, bound, fault, raise};
use crate::compile::{Charge, CodeId, Executable, LibId, LibRun, StmtId};
use crate::functions::MachineFunction;
use crate::heap::within_depth;
use crate::interface::Bound;
use crate::observe::Event;
use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::heap::{FieldName, IntRef};
use lash_kernel_state::paged::{ObjectInit, SemanticHeap, TaskStateRecord};

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
        #[cfg(test)]
        {
            self.storage.value_moves += 1;
        }
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
        #[cfg(test)]
        {
            self.storage.value_moves += 1;
        }
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

    /// The memory anchor: holds `bytes` more memory until the statement
    /// ends, refining exactly at the collection point when the heap is due
    /// and ending the run when what is live does not leave room.
    pub(super) fn reserve(&mut self, bytes: u64) -> Result<(), Halt> {
        self.hold(bytes, false)
    }

    /// The memory anchor for what the run keeps in its frames (a frame, a
    /// bound value): held until the frames are next published, which
    /// prices them exactly (FIG-6318).
    pub(super) fn reserve_held(&mut self, bytes: u64) -> Result<(), Halt> {
        self.hold(bytes, true)
    }

    fn hold(&mut self, bytes: u64, persistent: bool) -> Result<(), Halt> {
        #[cfg(test)]
        if self.pins.force_precision {
            self.pins.reservations.push((bytes, self.charged));
            self.refine_for(bytes)?;
        }
        if self.heap.hold(bytes, persistent)? {
            return Ok(());
        }
        self.refine_for(bytes)?;
        if self.heap.hold(bytes, persistent)? {
            Ok(())
        } else {
            Err(bound(Bound::Memory, self.bounds.memory))
        }
    }

    pub(super) fn alloc(&mut self, object: ObjectInit) -> Result<EntityId, Halt> {
        let id = self.heap_write(|heap| {
            heap.allocate_object(object.clone())
                .map_err(super::write_halt)
        })?;
        self.fresh.push(id);
        Ok(id)
    }

    /// The pin anchor: keeps the value in register `r`, which the
    /// statement made, live until the statement ends, and reserves its
    /// bytes.
    #[inline(always)]
    pub(super) fn pin(&mut self, r: Reg) -> Result<(), Halt> {
        let bytes = self.pins.pin(&self.regs[r])?;
        self.reserve(bytes)
    }

    /// The pin anchor on a value the statement holds outside the
    /// registers.
    pub(super) fn pin_value(&mut self, value: &Value) -> Result<(), Halt> {
        let bytes = self.pins.pin(value)?;
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
                self.heap_write(|heap| {
                    heap.list_write(list, position as u64, value.clone())
                        .map_err(Interrupt::from)
                })?;
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
                self.heap_write(|heap| {
                    heap.map_write(map, key.clone(), value.clone())
                        .map_err(Interrupt::from)
                })?;
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
                self.heap_write(|heap| {
                    heap.set_write(set, member.clone(), present)
                        .map_err(Interrupt::from)
                })?;
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
    /// its formula: `Done`, its value in `args`. A function that runs its
    /// kernel body, its native declining or absent, has that body's frame
    /// pushed, its arguments moved in, the caller's statement `awaiting`
    /// waiting in it, if any: `Step`.
    pub(super) fn call_lib(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        count: usize,
        awaiting: Option<StmtId>,
    ) -> Status {
        let function = exe.lib(lib);
        let code = match self
            .answer(task, exe, lib, args, count, args)
            .map_err(|interrupt| interrupt.in_function(&function.definition.name))?
        {
            Answer::Done => return Ok(Flow::Done),
            Answer::Body(code) => code,
        };
        self.enter_body(task, exe, lib, code, args, awaiting)
    }

    /// Pushes the frame of library function `lib`'s kernel body `code`,
    /// its arguments the registers from `args`, the caller's statement
    /// `awaiting` waiting in it, if any: `Step`.
    pub(super) fn enter_body(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        code: CodeId,
        args: Reg,
        awaiting: Option<StmtId>,
    ) -> Status {
        let function = exe.lib(lib);
        // An argument the call leaves out is absent: the body binds every
        // parameter.
        let call = Call::new(code, args, function.arity).of_library(lib);
        self.push_frame(task, exe, call.awaited_by(awaiting))?;
        Ok(Flow::Step)
    }

    /// Calls `lib`, which answers without a frame, inside an expression,
    /// with the `count` arguments from `args`, its value there.
    pub(super) fn call_answered(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        count: usize,
    ) -> Eval<()> {
        let function = exe.lib(lib);
        match self
            .answer(task, exe, lib, args, count, args)
            .map_err(|interrupt| interrupt.in_function(&function.definition.name))?
        {
            Answer::Done => Ok(()),
            Answer::Body(_) => {
                Err(fault("a call that runs its body was lowered as one that answers").into())
            }
        }
    }

    /// Answers a call of a library function that needs no frame: a native
    /// implementation or one of the machine's own, its value in `out`.
    /// [`Answer::Body`] leaves the arguments from `args`, one per parameter,
    /// for the kernel body that runs the call: the function has no native,
    /// or its native declined.
    pub(super) fn answer(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        args: Reg,
        given: usize,
        out: Reg,
    ) -> Eval<Answer> {
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
            LibRun::Body(code) => return Ok(Answer::Body(*code)),
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
            LibRun::Native { native, body } => {
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
                        return Ok(Answer::Done);
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
                // Prepared entries above cannot touch the heap. A general
                // native starts a new frame-access segment before it can.
                self.tasks.invalidate_active();
                let answer = native.call(NativeCall {
                    args: &self.regs[args..end],
                    heap: &mut super::native_heap::CallHeap {
                        heap: &mut self.heap,
                        roots: super::native_heap::Roots {
                            tasks: &mut self.tasks,
                            joins: &self.joins,
                            registers: &self.regs,
                            pinned: &self.pins.values,
                            pinned_bytes: self.pins.bytes,
                            fresh: &self.fresh,
                            refinements: &mut self.census_refinements,
                        },
                    },
                    counter: &mut counter,
                });
                self.heap.execution_roots_moved();
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
                    Err(NativeError::Heap(error)) => return Err(Halt::from(error).into()),
                    // The body runs the call, charged by the formula when
                    // its frame is left (`K-CHG-007`).
                    Err(NativeError::Decline) => {
                        let Some(body) = body else {
                            return Err(fault("a native with no kernel body declined").into());
                        };
                        self.observe(Event::NativeExit);
                        return Ok(Answer::Body(*body));
                    }
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
                Ok(Answer::Done)
            }
            Err(value) => Err(Interrupt::raised(value)),
        }
    }
}

/// How [`KernelMachine::answer`] left a library call.
pub(super) enum Answer {
    /// Its value is in the call's result register.
    Done,
    /// The kernel body runs the call.
    Body(CodeId),
}
