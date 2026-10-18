//! The stream: the one loop that runs a statement's ops
//! (`lash_kernel_exec::Op`), each a decoded enum over explicit registers.
//!
//! A statement runs in the registers, every operation strict
//! (`K-EVAL-001`). A range ends at its last op, which completes the
//! statement, ends its step where it left the task, departs, or, for a
//! `while` condition, gives the test's value. An action nested in an
//! expression that enters a frame, waits or spawns ends the step too: the
//! statement stands at it, its evaluated operands saved as roles, and its
//! resume entry runs it on from the op after the action's (`K-EVAL-008`).

use std::sync::Arc;

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{Float, Name, NativeError, NewObject, TaskId, Value};
use lash_kernel_exec::charge::Crossed;
use lash_kernel_exec::{LocalRef, Op, Reg, RegRange, RegionIx, StepEdge};

use super::ops::Flow;
use super::{
    Completion, Control, Eval, Halt, Interrupt, KernelMachine, Refusal, SlotState, TryPhase, bound,
    fault, raise,
};
use crate::compile::{BlockId, CodeId, Executable, Form, LibId, Stmt, StmtId, Stream};
use crate::interface::{Bound, Host};

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

/// The register stack's place of `r`.
#[inline(always)]
fn at(r: Reg) -> usize {
    r.0 as usize
}

/// The error a read of a variable no scope binds raises.
pub(super) fn unbound<T>(name: &Name) -> Eval<T> {
    Err(unbound_variable(name))
}

