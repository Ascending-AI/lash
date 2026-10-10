//! Expressions: they never pause a task, and every operation is strict
//! (`K-EVAL-001`, `K-EVAL-006`). Each evaluates into the next register.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorValue, Float, ObjectId, TaskId, Value};

use super::ops::{Entry, Flow, Reg, key};
use super::{Eval, Halt, Interrupt, KernelMachine, fault, raise};
use crate::compile::{CodeId, Executable, Expr, LibId, Member};
use crate::heap::{ClosureObj, Key, MAX_VALUE_DEPTH, Obj, Table, within_depth};
use crate::interface::Host;

impl KernelMachine {
    /// Evaluates `expr` into the next register.
    pub(super) fn eval(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        expr: &Expr,
    ) -> Eval<()> {
        self.charge(1)?;
        let out = self.regs.len();
        match expr {
            Expr::Literal(value) => self.regs.push(value.clone()),
            Expr::Var(var) => {
                let value = self.read_var(task, exe, var)?;
                self.regs.push(value);
            }
            Expr::Member(member) => match &**member {
                Member::Field(target, field) => {
                    self.eval(task, host, exe, target)?;
                    self.read_field(out, field, out)?;
                }
                Member::Index(target, index) => {
                    self.eval(task, host, exe, target)?;
                    self.eval(task, host, exe, index)?;
                    self.read_index(out, out + 1, out)?;
                    self.regs.pop();
                }
            },
            Expr::Call { lib, args } => self.call_expr(task, host, exe, *lib, args, out)?,
            Expr::Tuple(items) => {
                self.eval_each(task, host, exe, items)?;
                self.charge((self.regs.len() - out) as u64)?;
                let tuple = Value::Tuple(self.regs.drain(out..).collect());
                if !within_depth(&tuple, MAX_VALUE_DEPTH) {
                    return raise(
                        "too_deep",
                        format!("a tuple nests more than {MAX_VALUE_DEPTH} levels"),
                    );
                }
                self.regs.push(tuple);
                self.pin(out)?;
            }
            Expr::List(items) => {
                self.eval_each(task, host, exe, items)?;
                self.charge((self.regs.len() - out) as u64)?;
                let items = self.regs.drain(out..).collect();
                let list = Value::List(self.alloc(Obj::List(items))?);
                self.regs.push(list);
            }
            Expr::Set(items) => {
                self.eval_each(task, host, exe, items)?;
                self.charge((self.regs.len() - out) as u64)?;
                let mut table = Table::default();
                for member in self.regs.drain(out..) {
                    table.insert(key(&member)?, member, Value::Null);
                }
                let set = Value::Set(self.alloc(Obj::Set(table))?);
                self.regs.push(set);
            }
            Expr::Map(entries) => {
                for (entry_key, entry_value) in entries {
                    self.eval(task, host, exe, entry_key)?;
                    self.eval(task, host, exe, entry_value)?;
                }
                self.charge(entries.len() as u64)?;
                let mut table = Table::default();
                let mut written = self.regs.drain(out..);
                while let (Some(written_key), Some(value)) = (written.next(), written.next()) {
                    table.insert(key(&written_key)?, written_key, value);
                }
                drop(written);
                let map = Value::Map(self.alloc(Obj::Map(table))?);
                self.regs.push(map);
            }
            Expr::Record(entries) => {
                for (_, value) in entries {
                    self.eval(task, host, exe, value)?;
                }
                self.charge(entries.len() as u64)?;
                let fields = entries
                    .iter()
                    .map(|(name, _)| name.clone())
                    .zip(self.regs.drain(out..))
                    .collect();
                let record = Value::Record(self.alloc(Obj::Record(fields))?);
                self.regs.push(record);
            }
            Expr::Closure(code) => {
                let closure = Value::Closure(self.closure(task, exe, *code)?);
                self.regs.push(closure);
            }
            Expr::Clock => self.regs.push(Value::Timestamp(host.clock())),
            Expr::Random => {
                // The top 53 of the 64 bits, times 2^-53 (`K-HOST-003`).
                let bits = host.random() >> 11;
                self.regs
                    .push(Value::Float(Float::new(bits as f64 / (1u64 << 53) as f64)));
            }
            Expr::Read(read) => {
                self.eval(task, host, exe, &read.0)?;
                self.eval(task, host, exe, &read.1)?;
                self.host_read(host, exe, out, out + 1, out)?;
                self.regs.pop();
            }
        }
        Ok(())
    }

    /// Evaluates `expr` and takes its value out of the registers.
    #[inline(always)]
    pub(super) fn eval_value(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        expr: &Expr,
    ) -> Eval<Value> {
        self.eval(task, host, exe, expr)?;
        Ok(self.pop()?)
    }

    fn eval_each(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        exprs: &[Expr],
    ) -> Eval<()> {
        exprs
            .iter()
            .try_for_each(|expr| self.eval(task, host, exe, expr))
    }

    /// Evaluates a call's arguments into the registers from `out` and calls
    /// the function, its value in `out`.
    fn call_expr(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        lib: LibId,
        args: &[Expr],
        out: Reg,
    ) -> Eval<()> {
        for arg in args {
            // A variable or a literal, the usual argument, is evaluated
            // here as `eval` would, without a call of its own.
            match arg {
                Expr::Literal(value) => {
                    self.charge(1)?;
                    self.regs.push(value.clone());
                }
                Expr::Var(var) => {
                    self.charge(1)?;
                    let value = self.read_var(task, exe, var)?;
                    self.regs.push(value);
                }
                _ => self.eval(task, host, exe, arg)?,
            }
        }
        match self.call_lib(task, exe, lib, out, out, Entry::Inline)? {
            Flow::Done => self.cut(out + 1),
            Flow::Step => {
                self.cut(out);
                self.run_inline(task, host, exe, lib, out)?;
            }
        }
        Ok(())
    }

    /// A closure of `code` over the cells of the variables it shares.
    fn closure(&mut self, task: TaskId, exe: &Executable, code: CodeId) -> Eval<ObjectId> {
        let frame = self.frame(task)?;
        let mut captures = Vec::new();
        for capture in &exe.code(code).captures {
            match &frame.slots[capture.outer_at as usize] {
                super::SlotState::Cell(cell) => captures.push(*cell),
                _ => return Err(fault("a captured variable has no cell").into()),
            }
        }
        Ok(self.alloc(Obj::Closure(ClosureObj { code, captures }))?)
    }

    /// `read`: reads through the handle in `handle` with the request in
    /// `request`, its answer in `out` (`K-HOST-004`).
    fn host_read(
        &mut self,
        host: &mut dyn Host,
        exe: &Executable,
        handle: Reg,
        request: Reg,
        out: Reg,
    ) -> Eval<()> {
        let Value::Handle(handle) = self.take(handle) else {
            return raise("type_error", "`read` reads through a handle");
        };
        let request = self.take(request);
        let request = self.copy_out(&request)?;
        let value = match host.read(&handle, &request) {
            Ok(answer) => self.decode(exe, &answer, &lash_kernel_doc::Type::Any)?,
            Err(error) => {
                let datum = Datum::Error(Box::new(error));
                return Err(Interrupt::Raise(self.decode(
                    exe,
                    &datum,
                    &lash_kernel_doc::Type::Any,
                )?));
            }
        };
        self.put(out, value);
        Ok(())
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
                self.pin_value(&tuple)?;
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
