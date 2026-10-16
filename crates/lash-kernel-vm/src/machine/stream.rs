//! The stream: the one loop that runs a statement's ops
//! (`lash_kernel_exec::Op`), each a decoded enum over explicit registers.
//!
//! A statement runs in a window of the register stack from `base`: every
//! register an op names is relative to it. Expressions never pause a task,
//! and every operation is strict (`K-EVAL-001`, `K-EVAL-006`). A range ends
//! at its last op, which completes the statement, ends its step where it
//! left the task, departs, or, for a `while` condition, gives the test's
//! value. A call inside an expression whose function runs its body ends
//! the step too: the statement stands in the call, its window kept, and
//! runs on from the op after it when the callee returns.

use std::sync::Arc;

use lash_kernel_doc::{EntityId, Float, Name, TaskId, Value};
use lash_kernel_exec::{LocalRef, Op, Reg, RegRange, StepEdge};

use super::ops::{Entry, Flow};
use super::{
    Calling, Completion, Control, Eval, Halt, Interrupt, KernelMachine, SlotState, TryPhase, bound,
    fault, raise,
};
use crate::compile::{BlockId, CodeId, Executable, Form, LibId, Stmt, StmtId, Stream};
use crate::heap::{value_bytes, within_depth};
use crate::interface::{Bound, Host};
use lash_kernel_doc::NativeHeap;
use lash_kernel_doc::heap::{ErrorRef, Payload};
use lash_kernel_state::paged::{ObjectInit, SemanticHeap};

/// How a statement's range ends.
pub(super) enum Ran {
    /// The statement completed in its own block: its step edge is next.
    Done,
    /// Having entered a block, a loop, a `try` or a frame, or waiting: the
    /// step ends where the statement left the task.
    Step,
    /// Departing: a `break`, a `continue` or a `return`.
    Depart(Completion),
    /// A `while` condition, checked before leaving the register window.
    Tested(bool),
}

/// The register window's place `r`.
#[inline(always)]
fn at(base: usize, r: Reg) -> usize {
    base + r.0 as usize
}

/// The error a read of a variable no scope binds raises.
pub(super) fn unbound<T>(name: &Name) -> Eval<T> {
    let mut error = ErrorRef::new(
        "unbound_variable",
        &format!("no variable `{name}` is bound here"),
    );
    error.data = Payload::Resident(Arc::new(Value::text(name.as_str())));
    error.cost.0 += name.as_str().len() as u64;
    Err(Interrupt::raised(Value::Error(Arc::new(error))))
}

/// The statement a stream runs: its task, executable and ops.
pub(super) struct Running<'a> {
    pub(super) task: TaskId,
    pub(super) exe: &'a Executable,
    pub(super) id: StmtId,
    pub(super) stream: &'a Stream,
    pub(super) stmt: &'a Stmt,
    /// Where the statement's step leaves its frame when it completes in its
    /// own block.
    pub(super) edge: StepEdge,
}

impl<'a> Running<'a> {
    /// Statement `id` of `task`.
    pub(super) fn new(task: TaskId, exe: &'a Executable, id: StmtId) -> Self {
        let (stream, stmt, edge) = exe.statement(id);
        Self {
            task,
            exe,
            id,
            stream,
            stmt,
            edge,
        }
    }
}

