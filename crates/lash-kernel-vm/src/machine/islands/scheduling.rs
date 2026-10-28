//! Scheduler instructions transfer the physical windows to the existing task
//! owner before the first scheduler mutation. Only semantic source coordinates
//! and ordered live operands cross this handoff; no physical cursor is saved.
use super::super::ops::Flow;
use super::super::stream::{Ran, Running};
use super::super::{Completion, Eval, KernelMachine, fault};
use crate::compile::{Action, ActionKind, Executable, Form, NestedKind, StmtId};
use crate::interface::Host;
use crate::machine::actions::{Target, cancelled_here};
use crate::machine::ops::{Reg, Status};
use crate::machine::{Incoming, raise};
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Op;
use lash_kernel_state::paged::PhaseRecord;
use lash_kernel_value::package::OperationMode;

impl KernelMachine {
    #[inline(always)]
    pub(super) fn schedule_instruction(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        pc: u32,
        values: Vec<(u32, Value)>,
    ) -> Result<(), super::Halt> {
        #[cfg(any(test, feature = "perf-witness"))]
        if self.islands.counting {
            return self.schedule_counted(host, task, exe, stmt, pc, values);
        }
        self.schedule_actions(host, task, exe, stmt, pc, values)
    }

    #[cfg(any(test, feature = "perf-witness"))]
    #[cold]
    #[inline(never)]
    fn schedule_counted(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        stmt: StmtId,
        pc: u32,
        values: Vec<(u32, Value)>,
    ) -> Result<(), super::Halt> {
        let _execution = crate::perf_witness::execution::Scope::enter(
            &self.islands.census,
            crate::perf_witness::execution::Owner::RegisterProgram,
            true,
        );
        self.schedule_actions(host, task, exe, stmt, pc, values)
    }

    fn schedule_actions(
        &mut self,
        host: &mut dyn Host,
        task: TaskId,
        exe: &Executable,
        mut stmt: StmtId,
        mut pc: u32,
        mut values: Vec<(u32, Value)>,
    ) -> Result<(), super::Halt> {
        loop {
            self.active_link = self.frame(task)?.link;
            self.islands.held = false;
            let running = Running::new(task, exe, stmt);
            self.regs.statement(running.stmt.regs);
            for (reg, value) in values {
                self.put(reg as usize, value);
            }
            self.regs
                .source_begin(running.stream, running.stmt, pc)
                .map_err(fault)?;
            let outcome = (|| {
                let flow = match running.stream.ops[pc as usize] {
                    Op::Act => {
                        let Form::Action(action) = &running.stmt.form else {
                            return Err(fault("a scheduler instruction has no action").into());
                        };
                        self.act(host, &running, action, 0)?
                    }
                    Op::Nested { act, args } => self.nested_act(host, &running, act, args)?,
                    _ => return Err(fault("a scheduler instruction names no source action").into()),
                };
                let ran = if flow == Flow::Step {
                    Ran::Step
                } else {
                    self.regs
                        .source_complete(running.stream, pc)
                        .map_err(fault)?;
                    match self.register_suffix(host, &running, pc as usize + 1, true)? {
                        Some(ran) => ran,
                        None => self.run(host, &running, pc as usize + 1)?,
                    }
                };
                self.resumed(task, exe, (stmt, ran))
            })();
            let next = self.islands.scheduled.take();
            self.settle(task, host, exe, outcome)?;
            match next {
                Some((owner, next)) => {
                    if owner != task {
                        return Err(fault("a scheduler suffix changed its task owner"));
                    }
                    match next {
                        super::RunsOn::Scheduling(id, at, regs) => {
                            stmt = id;
                            pc = at;
                            values = regs;
                        }
                        next => return self.run_on(host, task, exe, next),
                    }
                }
                None => return Ok(()),
            }
        }
    }

    /// A delivered action uses the same program suffix after a cold reopen.
    /// Refusal of admission leaves the already evaluated operands untouched.
    #[inline(always)]
    pub(in crate::machine) fn register_action_suffix(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        pc: usize,
    ) -> Eval<Option<Ran>> {
        self.register_suffix(host, running, pc, false)
    }

