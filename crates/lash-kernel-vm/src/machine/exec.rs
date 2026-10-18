//! Statements and control: blocks, loops, `try`, calls and returns.

use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::{Measure, TaskId, Value};
use lash_kernel_exec::LocalRef;

use super::ops::Reg;
use super::stream::{Ran, Running, unbound};
use super::{
    Completion, Control, Eval, Frame, Halt, Incoming, Interrupt, KernelMachine, LibraryCall,
    SlotState, TaskState, TryPhase, bound, fault, raise,
};
use crate::compile::{BlockId, CodeId, Executable, Form, LibId, Plan, Source, StmtId, Var};
use crate::data::{deep_size, magnitude, nested_size, size};
use crate::interface::{Bound, Host};
use crate::observe::Event;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::paged::HeapLifecycle;

/// A function about to run in a new frame.
pub(super) struct Call<'a> {
    pub(super) code: CodeId,
    /// The register the call's arguments start at.
    pub(super) args: Reg,
    /// How many arguments there are.
    pub(super) count: usize,
    /// The cells of the variables a closure shares, in its code's order.
    pub(super) captures: &'a [ArenaHandle],
    /// The library function the code is the body of.
    pub(super) library: Option<LibId>,
    /// The statement of the caller's frame that waits in the call.
    pub(super) awaited_by: Option<StmtId>,
}

impl<'a> Call<'a> {
    pub(super) fn new(code: CodeId, args: Reg, count: usize) -> Self {
        Self {
            code,
            args,
            count,
            captures: &[],
            library: None,
            awaited_by: None,
        }
    }

    pub(super) fn sharing(mut self, captures: &'a [ArenaHandle]) -> Self {
        self.captures = captures;
        self
    }

    pub(super) fn of_library(mut self, library: LibId) -> Self {
        self.library = Some(library);
        self
    }

    pub(super) fn awaited_by(mut self, stmt: Option<StmtId>) -> Self {
        self.awaited_by = stmt;
        self
    }
}

pub(super) enum ResultOrigin {
    Returned,
    Delivered,
}

impl KernelMachine {
    /// Rebuilds the active charge mode from the resident frame cache.
    pub(super) fn refresh_charging(&mut self, task: TaskId) -> Result<(), Halt> {
        let metered = self.bounds.charge == crate::Charge::Exact;
        (self.charging, self.paced) = self
            .task(task)?
            .frames
            .last_mut()?
            .map_or((metered, !metered), |frame| (frame.charging, frame.paced));
        Ok(())
    }

    /// Immediate literals are compiled values. A literal an arena object
    /// holds is built in the arena when the operation reads it.
    pub(super) fn literal(&mut self, literal: &crate::compile::Constant) -> Eval<Value> {
        use lash_kernel_doc::Literal;
        let literal = match literal {
            crate::compile::Constant::Resident(value) => return Ok(*value),
            crate::compile::Constant::Deferred(literal) => literal,
        };
        let mut arena = self.arena.borrow_mut();
        Ok(match literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Float(value) => Value::Float(*value),
            Literal::Int(value) => arena.int(value.clone()),
            Literal::Text(value) => arena.text(value),
            Literal::Bytes(value) => arena.bytes(value.as_slice()),
            Literal::Function(name) => arena.function(name.as_str()),
        })
    }
}