impl KernelMachine {
    /// Runs a statement's ops from `pc` in the register window from
    /// `base`. The common ops run here; the rest in [`Self::run_rare`].
    pub(super) fn run(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        mut pc: usize,
        base: usize,
    ) -> Eval<Ran> {
        let window = base + running.stmt.regs as usize;
        if self.regs.len() < window {
            self.regs.resize(window, Value::Null);
        }
        let ops = running.stream.ops.as_slice();
        let task = running.task;
        loop {
            let op = ops[pc];
            pc += 1;
            match op {
                Op::Charge { units } => self.charge_nodes(units)?,
                Op::ChargeLocal { units, dst, local } => {
                    self.charge_nodes(u32::from(units))?;
                    let value = self.read_local(task, running.exe, local)?;
                    self.put(at(base, dst), value);
                }
                Op::ChargeConst { units, dst, c } => {
                    self.charge_nodes(u32::from(units))?;
                    let value = self.literal(&running.stream.consts[c.ix()])?;
                    self.put(at(base, dst), value);
                }
                Op::Const { dst, c } => {
                    let value = self.literal(&running.stream.consts[c.ix()])?;
                    self.put(at(base, dst), value);
                }
                Op::Local { dst, local } => {
                    let value = self.read_local(task, running.exe, local)?;
                    self.put(at(base, dst), value);
                }
                Op::Call { lib, args } => {
                    let calling = Calling {
                        stmt: running.id,
                        pc,
                        base,
                        dst: at(base, args.start),
                    };
                    if self.call_expr(running, LibId(lib.0), calling, args.len)? == Flow::Step {
                        return Ok(Ran::Step);
                    }
                }
                Op::Session { dst, name } => {
                    let name = &running.stream.names[name.ix()];
                    let value = match self.session_get(name)? {
                        Some(value) => value.clone(),
                        None => return unbound(name),
                    };
                    self.put(at(base, dst), value);
                }
                Op::WriteSession { name, src } => {
                    let value = self.take(at(base, src));
                    self.reserve(value_bytes(&value))?;
                    let name = &running.stream.names[name.ix()];
                    if self.session_get(name)?.is_none() {
                        return unbound(name);
                    }
                    self.session_write(name.clone(), value)?;
                    return Ok(Ran::Done);
                }
                Op::Jump { skip } => pc += skip as usize,
                Op::JumpIfFalse { cond, skip } => match self.take(at(base, cond)) {
                    Value::Bool(true) => {}
                    Value::Bool(false) => pc += skip as usize,
                    _ => return raise("type_error", "an `if` condition must be a bool"),
                },
                Op::LetBind { local, src } | Op::LetBindShared { local, src } => {
                    let value = self.take(at(base, src));
                    self.bind(
                        task,
                        local.at,
                        matches!(op, Op::LetBindShared { .. }),
                        value,
                    )?;
                }
                Op::LetEnd { local } => {
                    self.frame(task)?.set_slot(local.at, SlotState::Empty);
                }
                Op::Bind { local, src } => {
                    let value = self.take(at(base, src));
                    self.bind(task, local.at, false, value)?;
                    return Ok(Ran::Done);
                }
                Op::WriteLocal { local, src } => {
                    let value = self.take(at(base, src));
                    self.write_local(task, running.exe, local, value)?;
                    return Ok(Ran::Done);
                }
                Op::Branch { cond, then_, else_ } => {
                    let block = match self.take(at(base, cond)) {
                        Value::Bool(true) => BlockId(then_.0),
                        Value::Bool(false) => BlockId(else_.0),
                        _ => return raise("type_error", "an `if` condition must be a bool"),
                    };
                    // An empty block would end as soon as it began.
                    if running.exe.block(block).stmts.is_empty() {
                        self.observe_empty_block(task, block, running.exe);
                        return Ok(Ran::Done);
                    }
                    self.push_block(task, block)?;
                    return Ok(Ran::Step);
                }
                Op::Test { cond } => {
                    let Value::Bool(tested) = self.take(at(base, cond)) else {
                        return raise("type_error", "a `while` condition must be a bool");
                    };
                    return Ok(Ran::Tested(tested));
                }
                Op::End => return Ok(Ran::Done),
                op => {
                    if let Some(ran) = self.run_rare(host, running, op, base)? {
                        return Ok(ran);
                    }
                }
            }
        }
    }