/// The refusal of a read of a variable no scope binds.
fn unbound_variable(name: &Name) -> Interrupt {
    Interrupt::Refuse(Box::new(Refusal {
        kind: "unbound_variable".into(),
        message: format!("no variable `{name}` is bound here"),
        data: Some(name.as_str().to_owned()),
    }))
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

/// The value of `$result`, an op's inside a charge region; an exit there
/// leaves the region with the events before the op.
macro_rules! inside {
    ($machine:ident, $running:ident, $pc:ident, $result:expr) => {
        match $result {
            Ok(value) => value,
            Err(exit) => return Err($machine.exit($running, $pc, exit.into())),
        }
    };
}

/// A charge region admitted whole (`docs/design/kernel-end-state.md` §B):
/// the charge it was entered at and which region of the running stream it
/// is. Nothing it executes is charged until it closes
/// ([`KernelMachine::close_region`]) or an exit inside it leaves it
/// ([`KernelMachine::leave_region`]): each commits the entry charge plus
/// the events executed, never a suffix ahead.
#[derive(Clone, Copy, Debug)]
pub(super) struct Admitted {
    entry: u64,
    /// What the region commits when it closes: the entry plus its total.
    end: u64,
    region: RegionIx,
}

impl KernelMachine {
    /// Runs a statement's ops from `pc`. The common ops run here; the rest
    /// in [`Self::in_region`] and [`Self::run_rare`]. Every op that stands
    /// in no region closes the one it follows first; an exit inside a
    /// region commits the events before the op that left it.
    pub(super) fn run(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        mut pc: usize,
    ) -> Eval<Ran> {
        self.regs.statement(running.stmt.regs);
        let ops = running.stream.ops.as_slice();
        let task = running.task;
        loop {
            let op = ops[pc];
            pc += 1;
            match op {
                Op::Region { region, total } => self.enter_region(running, region, total, pc)?,
                Op::Const { dst, c } => {
                    let value = inside!(
                        self,
                        running,
                        pc,
                        self.literal(&running.stream.consts[c.ix()])
                    );
                    self.put(at(dst), value);
                }
                Op::Local { dst, local } => {
                    let value =
                        inside!(self, running, pc, self.read_local(task, running.exe, local));
                    self.put(at(dst), value);
                }
                Op::LetBind { local, src } => {
                    let value = self.take(at(src));
                    inside!(self, running, pc, self.bind(task, local.at, false, value));
                }
                Op::LetEnd { local } => {
                    inside!(self, running, pc, self.frame(task))
                        .set_slot(local.at, SlotState::Empty);
                }
                Op::Session { dst, name } => {
                    let name = &running.stream.names[name.ix()];
                    let value = match inside!(self, running, pc, self.session_get(name)) {
                        Some(value) => value,
                        None => return Err(self.exit(running, pc, unbound_variable(name))),
                    };
                    self.put(at(dst), value);
                }
                Op::LetBindShared { .. }
                | Op::Unbound { .. }
                | Op::Field { .. }
                | Op::Index { .. }
                | Op::MakeTuple { .. }
                | Op::MakeList { .. }
                | Op::MakeSet { .. }
                | Op::MakeMap { .. }
                | Op::MakeRecord { .. }
                | Op::Closure { .. }
                | Op::Pin { .. } => {
                    inside!(self, running, pc, self.in_region_rare(running, op));
                }
                Op::Nested { act, args } => {
                    self.close_region(running.stream);
                    let nested = &running.stream.nested[act.ix()];
                    if nested.operator
                        && let crate::compile::NestedKind::Body(lib) = nested.kind
                    {
                        if self.nested_operator(running, act, lib, at(args.start))? == Flow::Step {
                            return Ok(Ran::Step);
                        }
                    } else if let Some(ran) = self.run_rare(host, running, op)? {
                        return Ok(ran);
                    }
                }
                Op::Operator { lib, args } => {
                    self.close_region(running.stream);
                    let lib = LibId(lib.0);
                    let start = at(args.start);
                    match self
                        .operator(task, running.exe, lib, start, start)
                        .map_err(|interrupt| interrupt.answering(running.exe.lib(lib)))?
                    {
                        super::ops::Answer::Done => {}
                        _ => {
                            return Err(fault(
                                "a call that runs its body was lowered as an operator that answers",
                            )
                            .into());
                        }
                    }
                }
                Op::Call { lib, args } => {
                    self.close_region(running.stream);
                    let (lib, count) = (LibId(lib.0), args.len as usize);
                    self.call_answered(task, running.exe, lib, at(args.start), count)?;
                }
                Op::WriteSession { name, src } => {
                    self.close_region(running.stream);
                    let value = self.take(at(src));
                    self.reserve(self.cost_of(&value))?;
                    let name = &running.stream.names[name.ix()];
                    if !self.session_write(name, value, true)? {
                        return unbound(name);
                    }
                    return Ok(Ran::Done);
                }
                Op::Jump { skip } => {
                    self.close_region(running.stream);
                    pc += skip as usize;
                }
                Op::JumpIfFalse { cond, skip } => {
                    self.close_region(running.stream);
                    match self.take(at(cond)) {
                        Value::Bool(true) => {}
                        Value::Bool(false) => pc += skip as usize,
                        _ => return raise("type_error", "an `if` condition must be a bool"),
                    }
                }
                Op::Bind { local, src } => {
                    self.close_region(running.stream);
                    let value = self.take(at(src));
                    self.bind(task, local.at, false, value)?;
                    return Ok(Ran::Done);
                }
                Op::WriteLocal { local, src } => {
                    self.close_region(running.stream);
                    let value = self.take(at(src));
                    self.write_local(task, running.exe, local, value)?;
                    return Ok(Ran::Done);
                }
                Op::Branch { cond, then_, else_ } => {
                    self.close_region(running.stream);
                    let block = match self.take(at(cond)) {
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
                    self.close_region(running.stream);
                    let Value::Bool(tested) = self.take(at(cond)) else {
                        return raise("type_error", "a `while` condition must be a bool");
                    };
                    return Ok(Ran::Tested(tested));
                }
                Op::End => {
                    self.close_region(running.stream);
                    return Ok(Ran::Done);
                }
                op => {
                    self.close_region(running.stream);
                    if let Some(ran) = self.run_rare(host, running, op)? {
                        return Ok(ran);
                    }
                }
            }
        }
    }

    /// Admits charge region `region`, headed at the op before `pc` with
    /// its `total`, once the one before it is closed. A region that cannot
    /// pass the bound or reach the slice's end runs whole, charged when it
    /// closes (`RegionSummary::admit`); one that passes the bound replays
    /// as the reference charges it ([`Self::replay`]).
    #[inline(always)]
    fn enter_region(
        &mut self,
        running: &Running<'_>,
        region: RegionIx,
        total: u32,
        pc: usize,
    ) -> Eval<()> {
        self.close_region(running.stream);
        if !self.charging {
            if self.observer.is_some() {
                let entry = self.charged;
                self.region = Some(Admitted {
                    entry,
                    end: entry,
                    region,
                });
            }
            return Ok(());
        }
        let entry = self.charged;
        match entry.checked_add(u64::from(total)) {
            Some(end) if end <= self.bounds.charge && end < self.slice_end && total < u32::MAX => {
                self.region = Some(Admitted { entry, end, region });
                Ok(())
            }
            _ => self.admit_slowly(running, region, entry, pc),
        }
    }

    /// A region that may pass the bound or reach the slice's end: it
    /// replays when an event passes the bound, and otherwise runs whole.
    #[cold]
    #[inline(never)]
    fn admit_slowly(
        &mut self,
        running: &Running<'_>,
        region: RegionIx,
        entry: u64,
        pc: usize,
    ) -> Eval<()> {
        let table = &running.stream.regions;
        if let Some((event, crossed)) = table.crossing(region.0, entry, self.bounds.charge) {
            return self.replay(running, region, entry, event, crossed, pc);
        }
        let end = table.commit(region.0, entry, usize::MAX);
        self.region = Some(Admitted { entry, end, region });
        Ok(())
    }

    /// Runs region `region`, entered charged `entry`, up to its event
    /// `event`, which passes the bound: the ops before it as the reference
    /// runs them, an exit among them charged with the events before it;
    /// then the run ends charged as the reference charges `crossed`, with
    /// the events before it and nothing after.
    #[cold]
    #[inline(never)]
    fn replay(
        &mut self,
        running: &Running<'_>,
        region: RegionIx,
        entry: u64,
        event: usize,
        crossed: Crossed,
        pc: usize,
    ) -> Eval<()> {
        let table = &running.stream.regions;
        let stop = table.at(region.0)[event] as usize;
        self.region = Some(Admitted {
            entry,
            end: crossed.charged,
            region,
        });
        for op_at in pc..stop {
            let op = running.stream.ops[op_at];
            if let Err(exit) = self.in_region(running, op) {
                self.leave_region(running.stream, op_at as u32);
                return Err(exit);
            }
        }
        self.region = None;
        if self.observer.is_some() {
            let events = table.events(region.0);
            self.observe_region(&events[..event]);
            self.observe_crossing(events[event], crossed.unit);
        }
        self.charged = crossed.charged;
        Err(bound(Bound::Charge, self.bounds.charge).into())
    }

    /// Closes the region the running statement stands in: it is charged
    /// whole.
    #[inline(always)]
    pub(super) fn close_region(&mut self, stream: &Stream) {
        if let Some(admitted) = self.region.take() {
            self.charged = admitted.end;
            if self.observer.is_some() {
                self.observe_region(stream.regions.events(admitted.region.0));
            }
        }
    }

    /// The exit of the op before `pc`, inside the running statement's
    /// region: the region is left there.
    #[cold]
    #[inline(never)]
    fn exit(&mut self, running: &Running<'_>, pc: usize, exit: Interrupt) -> Interrupt {
        self.leave_region(running.stream, pc as u32 - 1);
        exit
    }

    /// Leaves the region the running statement stands in at an exit at op
    /// `op`: it is charged with the events before it, never a suffix
    /// ahead.
    pub(super) fn leave_region(&mut self, stream: &Stream, op: u32) {
        if let Some(admitted) = self.region.take() {
            let table = &stream.regions;
            let executed = table.executed(admitted.region.0, op);
            if self.charging {
                self.charged = table.commit(admitted.region.0, admitted.entry, executed);
            }
            if self.observer.is_some() {
                self.observe_region(&table.events(admitted.region.0)[..executed]);
            }
        }
    }

    /// The ops that stand in a charge region: they charge nothing of their
    /// own. The common ones run here; the rest in
    /// [`Self::in_region_rare`].
    #[inline(always)]
    fn in_region(&mut self, running: &Running<'_>, op: Op) -> Eval<()> {
        let task = running.task;
        match op {
            Op::Const { dst, c } => {
                let value = self.literal(&running.stream.consts[c.ix()])?;
                self.put(at(dst), value);
            }
            Op::Local { dst, local } => {
                let value = self.read_local(task, running.exe, local)?;
                self.put(at(dst), value);
            }
            Op::LetBind { local, src } => {
                let value = self.take(at(src));
                self.bind(task, local.at, false, value)?;
            }
            Op::LetEnd { local } => {
                self.frame(task)?.set_slot(local.at, SlotState::Empty);
            }
            Op::Session { dst, name } => {
                let name = &running.stream.names[name.ix()];
                let value = match self.session_get(name)? {
                    Some(value) => value,
                    None => return unbound(name),
                };
                self.put(at(dst), value);
            }
            op => self.in_region_rare(running, op)?,
        }
        Ok(())
    }

    #[inline(never)]
    fn in_region_rare(&mut self, running: &Running<'_>, op: Op) -> Eval<()> {
        let (task, exe, stream) = (running.task, running.exe, running.stream);
        match op {
            Op::Unbound { name, .. } => unbound(&stream.names[name.ix()]),
            Op::LetBindShared { local, src } => {
                let value = self.take(at(src));
                Ok(self.bind(task, local.at, true, value)?)
            }
            Op::Field { at: r, field } => {
                let r = at(r);
                self.read_field(r, &stream.fields[field.ix()], r)
            }
            Op::Index { at: r, site } => {
                let r = at(r);
                let site = crate::compile::ReadSite::new(site.0, stream.layout);
                self.read_index(r, r + 1, r, site)
            }
            Op::MakeTuple { items } => self.make_tuple(items),
            Op::MakeList { items } => self.make_list(items),
            Op::MakeSet { items } => self.make_set(items),
            Op::MakeMap { entries } => self.make_map(entries),
            Op::MakeRecord { fields, values } => {
                let names = stream.field_lists[fields.ix()]
                    .iter()
                    .map(|name| name.as_str());
                self.make_record(names, values)
            }
            Op::Closure { dst, code } => {
                let closure = Value::Closure(self.closure(task, exe, CodeId(code.0))?);
                self.put(at(dst), closure);
                Ok(())
            }
            Op::Pin { src } => Ok(self.pin(at(src))?),
            _ => Err(fault("an op that stands in no charge region ran in one").into()),
        }
    }

    /// The ops [`Self::run`] leaves out of its loop: `Some` ends the
    /// range.
    #[inline(never)]
    fn run_rare(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        op: Op,
    ) -> Eval<Option<Ran>> {
        let (task, exe, stream) = (running.task, running.exe, running.stream);
        let ran = match op {
            Op::Clock { dst } => {
                self.report_ran(host);
                let now = host.clock();
                let now = self.arena.borrow_mut().timestamp(now.nanoseconds);
                self.put(at(dst), now);
                return Ok(None);
            }
            Op::Random { dst } => {
                // The top 53 of the 64 bits, times 2^-53 (`K-HOST-003`).
                self.report_ran(host);
                let bits = host.random() >> 11;
                let value = Value::Float(Float::new(bits as f64 / (1u64 << 53) as f64));
                self.put(at(dst), value);
                return Ok(None);
            }
            Op::HostRead { at: r } => {
                let r = at(r);
                self.host_read(host, exe, r, r + 1, r)?;
                return Ok(None);
            }
            Op::Act => {
                let Form::Action(action) = &running.stmt.form else {
                    return Err(fault("an action op stands in no action statement").into());
                };
                if self.act(task, host, exe, running.id, action, 0)? == Flow::Step {
                    return Ok(Some(Ran::Step));
                }
                return Ok(None);
            }
            Op::Nested { act, args } => {
                if self.nested_act(host, running, act, args)? == Flow::Step {
                    return Ok(Some(Ran::Step));
                }
                return Ok(None);
            }
            Op::BindShared { local, src } => {
                let value = self.take(at(src));
                self.bind(task, local.at, true, value)?;
                Ran::Done
            }
            Op::BindSession { name, src } => {
                let value = self.take(at(src));
                self.reserve(self.cost_of(&value))?;
                self.session_write(&stream.names[name.ix()], value, false)?;
                Ran::Done
            }
            Op::WriteUnbound { name, src } => {
                let value = self.take(at(src));
                self.reserve(self.cost_of(&value))?;
                return unbound(&stream.names[name.ix()]);
            }
            Op::SetField { target, field, src } => {
                self.write_field(at(target), &stream.fields[field.ix()].name, at(src))?;
                Ran::Done
            }
            Op::SetIndex { target, src } => {
                let target = at(target);
                self.write_index(target, target + 1, at(src))?;
                Ran::Done
            }
            Op::RemoveField { target, field } => {
                self.remove_field(at(target), &stream.fields[field.ix()].name)?;
                Ran::Done
            }
            Op::RemoveIndex { target } => {
                let target = at(target);
                self.remove_index(target, target + 1)?;
                Ran::Done
            }
            Op::ForIn { iterable } => {
                ran(self.for_start(task, host, exe, running.id, at(iterable))?)
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
                let value = self.take(at(src));
                if task == TaskId::MAIN && self.task(task)?.frames.len() == 1 {
                    // The run's result leaves the run (`K-EFF-002`): a value
                    // that cannot is refused here, where a `catch` sees it.
                    self.copy_out(&value)?;
                }
                Ran::Depart(Completion::Return(Box::new(value)))
            }
            Op::ReturnVia { src } => Ran::Depart(Completion::ReturnVia {
                value: Box::new(self.take(at(src))),
                stmt: running.id,
            }),
            Op::Throw { src } => return Err(Interrupt::raised(self.take(at(src)))),
            Op::Print { src } => {
                let value = self.take(at(src));
                let datum = self.copy_out(&value)?;
                host.print(&datum);
                Ran::Done
            }
            Op::Finish { src } => {
                let value = self.take(at(src));
                return Err(Halt::Finish(Box::new(self.copy_out(&value)?)).into());
            }
            Op::Fail { src } => {
                let value = self.take(at(src));
                return Err(Halt::Fail(Box::new(self.copy_out(&value)?)).into());
            }
            Op::Region { .. }
            | Op::Const { .. }
            | Op::Local { .. }
            | Op::Session { .. }
            | Op::Unbound { .. }
            | Op::Field { .. }
            | Op::Index { .. }
            | Op::MakeTuple { .. }
            | Op::MakeList { .. }
            | Op::MakeSet { .. }
            | Op::MakeMap { .. }
            | Op::MakeRecord { .. }
            | Op::Closure { .. }
            | Op::Pin { .. }
            | Op::WriteSession { .. }
            | Op::Call { .. }
            | Op::Operator { .. }
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

    /// Takes a construction's members out of their registers, which its
    /// charge region charged (`K-CHG-002`); gives where the construction
    /// goes.
    fn members(&mut self, items: RegRange) -> Eval<(usize, Vec<Value>)> {
        let start = at(items.start);
        let members = (start..start + items.len as usize)
            .map(|r| self.take(r))
            .collect();
        Ok((start, members))
    }

    /// A list of the members in `items`, into the first of them.
    pub(super) fn make_list(&mut self, items: RegRange) -> Eval<()> {
        let (start, items) = self.members(items)?;
        let list = self.new_object(NewObject::List(items))?;
        self.put(start, list);
        Ok(())
    }

    /// A set of the members in `items`, into the first of them.
    pub(super) fn make_set(&mut self, items: RegRange) -> Eval<()> {
        let (start, items) = self.members(items)?;
        for member in &items {
            self.key(member)?;
        }
        let set = self.new_object(NewObject::Set(items))?;
        self.put(start, set);
        Ok(())
    }

    /// A map of the keys and values `entries` holds by turns, into the
    /// first of them.
    pub(super) fn make_map(&mut self, entries: RegRange) -> Eval<()> {
        let start = at(entries.start);
        let mut table = Vec::new();
        for pair in (start..start + entries.len as usize).step_by(2) {
            let written = self.take(pair);
            self.key(&written)?;
            let value = self.take(pair + 1);
            table.push((written, value));
        }
        let map = self.new_object(NewObject::Map(table))?;
        self.put(start, map);
        Ok(())
    }

    /// A record of `fields`, each with the value in its register of
    /// `values`, into the first of them.
    pub(super) fn make_record<'n>(
        &mut self,
        fields: impl Iterator<Item = &'n str>,
        values: RegRange,
    ) -> Eval<()> {
        let (start, values) = self.members(values)?;
        let fields = {
            let mut arena = self.arena.borrow_mut();
            fields.map(|name| arena.text(name)).zip(values).collect()
        };
        let record = self.new_object(NewObject::Record(fields))?;
        self.put(start, record);
        Ok(())
    }

    pub(super) fn make_tuple(&mut self, items: RegRange) -> Eval<()> {
        let (start, items) = self.members(items)?;
        let depth = self.limits.value_depth();
        let tuple = match self.arena.borrow_mut().tuple(items, depth) {
            Ok(tuple) => tuple,
            Err(lash_kernel_state::arena::Raise::TooDeep) => {
                return raise(
                    "too_deep",
                    format!("a tuple exceeds the configured {depth}-level value depth"),
                );
            }
        };
        self.put(start, tuple);
        self.pin(start)?;
        Ok(())
    }

    /// A closure of `code` over the cells of the variables it shares.
    fn closure(&mut self, task: TaskId, exe: &Executable, code: CodeId) -> Eval<ArenaHandle> {
        let frame = self.frame(task)?;
        let mut cells = Vec::new();
        for capture in &exe.code(code).captures {
            match frame.slot(capture.outer_at) {
                SlotState::Cell(cell) => cells.push(*cell),
                _ => return Err(fault("a captured variable has no cell").into()),
            }
        }
        self.make_closure(exe, code, &cells)
    }

    /// Both body and plan create the same closure at its semantic site,
    /// sharing the cells allocated when its captured variables were bound.
    /// Every closure of a code shares the one site the machine built for it.
    pub(super) fn make_closure(
        &mut self,
        exe: &Executable,
        code: CodeId,
        cells: &[ArenaHandle],
    ) -> Eval<ArenaHandle> {
        let at = code.0 as usize;
        if self.storage.closure_sites.len() <= at {
            self.storage.closure_sites.resize_with(at + 1, || None);
        }
        let site = self.storage.closure_sites[at].get_or_insert_with(|| {
            let mut site = exe.code(code).site.clone();
            site.path.pop();
            Arc::new(site)
        });
        let site = Arc::clone(site);
        let (closure, cost) = self.arena.borrow_mut().new_closure(site, cells.to_vec());
        self.grow(cost)?;
        Ok(closure)
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
        let handle = self.in_arena(|arena| match arena.body(handle)? {
            Body::Handle(handle) => Ok(handle.clone()),
            _ => Err(NativeError::Malformed("a handle names another body")),
        })?;
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
        match frame.slot(local.at) {
            SlotState::Value(value) => Ok(*value),
            SlotState::Cell(cell) => {
                let cell = *cell;
                self.in_arena(|arena| arena.cell(cell))
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
        match frame.slot(local.at) {
            SlotState::Value(old) => {
                let old = *old;
                let growth = self.cost_of(&value).saturating_sub(self.cost_of(&old));
                let frame = self.frame(task)?;
                if growth == 0 {
                    frame.set_slot(local.at, SlotState::Value(value));
                } else {
                    // The slot's old holding already pays for that much.
                    // Hold its growth before publishing or changing it.
                    self.reserve_growth(growth)?;
                    self.frame(task)?
                        .set_slot(local.at, SlotState::Value(value));
                }
            }
            SlotState::Cell(cell) => {
                let cell = *cell;
                let growth = self.in_arena(|arena| arena.cell_set(cell, value))?;
                self.grow(growth)?;
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
