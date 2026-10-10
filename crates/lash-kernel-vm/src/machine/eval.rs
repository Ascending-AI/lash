//! Expressions: they never pause a task, and every operation is strict
//! (`K-EVAL-001`, `K-EVAL-006`).

use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorValue, Float, Identity, NativeCall, NativeError, TaskId, Value, WorkCounter,
};
use num_traits::ToPrimitive;

use super::exec::Call;
use super::{
    Eval, Halt, Interrupt, KernelMachine, MAX_INLINE_DEPTH, TaskState, bound, fault, raise,
};
use crate::compile::{Charge, Executable, Expr, LibId, LibRun, Member};
use crate::functions::MachineFunction;
use crate::heap::{ClosureObj, Key, MAX_VALUE_DEPTH, NativeView, Obj, Table, within_depth};
use crate::interface::{Bound, Host};

/// The list position a value is: a number with an integral value that is
/// not negative (`K-FORM-006`). `None` is a number that is no position.
pub(super) fn position(index: &Value) -> Eval<Option<usize>> {
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
    pub(super) fn eval(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        expr: &Expr,
    ) -> Eval<Value> {
        self.charge(1)?;
        match expr {
            Expr::Literal(value) => Ok(value.clone()),
            Expr::Var(var) => self.read_var(task, exe, var),
            Expr::Member(member) => self.read_member(task, host, exe, member),
            Expr::Call { lib, args } => {
                let base = self.storage.args.len();
                let value = self.call_expr(task, host, exe, *lib, args, base);
                self.storage.args.truncate(base);
                value
            }
            Expr::Tuple(items) => {
                let members = self.eval_all(task, host, exe, items)?;
                self.charge(members.len() as u64)?;
                let tuple = Value::Tuple(members.into());
                if !within_depth(&tuple, MAX_VALUE_DEPTH) {
                    return raise(
                        "too_deep",
                        format!("a tuple nests more than {MAX_VALUE_DEPTH} levels"),
                    );
                }
                self.pin(&tuple)?;
                Ok(tuple)
            }
            Expr::List(items) => {
                let items = self.eval_all(task, host, exe, items)?;
                self.charge(items.len() as u64)?;
                Ok(Value::List(self.alloc(Obj::List(items))?))
            }
            Expr::Set(items) => {
                let members = self.eval_all(task, host, exe, items)?;
                self.charge(members.len() as u64)?;
                let mut table = Table::default();
                for member in members {
                    table.insert(key(&member)?, member, Value::Null);
                }
                Ok(Value::Set(self.alloc(Obj::Set(table))?))
            }
            Expr::Map(entries) => {
                let mut written = Vec::with_capacity(entries.len());
                for (entry_key, entry_value) in entries {
                    let entry_key = self.eval(task, host, exe, entry_key)?;
                    written.push((entry_key, self.eval(task, host, exe, entry_value)?));
                }
                self.charge(written.len() as u64)?;
                let mut table = Table::default();
                for (written_key, value) in written {
                    table.insert(key(&written_key)?, written_key, value);
                }
                Ok(Value::Map(self.alloc(Obj::Map(table))?))
            }
            Expr::Record(entries) => {
                let mut fields = Vec::with_capacity(entries.len());
                for (name, value) in entries {
                    fields.push((name.clone(), self.eval(task, host, exe, value)?));
                }
                self.charge(fields.len() as u64)?;
                Ok(Value::Record(self.alloc(Obj::Record(fields))?))
            }
            Expr::Closure(code) => {
                let frame = self.frame(task)?;
                let mut captures = Vec::new();
                for capture in &exe.code(*code).captures {
                    match &frame.slots[capture.outer_at as usize] {
                        super::SlotState::Cell(cell) => captures.push(*cell),
                        _ => return Err(fault("a captured variable has no cell").into()),
                    }
                }
                let closure = ClosureObj {
                    code: *code,
                    captures,
                };
                Ok(Value::Closure(self.alloc(Obj::Closure(closure))?))
            }
            Expr::Clock => Ok(Value::Timestamp(host.clock())),
            Expr::Random => {
                // The top 53 of the 64 bits, times 2^-53 (`K-HOST-003`).
                let bits = host.random() >> 11;
                Ok(Value::Float(Float::new(bits as f64 / (1u64 << 53) as f64)))
            }
            Expr::Read(read) => {
                let handle = self.eval(task, host, exe, &read.0)?;
                let request = self.eval(task, host, exe, &read.1)?;
                let Value::Handle(handle) = handle else {
                    return raise("type_error", "`read` reads through a handle");
                };
                let request = self.copy_out(&request)?;
                match host.read(&handle, &request) {
                    Ok(answer) => self.decode(exe, &answer, &lash_kernel_doc::Type::Any),
                    Err(error) => {
                        let datum = Datum::Error(Box::new(error));
                        Err(Interrupt::Raise(self.decode(
                            exe,
                            &datum,
                            &lash_kernel_doc::Type::Any,
                        )?))
                    }
                }
            }
        }
    }

    fn eval_all(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        exprs: &[Expr],
    ) -> Eval<Vec<Value>> {
        exprs
            .iter()
            .map(|expr| self.eval(task, host, exe, expr))
            .collect()
    }

    /// Evaluates a call's arguments onto the argument stack from `base`
    /// and calls the function. The caller takes the arguments off.
    fn call_expr(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        lib: LibId,
        args: &[Expr],
        base: usize,
    ) -> Eval<Value> {
        for arg in args {
            // A variable or a literal, the usual argument, is evaluated
            // here as `eval` would, without a call of its own.
            let value = match arg {
                Expr::Literal(value) => {
                    self.charge(1)?;
                    value.clone()
                }
                Expr::Var(var) => {
                    self.charge(1)?;
                    self.read_var(task, exe, var)?
                }
                _ => self.eval(task, host, exe, arg)?,
            };
            self.storage.args.push(value);
        }
        match self.call_library(task, exe, lib, base)? {
            Some(value) => Ok(value),
            None => self.call_inline(task, host, exe, lib, base),
        }
    }

    /// Reads a field or an index (`K-FORM-009`, `K-FORM-010`).
    fn read_member(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        member: &Member,
    ) -> Eval<Value> {
        match member {
            Member::Field(target, field) => match self.eval(task, host, exe, target)? {
                Value::Record(record) => Ok(self
                    .heap
                    .record(record)
                    .and_then(|fields| fields.iter().find(|(name, _)| name == field))
                    .map_or(Value::Absent, |(_, value)| value.clone())),
                Value::Error(error) => Ok(match field.as_str() {
                    "kind" => Value::text(error.kind.as_str()),
                    "message" => Value::text(error.message.as_str()),
                    "data" => error.data.clone(),
                    _ => Value::Absent,
                }),
                _ => raise("type_error", "only a record or an error has fields"),
            },
            Member::Index(target, index) => {
                let target = self.eval(task, host, exe, target)?;
                let index = self.eval(task, host, exe, index)?;
                let element = |items: &[Value]| match position(&index)? {
                    Some(position) if position < items.len() => Ok(items[position].clone()),
                    _ => raise(
                        "index_out_of_range",
                        format!("no element there; the length is {}", items.len()),
                    ),
                };
                match &target {
                    Value::Tuple(items) => element(items),
                    Value::List(list) => element(self.heap.list(*list).map_or(&[], Vec::as_slice)),
                    Value::Record(record) => {
                        let Value::Text(field) = &index else {
                            return raise("type_error", "a record index must be text");
                        };
                        Ok(self
                            .heap
                            .record(*record)
                            .and_then(|fields| {
                                fields.iter().find(|(name, _)| name == field.as_ref())
                            })
                            .map_or(Value::Absent, |(_, value)| value.clone()))
                    }
                    Value::Map(map) => {
                        let key = key(&index)?;
                        match self.heap.table(*map).and_then(|table| table.get(&key)) {
                            Some(value) => Ok(value.clone()),
                            None => raise("key_missing", "the map has no entry under that key"),
                        }
                    }
                    Value::Set(set) => {
                        let key = key(&index)?;
                        Ok(Value::Bool(
                            self.heap
                                .table(*set)
                                .is_some_and(|table| table.contains(&key)),
                        ))
                    }
                    _ => raise(
                        "type_error",
                        "only a list, a tuple, a record, a map or a set is read by index",
                    ),
                }
            }
        }
    }

    /// Calls a library function that needs no frame: a native
    /// implementation or one of the machine's own. Its arguments are the
    /// argument stack from `base`; `None` leaves them there, one per
    /// parameter, for a function that runs its kernel body.
    pub(super) fn call_library(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        base: usize,
    ) -> Eval<Option<Value>> {
        self.call_library_inner(task, exe, lib, base)
            .map_err(|interrupt| interrupt.in_function(&exe.lib(lib).definition.name))
    }

    fn call_library_inner(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        base: usize,
    ) -> Eval<Option<Value>> {
        let function = exe.lib(lib);
        let given = self.storage.args.len() - base;
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
            self.storage
                .args
                .resize(base + function.arity, Value::Absent);
        }
        let result = match &function.run {
            LibRun::Body(_) => return Ok(None),
            LibRun::Machine(MachineFunction::Deref) => match self.storage.args.get(base) {
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
                if let Some(fast) = &function.fast {
                    let view = NativeView {
                        heap: &mut self.heap,
                        bound: self.bounds.memory,
                        reserved: 0,
                    };
                    // A result too deep to keep is the general call's to
                    // raise.
                    if let Some(value) = native
                        .fast(&self.storage.args[base..], &view)
                        .filter(|value| within_depth(value, MAX_VALUE_DEPTH))
                    {
                        // What the general call does with its result: keep
                        // it, then charge the formula.
                        self.pin(&value)?;
                        let units = if self.charging && function.native {
                            let args = &self.storage.args[base..];
                            match fast.charge(&function.charge, args, &value) {
                                Charge::Units(units) => units,
                                Charge::Plan(plan) => self.formula(plan, args, Some(&value)),
                            }
                        } else {
                            0
                        };
                        self.charge_call(exe, lib, units)?;
                        return Ok(Some(value));
                    }
                }
                let limit = function
                    .limit
                    .as_ref()
                    .map(|limit| self.formula(limit, &self.storage.args[base..], None));
                let mut attempt = 0;
                loop {
                    let mut counter = WorkCounter::new(limit);
                    let mut view = NativeView {
                        heap: &mut self.heap,
                        bound: self.bounds.memory,
                        reserved: 0,
                    };
                    let call = NativeCall {
                        args: &self.storage.args[base..],
                        heap: &mut view,
                        counter: &mut counter,
                    };
                    match native.call(call) {
                        Ok(value) => {
                            if !within_depth(&value, MAX_VALUE_DEPTH) {
                                break Err(super::error(
                                    "too_deep",
                                    format!("a value nests more than {MAX_VALUE_DEPTH} levels"),
                                ));
                            }
                            // What the call reserved was room the bound
                            // had; this accounts the result itself.
                            self.pin(&value)?;
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
                            self.collect();
                        }
                        Err(NativeError::Memory) => {
                            return Err(bound(Bound::Memory, self.bounds.memory).into());
                        }
                    }
                }
            }
        };
        let units = self.call_units(exe, lib, &self.storage.args[base..], result.as_ref().ok());
        self.charge_call(exe, lib, units)?;
        match result {
            Ok(value) => Ok(Some(value)),
            Err(value) => Err(Interrupt::Raise(value)),
        }
    }

    /// Runs a library function's kernel body to its end inside an
    /// expression. Such a body stands beside a native implementation and
    /// cannot wait (`K-LIB-004`).
    fn call_inline(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        lib: LibId,
        base: usize,
    ) -> Eval<Value> {
        let LibRun::Body(code) = &exe.lib(lib).run else {
            return Err(fault("a function with no body was run as one").into());
        };
        if self.inline_depth >= MAX_INLINE_DEPTH {
            return Err(bound(Bound::CallDepth, u64::from(self.bounds.call_depth)).into());
        }
        self.push_frame(task, exe, Call::new(*code, base).of_library(lib).inline())
            .map_err(|halt| halt.in_function(&exe.lib(lib).definition.name))?;
        self.inline_depth += 1;
        let result = loop {
            if let Some(result) = self.inline_result.take() {
                break result;
            }
            let outcome = self.advance(task, host, exe);
            if let Err(halt) = self.settle(task, host, exe, outcome) {
                self.inline_depth -= 1;
                return Err(halt.in_function(&exe.lib(lib).definition.name).into());
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
                self.pin(&value)
                    .map_err(|halt| halt.in_function(&exe.lib(lib).definition.name))?;
                Ok(value)
            }
            Err(value) => Err(Interrupt::Raise(value)),
        }
    }

    /// Builds a decoded datum as a fresh graph (`K-EFF-003`).
    pub(super) fn materialise(&mut self, datum: &Datum) -> Eval<Value> {
        let invalid = || -> Halt { fault("a decoded datum holds an undecoded number") };
        Ok(match datum {
            Datum::Null => Value::Null,
            Datum::Absent => Value::Absent,
            Datum::Bool(flag) => Value::Bool(*flag),
            Datum::Int(integer) => Value::Int(integer.clone()),
            Datum::Float(float) => Value::Float(*float),
            Datum::Number(_) => return Err(invalid().into()),
            Datum::Text(text) => Value::text(text.as_str()),
            Datum::Bytes(bytes) => Value::Bytes(bytes.clone()),
            Datum::Timestamp(timestamp) => Value::Timestamp(timestamp.clone()),
            Datum::Function(name) => Value::Function(name.clone()),
            Datum::Handle(handle) => Value::Handle(Arc::new(handle.clone())),
            Datum::Error(error) => Value::Error(Arc::new(ErrorValue {
                kind: error.kind.clone(),
                message: error.message.clone(),
                data: self.materialise(&error.data)?,
            })),
            Datum::Tuple(items) => {
                let tuple = Value::Tuple(self.materialise_all(items)?.into());
                self.pin(&tuple)?;
                tuple
            }
            Datum::List(items) => {
                let items = self.materialise_all(items)?;
                Value::List(self.alloc(Obj::List(items))?)
            }
            Datum::Set(items) => {
                let mut table = Table::default();
                for member in self.materialise_all(items)? {
                    table.insert(result_key(&member)?, member, Value::Null);
                }
                Value::Set(self.alloc(Obj::Set(table))?)
            }
            Datum::Map(entries) => {
                let mut table = Table::default();
                for (entry_key, value) in entries {
                    let entry_key = self.materialise(entry_key)?;
                    let value = self.materialise(value)?;
                    table.insert(result_key(&entry_key)?, entry_key, value);
                }
                Value::Map(self.alloc(Obj::Map(table))?)
            }
            Datum::Record(fields) => {
                let mut record: Vec<(String, Value)> = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let value = self.materialise(value)?;
                    match record.iter_mut().find(|(field, _)| field == name) {
                        Some(field) => field.1 = value,
                        None => record.push((name.clone(), value)),
                    }
                }
                Value::Record(self.alloc(Obj::Record(record))?)
            }
        })
    }

    fn materialise_all(&mut self, items: &[Datum]) -> Eval<Vec<Value>> {
        items.iter().map(|item| self.materialise(item)).collect()
    }
}

/// The key a decoded member is; a result whose key is not a legal key
/// does not fit (`K-EFF-007`).
fn result_key(value: &Value) -> Eval<Key> {
    match Key::of(value) {
        Some(key) => Ok(key),
        None => raise(
            "effect_result",
            "the result has a key that is not a legal key",
        ),
    }
}