    /// The ops [`Self::run`] leaves out of its loop: `Some` ends the range.
    #[inline(never)]
    fn run_rare(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        op: Op,
        base: usize,
    ) -> Eval<Option<Ran>> {
        let (task, exe, stream) = (running.task, running.exe, running.stream);
        let ran = match op {
            Op::Unbound { name, .. } => return unbound(&stream.names[name.ix()]),
            Op::Field { at: r, field } => {
                let r = at(base, r);
                self.read_field(r, &stream.fields[field.ix()], r)?;
                return Ok(None);
            }
            Op::Index { at: r } => {
                let r = at(base, r);
                self.read_index(r, r + 1, r)?;
                return Ok(None);
            }
            Op::MakeTuple { items } => {
                self.make_tuple(base, items)?;
                return Ok(None);
            }
            Op::MakeList { items } => {
                let (start, items) = self.members(base, items)?;
                let list = Value::List(self.alloc(ObjectInit::List(items))?);
                self.put(start, list);
                return Ok(None);
            }
            Op::MakeSet { items } => {
                let (start, items) = self.members(base, items)?;
                for member in &items {
                    self.key(member)?;
                }
                let set = Value::Set(self.alloc(ObjectInit::Set(items))?);
                self.put(start, set);
                return Ok(None);
            }
            Op::MakeMap { entries } => {
                let start = at(base, entries.start);
                self.charge(u64::from(entries.len / 2))?;
                let mut table = Vec::new();
                for pair in (start..start + entries.len as usize).step_by(2) {
                    let written = self.take(pair);
                    self.key(&written)?;
                    let value = self.take(pair + 1);
                    table.push((written, value));
                }
                let map = Value::Map(self.alloc(ObjectInit::Map(table))?);
                self.put(start, map);
                return Ok(None);
            }
            Op::MakeRecord { fields, values } => {
                let (start, values) = self.members(base, values)?;
                let fields = stream.field_lists[fields.ix()]
                    .iter()
                    .map(|name| {
                        let Value::Text(text) = Value::text(name.as_str()) else {
                            unreachable!()
                        };
                        text
                    })
                    .zip(values)
                    .collect();
                let record = Value::Record(self.alloc(ObjectInit::Record(fields))?);
                self.put(start, record);
                return Ok(None);
            }
            Op::Closure { dst, code } => {
                let closure = Value::Closure(self.closure(task, exe, CodeId(code.0))?);
                self.put(at(base, dst), closure);
                return Ok(None);
            }
            Op::Clock { dst } => {
                self.report_ran(host);
                self.put(at(base, dst), Value::Timestamp(host.clock()));
                return Ok(None);
            }
            Op::Random { dst } => {
                // The top 53 of the 64 bits, times 2^-53 (`K-HOST-003`).
                self.report_ran(host);
                let bits = host.random() >> 11;
                let value = Value::Float(Float::new(bits as f64 / (1u64 << 53) as f64));
                self.put(at(base, dst), value);
                return Ok(None);
            }
            Op::HostRead { at: r } => {
                let r = at(base, r);
                self.host_read(host, exe, r, r + 1, r)?;
                return Ok(None);
            }
            Op::Pin { src } => {
                self.pin(at(base, src))?;
                return Ok(None);
            }
            Op::Act => {
                let Form::Action(action) = &running.stmt.form else {
                    return Err(fault("an action op stands in no action statement").into());
                };
                if self.act(task, host, exe, running.id, action, base)? == Flow::Step {
                    return Ok(Some(Ran::Step));
                }
                return Ok(None);
            }
            Op::BindShared { local, src } => {
                let value = self.take(at(base, src));
                self.bind(task, local.at, true, value)?;
                Ran::Done
            }
            Op::BindSession { name, src } => {
                let value = self.take(at(base, src));
                self.reserve(value_bytes(&value))?;
                self.session_write(stream.names[name.ix()].clone(), value)?;
                Ran::Done
            }
            Op::WriteUnbound { name, src } => {
                let value = self.take(at(base, src));
                self.reserve(value_bytes(&value))?;
                return unbound(&stream.names[name.ix()]);
            }
            Op::SetField { target, field, src } => {
                self.write_field(at(base, target), &stream.fields[field.ix()], at(base, src))?;
                Ran::Done
            }
            Op::SetIndex { target, src } => {
                let target = at(base, target);
                self.write_index(target, target + 1, at(base, src))?;
                Ran::Done
            }
            Op::RemoveField { target, field } => {
                self.remove_field(at(base, target), &stream.fields[field.ix()])?;
                Ran::Done
            }
            Op::RemoveIndex { target } => {
                let target = at(base, target);
                self.remove_index(target, target + 1)?;
                Ran::Done
            }
            Op::ForIn { iterable } => {
                ran(self.for_start(task, host, exe, running.id, at(base, iterable))?)
            }
            Op::While => {
                self.frame(task)?.control.push(Control::While {
                    stmt: running.id,
                    started: 0,
                });
                ran(self.next_iteration(task, host, exe)?)
            }
            Op::Try { body } => {
                self.frame(task)?.control.push(Control::Try {
                    stmt: running.id,
                    phase: TryPhase::Body,
                });
                self.push_block(task, BlockId(body.0))?;
                Ran::Step
            }
            Op::Break => Ran::Depart(Completion::Break),
            Op::Continue => Ran::Depart(Completion::Continue),
            Op::Return { src } => {
                let value = self.take(at(base, src));
                if task == TaskId::MAIN && self.task(task)?.frames.len() == 1 {
                    // The run's result leaves the run (`K-EFF-002`): a value
                    // that cannot is refused here, where a `catch` sees it.
                    self.copy_out(&value)?;
                }
                Ran::Depart(Completion::Return(Box::new(value)))
            }
            Op::Throw { src } => return Err(Interrupt::raised(self.take(at(base, src)))),
            Op::Print { src } => {
                let value = self.take(at(base, src));
                let datum = self.copy_out(&value)?;
                host.print(&datum);
                Ran::Done
            }
            Op::Finish { src } => {
                let value = self.take(at(base, src));
                return Err(Halt::Finish(Box::new(self.copy_out(&value)?)).into());
            }
            Op::Fail { src } => {
                let value = self.take(at(base, src));
                return Err(Halt::Fail(Box::new(self.copy_out(&value)?)).into());
            }
            Op::Charge { .. }
            | Op::ChargeLocal { .. }
            | Op::ChargeConst { .. }
            | Op::Const { .. }
            | Op::Local { .. }
            | Op::Session { .. }
            | Op::WriteSession { .. }
            | Op::Call { .. }
            | Op::Bind { .. }
            | Op::WriteLocal { .. }
            | Op::Branch { .. }
            | Op::Test { .. }
            | Op::Jump { .. }
            | Op::JumpIfFalse { .. }
            | Op::LetBind { .. }
            | Op::LetBindShared { .. }
            | Op::LetEnd { .. }
            | Op::End => return Err(fault("the stream's loop runs this op itself").into()),
        };
        Ok(Some(ran))
    }