impl KernelMachine {
    /// Runs the next statement of the task's top frame, or leaves the
    /// block that has none left.
    pub(super) fn advance(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Eval<Completion> {
        let frame = self
            .tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("a running task is missing"))?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("a running task has no frame"))?;
        self.charging = frame.charging;
        self.paced = frame.paced;
        let code = frame.code;
        match frame.control.last_mut() {
            Some(Control::Block { block, next }) => {
                let stmts = &exe.block(*block).stmts;
                match stmts.get(*next) {
                    Some(&id) => {
                        *next += 1;
                        let last = self.steps.verify && *next == stmts.len();
                        if self.charging {
                            self.charge(1)?;
                        } else if self.paced {
                            self.charged += 1;
                        }
                        let running = Running::new(task, exe, id);
                        // Counting stays inline and does not enable the work/context
                        // observer. Off pays only this predictable branch.
                        if !self.ran.is_empty()
                            && let Some(ordinal) = running.stmt.ordinal
                        {
                            self.ran[ordinal.get() as usize] += 1;
                        }
                        self.observe(Event::Statement {
                            task,
                            code,
                            stmt: id,
                            charging: self.charging,
                        });
                        let edge = running.edge;
                        let start = running.stmt.ops.start as usize;
                        match self.run(host, &running, start)? {
                            Ran::Done => {
                                self.take_edge(task, exe, edge)?;
                                Ok(Completion::Normal)
                            }
                            Ran::Step => {
                                if last {
                                    self.verify_no_edge(task, exe)?;
                                }
                                Ok(Completion::Normal)
                            }
                            Ran::Depart(completion) => Ok(completion),
                            Ran::Tested(_) => {
                                Err(fault("a statement ended as a loop's test").into())
                            }
                        }
                    }
                    None => {
                        if *next == 0 {
                            let block = *block;
                            self.observe_empty_block(task, block, exe);
                        }
                        self.pop_control(task, exe)?;
                        self.block_ended(task, host, exe)
                    }
                }
            }
            Some(_) => Err(fault("a loop or a try has no block running").into()),
            None => Ok(Completion::Return(Box::new(Value::Null))),
        }
    }

    /// Removes the innermost control entry and ends the variables it
    /// declared.
    fn pop_control(&mut self, task: TaskId, exe: &Executable) -> Result<(), Halt> {
        let frame = self.frame(task)?;
        if let Some(Control::Block { block, .. }) = frame.control.pop() {
            for at in &exe.block(block).declares {
                frame.set_slot(*at, SlotState::Empty);
            }
        }
        Ok(())
    }

    pub(super) fn push_block(&mut self, task: TaskId, block: BlockId) -> Result<(), Halt> {
        self.frame(task)?
            .control
            .push(Control::Block { block, next: 0 });
        Ok(())
    }

    /// A block ran its last statement: what encloses it goes on.
    fn block_ended(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Eval<Completion> {
        let frame = self.frame(task)?;
        match frame.control.last_mut() {
            None => Ok(Completion::Return(Box::new(Value::Null))),
            Some(Control::Block { .. }) => Ok(Completion::Normal),
            Some(Control::For { .. } | Control::While { .. }) => {
                self.next_iteration(task, host, exe)?;
                Ok(Completion::Normal)
            }
            Some(Control::Try { stmt, phase }) => {
                let finally = match &exe.stmt(*stmt).form {
                    Form::Try { finally, .. } => *finally,
                    _ => None,
                };
                match (std::mem::replace(phase, TryPhase::Body), finally) {
                    (TryPhase::Finally(departure), _) => {
                        frame.control.pop();
                        Ok(departure)
                    }
                    (_, Some(finally)) => {
                        *phase = TryPhase::Finally(Completion::Normal);
                        self.push_block(task, finally)?;
                        Ok(Completion::Normal)
                    }
                    (_, None) => {
                        frame.control.pop();
                        Ok(Completion::Normal)
                    }
                }
            }
        }
    }

    /// Carries a departure outwards until something takes it: a loop, a
    /// `catch`, a `finally`, the caller, or the task's end (`K-FORM-014`
    /// to `K-FORM-017`). Inlined into the step, as `settle` is.
    #[inline(always)]
    pub(super) fn complete(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        mut departure: Completion,
    ) -> Result<(), Halt> {
        loop {
            match &departure {
                Completion::Normal => return Ok(()),
                // A value on its way out is in no variable: the frames it
                // leaves take theirs with them.
                Completion::Return(value)
                | Completion::ReturnVia { value, .. }
                | Completion::Throw(value) => {
                    self.borrow_value(**value);
                }
                Completion::Break | Completion::Continue => {}
            }
            let frame = self.frame(task)?;
            // The statement the call stood in is left behind, with the
            // continuation it saved there.
            frame.awaiting = None;
            frame.nested = None;
            frame.pending.take();
            // A return or a raise that no `try` of the frame can take leaves
            // the frame at once: ending the frame ends all its variables.
            if matches!(
                departure,
                Completion::Return(_) | Completion::ReturnVia { .. } | Completion::Throw(_)
            ) && !frame
                .control
                .iter()
                .any(|control| matches!(control, Control::Try { .. }))
            {
                frame.control.clear();
            }
            let Some(control) = frame.control.last_mut() else {
                if let Completion::ReturnVia { value, stmt } = departure {
                    let Form::ReturnVia { site, policy } = &exe.stmt(stmt).form else {
                        return Err(fault("a deferred return has no policy"));
                    };
                    // Source handlers have gone. The policy is an ordinary
                    // call, and its result leaves this frame directly.
                    for at in 0..exe.code(frame.code).slots_by_at.len() {
                        frame.set_slot(at as u32, SlotState::Empty);
                    }
                    frame.awaiting = Some(stmt);
                    self.frame(task)?.statement =
                        lash_kernel_state::paged::SiteRef::new(site, self.heap.pages())?;
                    self.regs.statement(exe.stmt(stmt).regs);
                    self.put(0, *value);
                    match self.call_lib(task, exe, *policy, 0, 1, Some(stmt)) {
                        Ok(super::ops::Flow::Step) => return Ok(()),
                        Ok(super::ops::Flow::Done) => {
                            let value = self.take(0);
                            departure = match self.finish_action(
                                task,
                                host,
                                exe,
                                value,
                                ResultOrigin::Returned,
                            ) {
                                Ok((_, Ran::Depart(next))) => next,
                                Ok(_) => return Err(fault("a return policy did not return")),
                                Err(interrupt) => Completion::Throw(self.thrown_by(interrupt)?),
                            };
                        }
                        Err(interrupt) => departure = Completion::Throw(self.thrown_by(interrupt)?),
                    }
                    continue;
                }
                let mut result = match departure {
                    Completion::Return(value) => Ok(*value),
                    Completion::Throw(value) => Err(*value),
                    _ => return Err(fault("a break or a continue left its function")),
                };
                if let Some(cleanup) = self.root_cleanup(task, exe)? {
                    match self.clean_up(task, exe, cleanup, result)? {
                        Some(cleaned) => result = cleaned,
                        None => return Ok(()),
                    }
                }
                match self.leave_frame(task, host, exe, result)? {
                    Some(next) => departure = next,
                    None => return Ok(()),
                }
                continue;
            };
            match control {
                Control::Block { .. } => self.pop_control(task, exe)?,
                Control::For { .. } | Control::While { .. } => match departure {
                    Completion::Break => {
                        frame.control.pop();
                        departure = Completion::Normal;
                    }
                    Completion::Continue => {
                        departure = match self.next_iteration(task, host, exe) {
                            Ok(_) => Completion::Normal,
                            Err(interrupt) => Completion::Throw(self.thrown_by(interrupt)?),
                        };
                    }
                    _ => {
                        frame.control.pop();
                    }
                },
                Control::Try { stmt, phase } => {
                    let (catch, finally) = match &exe.stmt(*stmt).form {
                        Form::Try { catch, finally, .. } => (*catch, *finally),
                        _ => (None, None),
                    };
                    match (&*phase, catch, finally, &departure) {
                        (TryPhase::Body, Some((binding, block)), _, Completion::Throw(value)) => {
                            let value = **value;
                            *phase = TryPhase::Catch;
                            self.push_block(task, block)?;
                            self.bind(task, binding.at, binding.shared, value)?;
                            departure = Completion::Normal;
                        }
                        (TryPhase::Body | TryPhase::Catch, _, Some(finally), _) => {
                            *phase = TryPhase::Finally(departure);
                            self.push_block(task, finally)?;
                            departure = Completion::Normal;
                        }
                        // A `finally` that leaves by its own departure
                        // replaces the one it interrupted.
                        _ => {
                            frame.control.pop();
                        }
                    }
                }
            }
        }
    }

    /// The run-root cleanup to call as the entry the run started on is
    /// left: its start convention's, when the departing frame is that
    /// entry's root frame.
    fn root_cleanup(&mut self, task: TaskId, exe: &Executable) -> Result<Option<LibId>, Halt> {
        if task != TaskId::MAIN || self.task(task)?.frames.len() != 1 {
            return Ok(None);
        }
        let code = self.frame(task)?.code;
        Ok(exe.root_cleanup(code))
    }

    /// Calls the run-root cleanup `cleanup` with no source scope left, the
    /// frame holding the body's departure `result` while it runs. Answers
    /// what then ends the run: `result`, or the cleanup's raise; `None`
    /// while the cleanup's body runs, whose return `leave_frame` takes.
    fn clean_up(
        &mut self,
        task: TaskId,
        exe: &Executable,
        cleanup: LibId,
        result: Result<Value, Value>,
    ) -> Result<Option<Result<Value, Value>>, Halt> {
        let code = exe.code(self.frame(task)?.code);
        let statement = lash_kernel_state::paged::SiteRef::new(&code.site, self.heap.pages())?;
        let frame = self.frame(task)?;
        for at in 0..code.slots_by_at.len() {
            frame.set_slot(at as u32, SlotState::Empty);
        }
        frame.statement = statement;
        frame.awaiting = None;
        frame.cleaning = Some(result);
        match self.call_lib(task, exe, cleanup, 0, 0, None) {
            Ok(super::ops::Flow::Step) => Ok(None),
            Ok(super::ops::Flow::Done) => {
                self.take(0);
                Ok(self.frame(task)?.cleaning.take())
            }
            Err(interrupt) => {
                let value = self.thrown_by(interrupt)?;
                self.frame(task)?.cleaning = None;
                Ok(Some(Err(*value)))
            }
        }
    }

    /// Ends the task's top frame with a result or a raise, and hands it to
    /// the caller. Returns the departure the caller is left with.
    fn leave_frame(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        result: Result<Value, Value>,
    ) -> Result<Option<Completion>, Halt> {
        let Some(frame) = self.task(task)?.frames.pop()? else {
            return Err(fault("a task with no frame returned"));
        };
        self.left(task, host, exe, frame, result)
    }

    /// The task's top frame `frame`, taken off its stack, has ended with
    /// a result or a raise, which its caller takes. Returns the departure
    /// the caller is left with.
    pub(super) fn left(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        frame: Box<Frame>,
        result: Result<Value, Value>,
    ) -> Result<Option<Completion>, Halt> {
        self.refresh_charging(task)?;
        if self.charging
            && let Some(call) = &frame.library
        {
            self.warm_values(&exe.lib(call.lib).charge, &call.args, result.as_ref().ok())?;
            let units = self.in_formula(call.lib, true, |machine| {
                machine.call_units(exe, call.lib, &call.args, result.as_ref().ok())
            })?;
            self.charge_call(exe, call.lib, units)?;
        }
        let result = if task == TaskId::MAIN && self.task(task)?.frames.is_empty() {
            match &exe.code(frame.code).site.unit {
                lash_kernel_doc::Unit::Function(name)
                    if self
                        .program
                        .document
                        .entries
                        .get(name)
                        .is_some_and(|entry| {
                            matches!(
                                entry.convention,
                                lash_kernel_doc::StartConvention::TsArgsList { .. }
                                    | lash_kernel_doc::StartConvention::TsDirect { .. }
                            )
                        }) =>
                {
                    result.map(|value| {
                        if matches!(value, Value::Absent) {
                            Value::Null
                        } else {
                            value
                        }
                    })
                }
                _ => result,
            }
        } else {
            result
        };
        self.observe(Event::Pop { task });
        self.recycle(frame);
        if self.task(task)?.frames.is_empty() {
            self.end_task(task, result)?;
            return Ok(None);
        }
        // The run-root cleanup has returned or raised: the body's departure,
        // or the cleanup's raise, leaves the entry.
        if let Some(departed) = self.frame(task)?.cleaning.take() {
            let result = if result.is_ok() { departed } else { result };
            return self.leave_frame(task, host, exe, result);
        }
        Ok(match result {
            Ok(value) => match self
                .segment_returned(task, host, exe, value)
                .map_err(Interrupt::from)
                .and_then(|returned| match returned {
                    // A segment the caller stands in ran on (FIG-6709).
                    Ok(departure) => Ok(Err(departure)),
                    Err(value) => self
                        .finish_action(task, host, exe, value, ResultOrigin::Returned)
                        .map(Ok),
                })
                .and_then(|ran| match ran {
                    Ok(ran) => self.resumed(task, exe, ran).map(Some),
                    Err(departure) => Ok(departure),
                }) {
                Ok(Some(Completion::Normal) | None) => None,
                Ok(Some(completion)) => Some(completion),
                Err(interrupt) => Some(Completion::Throw(self.thrown_by(interrupt)?)),
            },
            Err(value) => Some(Completion::Throw(Box::new(value))),
        })
    }

    /// What a library call is charged by its definition's formula
    /// (`K-CHG-003`). A call that raised has a result of size 0.
    /// Calls within library bodies add no formula charge (`K-CHG-007`).
    pub(super) fn call_units(
        &self,
        exe: &Executable,
        lib: LibId,
        args: &[Value],
        result: Option<&Value>,
    ) -> Result<u64, Halt> {
        // Callers enter only for charged code: library bodies are covered
        // by their caller's formula, and Off has no completion formula.
        debug_assert!(self.charging);
        let function = exe.lib(lib);
        self.formula(&function.charge, args, result)
    }

    /// Charges a library call what [`Self::call_units`] gave.
    #[inline(always)]
    pub(super) fn charge_call(
        &mut self,
        exe: &Executable,
        lib: LibId,
        units: u64,
    ) -> Result<(), Halt> {
        self.charge(units)
            .map_err(|halt| halt.in_function(&exe.lib(lib).definition.name))
    }

    /// Drop all guest holdings before retaining the activation's storage.
    pub(super) fn recycle(&mut self, mut frame: Box<Frame>) {
        frame.control.clear();
        frame.slots.clear();
        frame.pending.clear();
        frame.awaiting = None;
        frame.nested = None;
        frame.library = None;
        frame.cleaning = None;
        frame.held.clear();
        frame.owned = false;
        self.storage.frames.push(frame);
    }

    pub(super) fn formula(
        &self,
        plan: &Plan,
        args: &[Value],
        result: Option<&Value>,
    ) -> Result<u64, Halt> {
        self.formula_source(
            plan,
            |index, measure| self.measure(args.get(index), measure),
            result,
        )
    }

    fn formula_source(
        &self,
        plan: &Plan,
        mut argument: impl FnMut(usize, Measure) -> Result<u64, HeapFault>,
        result: Option<&Value>,
    ) -> Result<u64, Halt> {
        let mut fault = None;
        let units = plan.evaluate(|source, measure| {
            // Formula arithmetic is pure; after the first failed measurement
            // no subsequent measurement may read the heap.
            if fault.is_some() {
                return 0;
            }
            let amount = match source {
                Source::Arg(index) => argument(index, measure),
                Source::Result => self.measure(result, measure),
                Source::Nothing => Ok(0),
            };
            match amount {
                Ok(units) => units,
                Err(error) => {
                    fault = Some(error);
                    0
                }
            }
        });
        fault.map_or(Ok(units), |error| Err(Halt::from(error)))
    }

    fn measure(&self, value: Option<&Value>, measure: Measure) -> Result<u64, HeapFault> {
        #[cfg(test)]
        if let Some(value) = value {
            self.storage
                .formula_sources
                .borrow_mut()
                .push(std::ptr::from_ref(value).addr());
        }
        let arena = self.arena.borrow();
        match (value, measure) {
            (None, _) => Ok(0),
            (Some(value), Measure::Size) => size(&arena, value),
            (Some(value), Measure::DeepSize) => deep_size(&arena, value),
            (Some(value), Measure::NestedSize) => nested_size(&arena, value),
            (Some(value), Measure::Magnitude) => Ok(magnitude(&arena, value)),
        }
        .map_err(|error| HeapFault::Malformed(error.to_string()))
    }

    /// Decodes what `plan`'s measurements of the call's arguments from
    /// register `args`, and of `result`, read: a formula reads the arena
    /// as it is.
    pub(super) fn warm_formula(
        &mut self,
        plan: &Plan,
        args: super::ops::Reg,
        result: Option<&Value>,
    ) -> Result<(), Halt> {
        if !self.arena.borrow().any_cold() {
            return Ok(());
        }
        let given: Vec<Value> = self.regs[args..].to_vec();
        self.warm_values(plan, &given, result)
    }

    /// [`Self::warm_formula`] over arguments `args`.
    pub(super) fn warm_values(
        &mut self,
        plan: &Plan,
        args: &[Value],
        result: Option<&Value>,
    ) -> Result<(), Halt> {
        if !self.arena.borrow().any_cold() {
            return Ok(());
        }
        for (source, measure) in plan.measurements() {
            let value = match source {
                Source::Arg(index) => args.get(*index).copied(),
                Source::Result => result.copied(),
                Source::Nothing => None,
            };
            let Some(value) = value else {
                continue;
            };
            match measure {
                Measure::Size => {
                    if let Some(handle) = value.handle() {
                        self.warm_head(handle)?;
                    }
                }
                Measure::DeepSize | Measure::NestedSize => self.warm_measured(&value)?,
                Measure::Magnitude => {}
            }
        }
        Ok(())
    }

    /// Reserves the next active call's depth unit (K-BND-002). Native
    /// operation answers are synchronous, so their unit ends with dispatch;
    /// a decline transfers it to the body's frame until return or raise.
    /// Returns whether the admitted task has no frames.
    pub(super) fn check_call_depth(&mut self, task: TaskId) -> Eval<bool> {
        self.call_depth_headroom(task).map(|(root, _)| root)
    }

    pub(super) fn call_depth_headroom(&mut self, task: TaskId) -> Eval<(bool, u32)> {
        // `main`'s own frame is not a call.
        let frames = self.task(task)?.frames.len();
        let depth =
            frames + self.plans as usize + usize::from(task != TaskId::MAIN || !self.session_cell);
        if depth as u64 > u64::from(self.bounds.call_depth) {
            return Err(bound(Bound::CallDepth, u64::from(self.bounds.call_depth)).into());
        }
        Ok((frames == 0, self.bounds.call_depth - depth as u32))
    }

    /// Pushes a frame that runs `code` with the call's arguments bound to
    /// its parameters, in order; a parameter with no argument is absent
    /// (`K-FN-004`). The arguments are moved out of their registers.
    pub(super) fn push_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: Call<'_>,
    ) -> Eval<()> {
        self.push_operation_frame(task, exe, call, None)
    }

    /// A frame of code `code_id` standing at its body's entry, its storage
    /// reused when an earlier frame left it.
    pub(super) fn fresh_frame(
        &mut self,
        exe: &Executable,
        code_id: CodeId,
    ) -> Result<Box<Frame>, Halt> {
        let code = exe.code(code_id);
        let at = code_id.0 as usize;
        if self.storage.entry_sites.len() <= at {
            self.storage.entry_sites.resize_with(at + 1, || None);
        }
        if self.storage.entry_sites[at].is_none() {
            let entry =
                lash_kernel_state::paged::SiteRef::new(&code.site.child(0), self.heap.pages())?;
            #[cfg(test)]
            self.storage.activation_sites.push(code_id);
            self.storage.entry_sites[at] = Some(entry);
        }
        let entry = self.storage.entry_sites[at]
            .as_ref()
            .ok_or_else(|| fault("a prepared body has no entry site"))?;
        Ok(match self.storage.frames.pop() {
            Some(mut frame) => {
                frame.code = code_id;
                frame.charging = code.charging;
                frame.paced = code.paced;
                // A spent frame of the same body left standing at its entry
                // keeps that copy; only another site is replaced.
                if !frame.statement.same_version(entry) {
                    frame.statement = entry.clone();
                }
                frame
            }
            None => {
                #[cfg(test)]
                self.storage.activation_records.push(code_id);
                Box::new(Frame {
                    code: code_id,
                    charging: code.charging,
                    paced: code.paced,
                    slots: super::frames::Slots::default(),
                    statement: entry.clone(),
                    pending: super::frames::Continuation::default(),
                    control: Vec::new(),
                    awaiting: None,
                    nested: None,
                    library: None,
                    cleaning: None,
                    held: Vec::new(),
                    owned: false,
                })
            }
        })
    }

    pub(super) fn push_operation_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: Call<'_>,
        admission: Option<super::operation_entry::OperationEntry>,
    ) -> Eval<()> {
        let Call {
            library,
            code: code_id,
            args: base,
            count: given,
            captures,
            awaited_by,
        } = call;
        let code = exe.code(code_id);
        if given > code.params.len() {
            return raise(
                "arity",
                format!(
                    "the function takes {} argument(s); {} given",
                    code.params.len(),
                    given
                ),
            );
        }
        // An operation checked this unit at admission. Its fallback frame
        // represents that same active call, not a second one.
        if !library.is_some_and(|lib| exe.lib(lib).operation()) {
            self.check_call_depth(task)?;
        }
        // SiteRef contains page-access-local trees. Prepare each body's
        // immutable entry once here, after the same admission checks as a
        // cold call; sharing it across machines would share their page port.
        let mut frame = self.fresh_frame(exe, code_id)?;
        frame.slots.fresh(code.slots_by_at.len());
        frame.control.push(Control::Block {
            block: code.body,
            next: 0,
        });
        if let Some(lib) = library {
            // Nested calls are covered by their caller's schedule. A new
            // task has no enclosing library frame, so its root bills its own
            // formula even when an uncharged library body spawned it.
            let keep_arguments = self.charging || self.task(task)?.frames.is_empty();
            let args = if keep_arguments {
                self.regs[base..base + given].into()
            } else {
                Box::default()
            };
            frame.library = Some(LibraryCall { lib, args });
        }
        for (capture, cell) in code.captures.iter().zip(captures) {
            frame.set_slot(capture.inner_at, SlotState::Cell(*cell));
        }
        if let Some(stmt) = awaited_by {
            self.frame(task)?.awaiting = Some(stmt);
        }
        self.task(task)?.frames.push(frame);
        self.grown();
        self.observe(Event::Push {
            task,
            code: code_id,
        });
        for (index, param) in code.params.iter().enumerate() {
            #[cfg(test)]
            {
                self.storage.value_moves += usize::from(index < given);
            }
            let value = match index < given {
                true => self.regs.take(base + index, Value::Absent),
                false => Value::Absent,
            };
            if let Some(admission) = &admission {
                let state = match admission.shared.get(index).copied().flatten() {
                    Some(cell) => SlotState::Cell(cell),
                    None => SlotState::Value(value),
                };
                self.frame(task)?.set_slot(param.at, state);
            } else {
                self.bind(task, param.at, param.shared, value)?;
            }
        }
        if !code.free.is_empty() {
            self.bind_captures(task, code)?;
        }
        Ok(())
    }

    /// Binds each free name of a saved function's code to a fresh instance
    /// of the capture its session holds for it: every call its own family,
    /// none sharing an identity with another call's (`K-SES-004`). A name
    /// the session holds no capture of stays unbound.
    fn bind_captures(&mut self, task: TaskId, code: &crate::compile::Code) -> Eval<()> {
        let lash_kernel_doc::Unit::Function(function) = &code.site.unit else {
            return Ok(());
        };
        let saved =
            super::Tree::<lash_kernel_state::paged::FunctionRecord>::of_keyed(&self.functions)
                .get(
                    &lash_kernel_state::kernel_internals::metadata::metadata_key(function),
                    self.heap.pages(),
                )?;
        let Some(saved) = saved else {
            return Ok(());
        };
        for (name, local) in &code.free {
            if let Some(frozen) = saved.captures.get(name) {
                let value = self.heap.instantiate(frozen).map_err(super::write_halt)?;
                let value = self.admit_page(&value)?;
                self.bind(task, local.at, local.shared, value)?;
            }
        }
        Ok(())
    }

    /// Binds a new variable at frame position `at` of the top frame, in an
    /// arena cell when a closure shares it (`K-FORM-004`).
    pub(super) fn bind(
        &mut self,
        task: TaskId,
        at: u32,
        shared: bool,
        value: Value,
    ) -> Result<(), Halt> {
        let state = if shared {
            SlotState::Cell(self.new_cell(value))
        } else {
            SlotState::Value(value)
        };
        #[cfg(test)]
        {
            self.storage.value_moves += 1;
        }
        self.frame(task)?.set_slot(at, state);
        Ok(())
    }

    /// Reads an action's variable.
    pub(super) fn read_var(&mut self, task: TaskId, exe: &Executable, var: &Var) -> Eval<Value> {
        match var {
            Var::Local(local) => self.read_local(
                task,
                exe,
                LocalRef {
                    slot: local.slot,
                    at: local.at,
                },
            ),
            Var::Session(name) => match self.session_get(name)? {
                Some(value) => Ok(value),
                None => unbound(name),
            },
            Var::Unbound(name) => unbound(name),
        }
    }

    /// Copies a value out of the run, charging its deep size
    /// (`K-EFF-002`, `K-CHG-002`).
    pub(super) fn copy_out(&mut self, value: &Value) -> Eval<lash_kernel_doc::Datum> {
        if self.charging {
            let size = self.deep_size(value)?;
            self.charge(size)?;
        }
        Ok(self.copy_datum(value)?)
    }

    /// A value's deep charge size (`K-CHG-005`), its graph decoded first.
    pub(super) fn deep_size(&mut self, value: &Value) -> Result<u64, Halt> {
        self.warm_measured(value)?;
        deep_size(&self.arena.borrow(), value).map_err(|error| fault(&error.to_string()))
    }

    /// Copies an effect argument out of the run under the crossing
    /// policy: a function in it crosses as a function value
    /// ([`crate::crossing`]). The traversal is charged as a copy-out is,
    /// and each byte of a function value's canonical form besides.
    pub(super) fn copy_out_tool_arg(&mut self, value: &Value) -> Eval<lash_kernel_doc::Datum> {
        if self.charging {
            let size = self.deep_size(value)?;
            self.charge(size)?;
        }
        // Only a function crossing reads the session: the bindings the
        // cell wrote go to the heap first.
        let function = self.reaches_function(value)?;
        self.identify_session()?;
        if function {
            self.project_session()?;
        }
        // The crossing gathers functions from the page form: the heap
        // holds what the arena does first, each new object the argument
        // reaches with its id.
        self.identify_values(std::slice::from_ref(value))?;
        self.flush_arena()?;
        let value = &self.page_form(value)?;
        let document = Arc::clone(&self.program.document);
        let library = self.program.library.clone();
        let mut heap = crate::crossing::CrossingHeap {
            heap: &mut self.heap,
            functions: &self.functions,
            budget: lash_kernel_doc::snapshot::Budget::new(
                lash_kernel_doc::FunctionValueLimits::CEILING,
            ),
        };
        let mut run = crate::crossing::CrossingRun {
            document: &document,
            catalog: library.registry().as_ref(),
            data: self.limits,
            tag: None,
        };
        let datum = crate::crossing::copy_out_tool_arg(&mut heap, &mut run, value)?;
        if self.charging {
            self.charge(crossed_bytes(&datum))?;
        }
        Ok(datum)
    }

    /// Whether `value` reaches a closure or a function reference: what an
    /// effect argument crosses as a function value.
    fn reaches_function(&mut self, value: &Value) -> Result<bool, Halt> {
        fn function(value: &Value) -> bool {
            matches!(
                value,
                Value::Closure(_) | Value::Function(_) | Value::LongFunction(_)
            )
        }
        if function(value) {
            return Ok(true);
        }
        self.warm_reachable(value)?;
        let arena = self.arena.borrow();
        let mut pending: Vec<lash_kernel_doc::arena::ArenaHandle> =
            value.handle().into_iter().collect();
        let mut seen = std::collections::HashSet::new();
        let mut found = false;
        while let Some(handle) = pending.pop() {
            if found {
                break;
            }
            if !seen.insert((handle.index(), handle.generation())) {
                continue;
            }
            let body = arena
                .body(handle)
                .map_err(|error| fault(&error.to_string()))?;
            lash_kernel_state::arena::each_member(body, &mut |member| {
                found |= function(member);
                pending.extend(member.handle());
            });
        }
        Ok(found)
    }

    /// What a task's pending statement resumes with (`K-TASK-009`,
    /// `K-EFF-005`, `K-EFF-009`).
    pub(super) fn resume(
        &mut self,
        task: TaskId,
        exe: &Executable,
        incoming: Incoming,
    ) -> Eval<Value> {
        match incoming {
            Incoming::Value(value) => Ok(self.admit_page(&value)?),
            Incoming::Raise(value) => Err(Interrupt::raised(self.admit_page(&value)?)),
            Incoming::Joined(joined) => self.join_result(joined),
            Incoming::Outcome(wait) => self.resume_outcome(task, exe, wait),
        }
    }

    fn resume_outcome(
        &mut self,
        _task: TaskId,
        _exe: &Executable,
        wait: crate::interface::WaitId,
    ) -> Eval<Value> {
        let key = lash_kernel_state::kernel_internals::directory::ordinal_key(
            lash_kernel_doc::kernel_internals::wait_number(wait),
        );
        let pending = self
            .waits
            .get(&key, self.heap.pages())?
            .ok_or_else(|| fault("a resuming outcome has no wait"))?;
        let lash_kernel_state::paged::WaitPhase::Committed(outcome) = pending.phase else {
            return Err(fault("a resuming outcome is not committed").into());
        };
        use lash_kernel_state::paged::CommittedOutcomeRecord;
        let (value, failed) = match outcome {
            CommittedOutcomeRecord::Elapsed => (lash_kernel_doc::heap::PageValue::Null, false),
            CommittedOutcomeRecord::Rejected(frozen) => (
                self.heap.instantiate(&frozen).map_err(Interrupt::from)?,
                true,
            ),
            CommittedOutcomeRecord::Completed(frozen) | CommittedOutcomeRecord::Failed(frozen) => {
                let charge = if self.charging {
                    self.heap.frozen_charge(&frozen)?
                } else {
                    0
                };
                let value = self.heap.instantiate(&frozen).map_err(Interrupt::from)?;
                if self.charging {
                    self.charge(charge)?;
                }
                (value, matches!(outcome, CommittedOutcomeRecord::Failed(_)))
            }
        };
        // run cannot seal inside a statement; installation and consumption
        // therefore publish together at the statement's safe point.
        self.waits.remove(&key, self.heap.pages())?;
        let value = self.admit_page(&value)?;
        if failed {
            Err(Interrupt::raised(value))
        } else {
            Ok(value)
        }
    }

    /// The result of a task that has ended, as a `join` on its handle
    /// gives it: its value, or a raise of its error, which is then
    /// observed (`K-TASK-016`).
    pub(super) fn join_result(&mut self, joined: TaskId) -> Eval<Value> {
        let joined = self.task(joined)?;
        match &joined.state {
            TaskState::Ended(super::EndedTask::Returned(value)) => {
                let value = value.clone();
                Ok(self.admit_page(&value)?)
            }
            TaskState::Ended(super::EndedTask::Raised(value)) => {
                let value = value.clone();
                joined.observed = true;
                Err(Interrupt::raised(self.admit_page(&value)?))
            }
            _ => Err(fault("a join resumed before its task ended").into()),
        }
    }

    /// Decodes an inbound datum by a type and builds it as a fresh graph,
    /// charging its deep size (`K-EFF-003`, `K-EFF-007`, `K-CHG-002`).
    pub(super) fn decode(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_kernel_doc::Type,
    ) -> Eval<Value> {
        self.decode_as(exe, datum, ty, "the result")
    }

    /// [`Self::decode`] of the result `effect` answered a `perform` with:
    /// a raise that it does not fit names the effect.
    pub(super) fn decode_result(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_kernel_doc::Type,
        effect: &lash_kernel_doc::EffectName,
    ) -> Eval<Value> {
        self.decode_as(exe, datum, ty, &format!("the result of `{effect}`"))
    }

    fn decode_as(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_kernel_doc::Type,
        what: &str,
    ) -> Eval<Value> {
        let decoder = lash_kernel_doc::Decoder {
            max_depth: self.limits.value_depth(),
            policy: self.program.document.manifest.numbers,
            declared: &|name| exe.declared.contains_key(name),
        };
        let decoded = match decoder.decode(datum, ty) {
            Ok(decoded) => decoded,
            Err(mismatch) => {
                return raise(
                    "effect_result",
                    format!("{what} does not fit the stated type: {mismatch}"),
                );
            }
        };
        let frozen = self.heap.freeze_datum(&decoded).map_err(Interrupt::from)?;
        let charge = if self.charging {
            self.heap.frozen_charge(&frozen)?
        } else {
            0
        };
        let value = self.heap.instantiate(&frozen).map_err(Interrupt::from)?;
        let value = self.admit_page(&value)?;
        if self.charging {
            self.charge(charge)?;
        }
        Ok(value)
    }

    /// Completes the statement the top frame waits in with `value`, what
    /// its action yielded, and runs the statement on from the action
    /// (`K-EVAL-003`). A statement's own action leaves the value in
    /// register 0 and resumes after its action op, where the statement
    /// binds, writes or drops it. A nested action's resume entry loads the
    /// saved roles into their registers under this layout and the value
    /// into the action's, and the statement goes on from the op after the
    /// action's, charging nothing again (`K-EVAL-008`). Gives the statement
    /// and how its range ended.
    pub(super) fn finish_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
        origin: ResultOrigin,
    ) -> Eval<(StmtId, Ran)> {
        let frame = self.frame(task)?;
        let Some(id) = frame.awaiting.take() else {
            return Err(fault("a value arrived at no statement").into());
        };
        let nested = frame.nested.take();
        let pending = frame.pending.take();
        if nested.is_none()
            && pending.is_none()
            && matches!(exe.stmt(id).form, Form::ReturnVia { .. })
        {
            if task == TaskId::MAIN && self.task(task)?.frames.len() == 1 {
                self.copy_out(&value)?;
            }
            return Ok((id, Ran::Depart(Completion::Return(Box::new(value)))));
        }
        // A `for`'s protocol hook answered from its frame: the header
        // goes on (`K-ITER-005`).
        if nested.is_none() && pending.is_none() && self.in_header(task)? {
            let ran = match self.header_answered(task, host, exe, value)? {
                super::ops::Flow::Done => Ran::Done,
                super::ops::Flow::Step => Ran::Step,
            };
            return Ok((id, ran));
        }
        let running = Running::new(task, exe, id);
        let (Some(act), Some(pending)) = (nested, pending) else {
            if nested.is_some() {
                return Err(fault("a resuming nested action has no saved continuation").into());
            }
            // The value is in no variable until the statement binds it.
            self.put(0, value);
            if matches!(origin, ResultOrigin::Delivered) {
                self.pin(0)?;
            } else {
                self.borrow_value(self.regs[0]);
            }
            return match self.run(host, &running, running.stmt.resume())? {
                Ran::Done => Ok((id, Ran::Done)),
                _ => Err(fault("an action's statement did not complete").into()),
            };
        };
        let entry = running
            .stream
            .nested
            .get(act.ix())
            .ok_or_else(|| fault("a pending action has no resume entry"))?;
        let mut saved = pending.roles;
        for (index, role) in entry.roles.iter().enumerate() {
            // Saved in the entry's order, unless a restore admitted them
            // in another layout's.
            let at = match saved.get(index) {
                Some(stood) if stood.site == role.suffix => index,
                _ => saved
                    .iter()
                    .position(|stood| stood.site == role.suffix)
                    .ok_or_else(|| fault("a saved continuation role is missing"))?,
            };
            // Back in the register it was evaluated to, as if the
            // statement had never stopped.
            self.put(role.reg.0 as usize, saved[at].value);
        }
        if saved.capacity() > 0 {
            saved.clear();
            self.storage.roles.push(saved);
        }
        // The value is in no variable until the statement uses it.
        self.put(entry.dst.0 as usize, value);
        if matches!(origin, ResultOrigin::Delivered) {
            self.pin(entry.dst.0 as usize)?;
        } else {
            self.borrow_value(self.regs[entry.dst.0 as usize]);
        }
        let pc = running.stmt.ops.start as usize + entry.resume as usize;
        let ran = self.run(host, &running, pc)?;
        Ok((id, ran))
    }

    /// The statement a resume ran on has ended its range as `ran` says: in
    /// its block, its step edge is taken; a `while` condition's value goes
    /// to its loop; a departure is the frame's.
    pub(super) fn resumed(
        &mut self,
        task: TaskId,
        exe: &Executable,
        (stmt, ran): (StmtId, Ran),
    ) -> Eval<Completion> {
        match ran {
            Ran::Done => {
                self.take_edge_of(task, exe, stmt)?;
                Ok(Completion::Normal)
            }
            Ran::Step => {
                if self.steps.verify {
                    self.verify_no_edge(task, exe)?;
                }
                Ok(Completion::Normal)
            }
            Ran::Depart(completion) => Ok(completion),
            Ran::Tested(value) => {
                self.tested(task, value)?;
                Ok(Completion::Normal)
            }
        }
    }
}

/// The canonical bytes of the function values in `datum`: what a crossing
/// writes beyond a data copy.
fn crossed_bytes(datum: &lash_kernel_doc::Datum) -> u64 {
    use lash_kernel_doc::Datum;
    match datum {
        Datum::FunctionValue(value) => value
            .canonical_tagged_bytes()
            .map_or(0, |bytes| bytes.len() as u64),
        Datum::Tuple(items) | Datum::List(items) | Datum::Set(items) => {
            items.iter().map(crossed_bytes).sum()
        }
        Datum::Map(entries) => entries
            .iter()
            .map(|(key, value)| crossed_bytes(key) + crossed_bytes(value))
            .sum(),
        Datum::Record(fields) => fields.iter().map(|(_, value)| crossed_bytes(value)).sum(),
        Datum::Error(error) => crossed_bytes(&error.data),
        _ => 0,
    }
}