    #[inline(always)]
    fn register_suffix(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        pc: usize,
        defer: bool,
    ) -> Eval<Option<Ran>> {
        let task = running.task;
        let Some(plan) = running.exe.island_of(self.frame(task)?.code) else {
            return Ok(None);
        };
        let Some(compiled) = running
            .exe
            .island_plan(plan)
            .and_then(|program| program.compiled(running.exe))
        else {
            return Ok(None);
        };
        let Some(after) = compiled.action_after(pc) else {
            return Ok(None);
        };
        let gate = self.gate(task, host.observes_sites())?;
        let suffix = Some(super::ActionSuffix {
            stmt: running.id,
            after,
            defer,
        });
        let completion = if gate.projecting {
            self.resume_projecting::<true>(gate, task, host, running.exe, plan, suffix)?
        } else {
            self.resume_projecting::<false>(gate, task, host, running.exe, plan, suffix)?
        };
        Ok(completion.map(|completion| match completion {
            Completion::Normal => Ran::Step,
            departure => Ran::Depart(departure),
        }))
    }
}

impl KernelMachine {
    /// Action dispatch: starts the action of statement `stmt`, its
    /// arguments read into the registers from `out`. `Done` is one that
    /// completed at once, its value in `out`; `Step` is one the task now
    /// waits in, or a call whose frame now runs.
    pub(in crate::machine) fn act(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        action: &Action,
        out: Reg,
    ) -> Status {
        let (task, exe, stmt) = (running.task, running.exe, running.id);
        self.regs.source_atoms_begin(&action.site).map_err(fault)?;
        self.observe_action(exe, &action.site);
        if action.guest
            && matches!(
                action.kind,
                ActionKind::Call { .. }
                    | ActionKind::Join(_)
                    | ActionKind::JoinMany(..)
                    | ActionKind::Yield
                    | ActionKind::Cancel(_)
            )
        {
            let occurrence = self.task(task)?.counters.advance(action.occurrence)?;
            if host.observes_sites() {
                let identity = self.action_location(task, &action.site, occurrence)?;
                host.enter_site(&identity);
            }
        }
        match &action.kind {
            ActionKind::Call { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                self.put_atoms(task, exe, args, out)?;
                if let Target::Code(code, cells, link) = &target
                    && *link == self.current_link()
                    && let Some(flow) =
                        self.action_island(host, running, (*code, cells), out, args.len())?
                {
                    if let Target::Code(_, cells, _) = target {
                        self.spent_captures(cells);
                    }
                    return Ok(flow);
                }
                self.call(task, exe, stmt, target, out, args.len())
            }
            ActionKind::Spawn { callee, args } => {
                let target = self.callee(task, exe, callee)?;
                self.put_atoms(task, exe, args, out)?;
                self.spawn(task, host, exe, stmt, &action.site, target, out, args.len())
            }
            ActionKind::Perform {
                slot, args, result, ..
            } => {
                let effect = self.operation(task, *slot, &action.site.unit)?;
                let values = self.perform_atoms(task, exe, args)?;
                let mut copied = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    copied.push(self.copy_out_tool_arg(&effect, index, value)?);
                }
                if self.effect_mode(&effect)? == OperationMode::Read {
                    // A read completes here, inside its statement: no wait,
                    // and the task keeps its turn (`K-EFF-012`).
                    let value =
                        self.perform_read(task, host, exe, &effect, &copied, &action.site, result)?;
                    // Held as a delivered outcome is, until the statement
                    // binds it.
                    self.put(out, value);
                    self.pin(out)?;
                    return Ok(Flow::Done);
                }
                self.issue_effect(task, stmt, &action.site, effect, copied)?;
                Ok(Flow::Step)
            }

            ActionKind::Join(handle) => {
                let handle = self.atom(task, exe, handle)?;
                self.regs.source_atom(0, handle).map_err(fault)?;
                match self.join_one(task, handle)? {
                    Ok(value) => {
                        self.put(out, value);
                        Ok(Flow::Done)
                    }
                    Err(joined) => {
                        self.wait_one(task, stmt, joined)?;
                        Ok(Flow::Step)
                    }
                }
            }
            ActionKind::JoinMany(mode, handles) => {
                let handles = self.atom(task, exe, handles)?;
                self.regs.source_atom(0, handles).map_err(fault)?;
                match self.join_list(task, *mode, &handles)? {
                    Ok(value) => {
                        self.put(out, value);
                        Ok(Flow::Done)
                    }
                    Err(members) => {
                        self.wait_list(task, stmt, *mode, members)?;
                        Ok(Flow::Step)
                    }
                }
            }
            ActionKind::Yield => {
                let yielding = self.task(task)?;
                yielding.wake(Incoming::Value(PageValue::Null), false);
                self.frame(task)?.awaiting = Some(stmt);
                self.ready.push_back(task)?;
                self.current = None;
                Ok(Flow::Step)
            }
            ActionKind::Cancel(handle) => {
                let handle = self.atom(task, exe, handle)?;
                self.regs.source_atom(0, handle).map_err(fault)?;
                let Value::Task(target) = handle else {
                    return raise("type_error", "`cancel` takes a task handle");
                };
                if target == task {
                    return Err(cancelled_here());
                }
                self.cancel(target)?;
                self.put(out, Value::Null);
                Ok(Flow::Done)
            }
        }
    }

    /// An action nested in statement `running`'s expression, its
    /// arguments the registers `args` and its resume entry `act`
    /// (`K-EVAL-008`). `Done` is one that completed at once, its value in
    /// `args.start`. `Step` is one the task now waits in, a call whose
    /// frame now runs or a `spawn` whose child does: the frame stands at
    /// the action, the operands its statement evaluated saved as its
    /// continuation's roles.
    pub(in crate::machine) fn nested_act(
        &mut self,
        host: &mut dyn Host,
        running: &Running<'_>,
        act: lash_kernel_exec::ActIx,
        args: lash_kernel_exec::RegRange,
    ) -> Status {
        let (task, exe, stmt) = (running.task, running.exe, running.id);
        let entry = running
            .stream
            .nested
            .get(act.ix())
            .ok_or_else(|| fault("a nested op names no resume entry"))?;
        let count = args.len as usize;
        let args = args.start.0 as usize;
        let site = &entry.site;
        if !matches!(entry.kind, NestedKind::Body(_)) {
            self.observe_action(exe, site);
        }
        let library = !entry.guest;
        match &entry.kind {
            NestedKind::Call(callee) => {
                if !library {
                    self.enter(task, host, site, entry.occurrence)?;
                }
                let (target, at) = self.nested_target(exe, callee, args)?;
                let count = count - (at - args);
                let flow = match target {
                    Target::Library(lib, link) => {
                        self.active_link = link;
                        match self
                            .answer(task, exe, lib, at, count, at)
                            .map_err(|interrupt| interrupt.answering(exe.lib(lib)))?
                        {
                            crate::machine::ops::Answer::Done => Flow::Done,
                            crate::machine::ops::Answer::Body(code) => {
                                self.capture(task, entry, act, PhaseRecord::Called)?;
                                self.enter_body(
                                    task,
                                    exe,
                                    lib,
                                    code,
                                    crate::machine::exec::Invocation::new(at, exe.lib(lib).arity)
                                        .awaited_by(Some(stmt))
                                        .with_boundary(None),
                                )?
                            }
                        }
                    }
                    target @ (Target::Code(..) | Target::Export(..)) => {
                        self.capture(task, entry, act, PhaseRecord::Called)?;
                        let flow = self.call(task, exe, stmt, target, at, count)?;
                        if flow == Flow::Done {
                            // Imported code may answer natively without pushing a
                            // frame. Its caller continues in the same statement.
                            let frame = self.frame(task)?;
                            let mut pending = frame.pending.take().ok_or_else(|| {
                                fault("an inline call lost its captured operands")
                            })?;
                            frame.nested = None;
                            for (role, saved) in entry.roles.iter().zip(&pending.roles) {
                                self.put(role.reg.0 as usize, saved.value);
                            }
                            pending.roles.clear();
                            self.storage.roles.push(pending.roles);
                        }
                        flow
                    }
                };
                self.active_link = self.frame(task)?.link;
                Ok(self.answered_at(flow, at, args))
            }
            // An operation's native answers at once; only a call whose body
            // runs, its native declining or absent, stands at the action.
            NestedKind::Body(lib) => {
                let answer = self
                    .answer(task, exe, *lib, args, count, args)
                    .map_err(|interrupt| interrupt.answering(exe.lib(*lib)))?;
                self.body_answered(running, act, *lib, args, answer)
            }
            NestedKind::Spawn(callee) => {
                let (target, at) = self.nested_target(exe, callee, args)?;
                let flow =
                    self.spawn(task, host, exe, stmt, site, target, at, count - (at - args))?;
                if flow == Flow::Step {
                    self.capture(task, entry, act, PhaseRecord::Spawned)?;
                }
                Ok(self.answered_at(flow, at, args))
            }
            NestedKind::Perform { slot, result, .. } => {
                let effect = self.operation(task, *slot, &site.unit)?;
                let mut copied = Vec::with_capacity(count);
                for at in args..args + count {
                    let value = self.take(at);
                    copied.push(self.copy_out_tool_arg(&effect, at - args, &value)?);
                }
                if self.effect_mode(&effect)? == OperationMode::Read {
                    // A read completes here, inside its statement, and the
                    // task keeps its turn (`K-EFF-012`).
                    let value =
                        self.perform_read(task, host, exe, &effect, &copied, site, result)?;
                    self.put(args, value);
                    self.pin(args)?;
                    return Ok(Flow::Done);
                }
                self.capture(task, entry, act, PhaseRecord::Waiting)?;
                self.issue_effect(task, stmt, site, effect, copied)?;
                Ok(Flow::Step)
            }
            NestedKind::Join => {
                if !library {
                    self.enter(task, host, site, entry.occurrence)?;
                }
                let handle = self.take(args);
                match self.join_one(task, handle)? {
                    Ok(value) => {
                        self.put(args, value);
                        Ok(Flow::Done)
                    }
                    Err(joined) => {
                        self.capture(task, entry, act, PhaseRecord::Joining)?;
                        self.wait_one(task, stmt, joined)?;
                        Ok(Flow::Step)
                    }
                }
            }
            NestedKind::JoinMany(mode) => {
                if !library {
                    self.enter(task, host, site, entry.occurrence)?;
                }
                let handles = self.take(args);
                match self.join_list(task, *mode, &handles)? {
                    Ok(value) => {
                        self.put(args, value);
                        Ok(Flow::Done)
                    }
                    Err(members) => {
                        self.capture(task, entry, act, PhaseRecord::Joining)?;
                        self.wait_list(task, stmt, *mode, members)?;
                        Ok(Flow::Step)
                    }
                }
            }
            NestedKind::Yield => {
                if !library {
                    self.enter(task, host, site, entry.occurrence)?;
                }
                self.capture(task, entry, act, PhaseRecord::Yielded)?;
                self.task(task)?
                    .wake(Incoming::Value(PageValue::Null), false);
                self.frame(task)?.awaiting = Some(stmt);
                self.ready.push_back(task)?;
                self.current = None;
                Ok(Flow::Step)
            }
            NestedKind::Cancel => {
                if !library {
                    self.enter(task, host, site, entry.occurrence)?;
                }
                let Value::Task(target) = self.take(args) else {
                    return raise("type_error", "`cancel` takes a task handle");
                };
                if target == task {
                    return Err(cancelled_here());
                }
                self.cancel(target)?;
                self.put(args, Value::Null);
                Ok(Flow::Done)
            }
        }
    }
}