    /// The charge anchor of `units` one-unit events (`K-CHG-002`): each is
    /// charged, then observed. Where the sum crosses the bound, the run is
    /// charged and observed up to the event that crosses, as charging them
    /// one at a time would leave it.
    #[inline(always)]
    fn charge_nodes(&mut self, units: u32) -> Result<(), Halt> {
        if self.charging {
            let charged = self.charged.saturating_add(u64::from(units));
            if charged > self.bounds.charge {
                return Err(self.cross_nodes());
            }
            self.charged = charged;
        }
        self.observe_expressions(units);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn cross_nodes(&mut self) -> Halt {
        // The events before the one that crosses stay within the bound.
        let within = self.bounds.charge.saturating_sub(self.charged);
        self.charged = self.charged.saturating_add(within).saturating_add(1);
        self.observe_expressions(u32::try_from(within).unwrap_or(u32::MAX));
        bound(Bound::Charge, self.bounds.charge)
    }

    /// Takes a construction's members out of their registers, charging
    /// them (`K-CHG-002`); gives where the construction goes.
    fn members(&mut self, base: usize, items: RegRange) -> Eval<(usize, Vec<Value>)> {
        self.charge(u64::from(items.len))?;
        let start = at(base, items.start);
        let members = (start..start + items.len as usize)
            .map(|r| self.take(r))
            .collect();
        Ok((start, members))
    }

    fn make_tuple(&mut self, base: usize, items: RegRange) -> Eval<()> {
        let (start, items) = self.members(base, items)?;
        if items
            .iter()
            .any(|value| !within_depth(value, self.limits.value_depth().saturating_sub(1)))
        {
            return raise(
                "too_deep",
                format!(
                    "a tuple exceeds the configured {}-level value depth",
                    self.limits.value_depth()
                ),
            );
        }
        let tuple = Value::Tuple(
            self.heap_write(|heap| heap.tuple(items.clone()).map_err(Interrupt::from))?,
        );
        self.put(start, tuple);
        self.pin(start)?;
        Ok(())
    }

    /// Calls `lib` inside an expression with the `count` arguments from
    /// the call's result register: `Done`, its value there, or `Step`, the
    /// statement standing in the call as `calling` says while its body's
    /// frame runs.
    fn call_expr(
        &mut self,
        running: &Running<'_>,
        lib: LibId,
        calling: Calling,
        count: u32,
    ) -> Eval<Flow> {
        let (task, exe, args) = (running.task, running.exe, calling.dst);
        let entry = Entry::Expression(calling);
        self.call_lib(task, exe, lib, args, count as usize, args, entry)
    }

    /// A closure of `code` over the cells of the variables it shares.
    fn closure(&mut self, task: TaskId, exe: &Executable, code: CodeId) -> Eval<EntityId> {
        let access = self.heap.pages().clone();
        let frame = self.frame(task)?;
        let mut captures = Vec::new();
        for capture in &exe.code(code).captures {
            match frame.slot(capture.outer_at)? {
                SlotState::Cell(cell) => captures.push((
                    lash_kernel_state::paged::NameRef::new(
                        &exe.code(code).slots[capture.inner as usize].name,
                        &access,
                    )?,
                    *cell,
                )),
                _ => return Err(fault("a captured variable has no cell").into()),
            }
        }
        let code = exe.code(code);
        let mut site = code.site.clone();
        site.path.pop();
        let site = lash_kernel_state::paged::SiteRef::new(&site, self.heap.pages())?;
        Ok(self.alloc(ObjectInit::Closure { site, captures })?)
    }

    /// `read`: reads through the handle in `handle` with the request in
    /// `request`, its answer in `out` (`K-HOST-004`).
    fn host_read(
        &mut self,
        host: &mut dyn Host,
        exe: &Executable,
        handle: usize,
        request: usize,
        out: usize,
    ) -> Eval<()> {
        let Value::Handle(handle) = self.take(handle) else {
            return raise("type_error", "`read` reads through a handle");
        };
        let request = self.take(request);
        let request = self.copy_out(&request)?;
        self.report_ran(host);
        let value = match host.read(&handle, &request) {
            Ok(answer) => self.decode(exe, &answer, &lash_kernel_doc::Type::Any)?,
            Err(error) => {
                let datum = lash_kernel_doc::Datum::Error(Box::new(error));
                return Err(Interrupt::raised(self.decode(
                    exe,
                    &datum,
                    &lash_kernel_doc::Type::Any,
                )?));
            }
        };
        self.put(out, value);
        Ok(())
    }

    /// Reads a variable of the running frame.
    #[inline(always)]
    pub(super) fn read_local(
        &mut self,
        task: TaskId,
        exe: &Executable,
        local: LocalRef,
    ) -> Eval<Value> {
        self.read_other_local(task, exe, local)
    }
    pub(super) fn read_other_local(
        &mut self,
        task: TaskId,
        exe: &Executable,
        local: LocalRef,
    ) -> Eval<Value> {
        let frame = self.frame(task)?;
        let code = frame.code;
        match frame.slot(local.at)? {
            SlotState::Value(value) => Ok(value.clone()),
            SlotState::Cell(cell) => {
                let cell = *cell;
                Ok(self.heap.variable_get(cell)?)
            }
            SlotState::Empty => unbound(&exe.code(code).slots[local.slot as usize].name),
        }
    }
    fn write_local(
        &mut self,
        task: TaskId,
        exe: &Executable,
        local: LocalRef,
        value: Value,
    ) -> Eval<()> {
        let frame = self.frame(task)?;
        let code = frame.code;
        match frame.slot(local.at)? {
            SlotState::Value(old) => {
                let growth =
                    crate::heap::value_bytes(&value).saturating_sub(crate::heap::value_bytes(old));
                if growth == 0 {
                    frame.set_slot(local.at, SlotState::Value(value));
                } else {
                    // The slot's old holding already pays for that much.
                    // Hold its growth before publishing or changing it.
                    self.reserve_held(growth)?;
                    self.frame(task)?
                        .set_slot(local.at, SlotState::Value(value));
                }
            }
            SlotState::Cell(cell) => {
                let cell = *cell;
                self.heap_write(|heap| {
                    heap.variable_write(cell, value.clone())
                        .map_err(Interrupt::from)
                })?;
            }
            SlotState::Empty => return unbound(&exe.code(code).slots[local.slot as usize].name),
        }
        Ok(())
    }
}

/// How a statement that ran an operation ends: in its block, or where the
/// operation left the task.
fn ran(flow: Flow) -> Ran {
    match flow {
        Flow::Done => Ran::Done,
        Flow::Step => Ran::Step,
    }
}
