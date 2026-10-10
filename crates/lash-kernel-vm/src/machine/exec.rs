//! Statements and control: blocks, loops, `try`, calls and returns.

use std::sync::Arc;

use lash_kernel_doc::{Measure, ObjectId, TaskId, Value};

use super::ops::{Flow, Reg};
use super::{
    Completion, Control, Eval, Frame, FrameStorage, Halt, Incoming, Interrupt, KernelMachine,
    LibraryCall, SlotState, TaskState, TryPhase, bound, fault, raise,
};
use crate::compile::{
    BlockId, CodeId, Executable, LibId, Local, Member, Place, Plan, Rhs, Source, Stmt, StmtId,
    Target, Var,
};
use crate::data::{Decoder, copy_out, deep_size, magnitude, nested_size, size};
use crate::heap::{Obj, value_bytes};
use crate::interface::{Bound, Host, Outcome};

/// How a statement's run ends.
enum Ran {
    /// In its own block: its step edge is next.
    Done,
    /// Having entered a block, a loop, a `try` or a frame, or waiting: the
    /// step ends where the statement left the task.
    Step,
    /// Departing: a `break`, a `continue` or a `return`.
    Depart(Completion),
}

/// A function about to run in a new frame.
pub(super) struct Call<'a> {
    pub(super) code: CodeId,
    /// The register the call's arguments start at; they run to the top.
    pub(super) args: Reg,
    /// The cells of the variables a closure shares, in its code's order.
    pub(super) captures: &'a [ObjectId],
    /// The library function the code is the body of.
    pub(super) library: Option<LibId>,
    /// The call sits inside an expression and runs to its end there.
    pub(super) inline: bool,
    /// The statement of the caller's frame that waits in the call.
    pub(super) awaited_by: Option<StmtId>,
}

impl<'a> Call<'a> {
    pub(super) fn new(code: CodeId, args: Reg) -> Self {
        Self {
            code,
            args,
            captures: &[],
            library: None,
            inline: false,
            awaited_by: None,
        }
    }

    pub(super) fn sharing(mut self, captures: &'a [ObjectId]) -> Self {
        self.captures = captures;
        self
    }

    pub(super) fn of_library(mut self, library: LibId) -> Self {
        self.library = Some(library);
        self
    }

    pub(super) fn inline(mut self) -> Self {
        self.inline = true;
        self
    }

    pub(super) fn awaited_by(mut self, stmt: Option<StmtId>) -> Self {
        self.awaited_by = stmt;
        self
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
            .get_mut(task.0 as usize)
            .and_then(|task| task.frames.last_mut())
            .ok_or_else(|| fault("a running task has no frame"))?;
        self.charging = exe.code(frame.code).charged;
        match frame.control.last_mut() {
            Some(Control::Block { block, next }) => {
                let stmts = &exe.block(*block).stmts;
                match stmts.get(*next) {
                    Some(&id) => {
                        *next += 1;
                        let last = self.steps.verify && *next == stmts.len();
                        self.charge(1)?;
                        let (stmt, edge) = exe.stmt_edge(id);
                        match self.exec(task, host, exe, id, stmt)? {
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
                        }
                    }
                    None => {
                        self.pop_control(task, exe)?;
                        self.block_ended(task, host, exe)
                    }
                }
            }
            Some(_) => Err(fault("a loop or a try has no block running").into()),
            None => Ok(Completion::Return(Value::Null)),
        }
    }

    /// Removes the innermost control entry and ends the variables it
    /// declared.
    fn pop_control(&mut self, task: TaskId, exe: &Executable) -> Result<(), Halt> {
        let frame = self.frame(task)?;
        if let Some(Control::Block { block, .. }) = frame.control.pop() {
            for at in &exe.block(block).declares {
                frame.slots[*at as usize] = SlotState::Empty;
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
            None => Ok(Completion::Return(Value::Null)),
            Some(Control::Block { .. }) => Ok(Completion::Normal),
            Some(Control::For { .. } | Control::While { .. }) => {
                self.next_iteration(task, host, exe)?;
                Ok(Completion::Normal)
            }
            Some(Control::Try { stmt, phase }) => {
                let finally = match exe.stmt(*stmt) {
                    Stmt::Try { finally, .. } => *finally,
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
    /// to `K-FORM-017`).
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
                Completion::Return(value) | Completion::Throw(value) => {
                    self.pins.hold(value, value_bytes(value));
                }
                Completion::Break | Completion::Continue => {}
            }
            let frame = self.frame(task)?;
            // The statement the call stood in is left behind.
            frame.awaiting = None;
            // A return or a raise that no `try` of the frame can take leaves
            // the frame at once: ending the frame ends all its variables.
            if matches!(departure, Completion::Return(_) | Completion::Throw(_))
                && !frame
                    .control
                    .iter()
                    .any(|control| matches!(control, Control::Try { .. }))
            {
                frame.control.clear();
            }
            let Some(control) = frame.control.last_mut() else {
                let result = match departure {
                    Completion::Return(value) => Ok(value),
                    Completion::Throw(value) => Err(value),
                    _ => return Err(fault("a break or a continue left its function")),
                };
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
                            Err(Interrupt::Raise(value)) => Completion::Throw(value),
                            Err(Interrupt::Halt(halt)) => return Err(halt),
                        };
                    }
                    _ => {
                        frame.control.pop();
                    }
                },
                Control::Try { stmt, phase } => {
                    let (catch, finally) = match exe.stmt(*stmt) {
                        Stmt::Try { catch, finally, .. } => (*catch, *finally),
                        _ => (None, None),
                    };
                    match (&*phase, catch, finally, &departure) {
                        (TryPhase::Body, Some((binding, block)), _, Completion::Throw(value)) => {
                            let value = value.clone();
                            *phase = TryPhase::Catch;
                            self.push_block(task, block)?;
                            self.bind(task, binding, value)?;
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

    /// Ends the task's top frame with a result or a raise, and hands it to
    /// the caller. Returns the departure the caller is left with.
    fn leave_frame(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        result: Result<Value, Value>,
    ) -> Result<Option<Completion>, Halt> {
        let Some(frame) = self.task(task)?.frames.pop() else {
            return Err(fault("a task with no frame returned"));
        };
        self.refresh_charging(task, exe);
        if let Some(call) = &frame.library {
            let units = self.call_units(exe, call.lib, &call.args, result.as_ref().ok());
            self.charge_call(exe, call.lib, units)?;
        }
        let inline = frame.inline;
        self.recycle(frame);
        if inline {
            self.inline_result = Some(result);
            return Ok(None);
        }
        if self.task(task)?.frames.is_empty() {
            self.end_task(task, result)?;
            return Ok(None);
        }
        Ok(match result {
            Ok(value) => {
                // The value is in no variable until the statement binds it.
                let r = self.regs.len();
                self.regs.push(value);
                self.pin(r)?;
                match self.finish_action(task, host, exe, r) {
                    Ok(stmt) => {
                        self.take_edge_of(task, exe, stmt)?;
                        None
                    }
                    Err(Interrupt::Raise(value)) => Some(Completion::Throw(value)),
                    Err(Interrupt::Halt(halt)) => return Err(halt),
                }
            }
            Err(value) => Some(Completion::Throw(value)),
        })
    }

    /// What a library call is charged by its definition's formula
    /// (`K-CHG-003`). A call that raised has a result of size 0. A function
    /// with only a kernel-code body is charged nothing here: its body was
    /// charged as it ran (`K-CHG-007`).
    pub(super) fn call_units(
        &self,
        exe: &Executable,
        lib: LibId,
        args: &[Value],
        result: Option<&Value>,
    ) -> u64 {
        // Inside the body of a function with a native implementation
        // nothing is charged (`K-CHG-007`), so the formula, whose deep sizes
        // walk whole graphs, is not evaluated.
        if !self.charging {
            return 0;
        }
        let function = exe.lib(lib);
        if !function.native {
            return 0;
        }
        self.formula(&function.charge, args, result)
    }

    /// Charges a library call what [`Self::call_units`] gave.
    pub(super) fn charge_call(
        &mut self,
        exe: &Executable,
        lib: LibId,
        units: u64,
    ) -> Result<(), Halt> {
        self.charge(units)
            .map_err(|halt| halt.in_function(&exe.lib(lib).definition.name))
    }

    /// Keeps an ended frame's vectors for the next frame.
    fn recycle(&mut self, frame: Frame) {
        let Frame {
            mut slots,
            mut control,
            library,
            ..
        } = frame;
        slots.clear();
        control.clear();
        let mut args = library.map(|call| call.args).unwrap_or_default();
        args.clear();
        self.storage.frames.push(FrameStorage {
            slots,
            control,
            args,
        });
    }

    pub(super) fn formula(&self, plan: &Plan, args: &[Value], result: Option<&Value>) -> u64 {
        plan.evaluate(|source, measure| {
            let value = match source {
                Source::Arg(index) => args.get(index),
                Source::Result => result,
                Source::Nothing => None,
            };
            match (value, measure) {
                (None, _) => 0,
                (Some(value), Measure::Size) => size(&self.heap, value),
                (Some(value), Measure::DeepSize) => deep_size(&self.heap, value),
                (Some(value), Measure::NestedSize) => nested_size(&self.heap, value),
                (Some(value), Measure::Magnitude) => magnitude(value),
            }
        })
    }

    /// Pushes a frame that runs `code` with the call's arguments bound to
    /// its parameters, in order; a parameter with no argument is absent
    /// (`K-FN-004`). The arguments are moved out of their registers, and
    /// the caller takes what is left there off.
    pub(super) fn push_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: Call<'_>,
    ) -> Eval<()> {
        let Call {
            code: code_id,
            args: base,
            captures,
            library,
            inline,
            awaited_by,
        } = call;
        let code = exe.code(code_id);
        let given = self.regs.len() - base;
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
        let frames = &self.task(task)?.frames;
        // `main`'s own frame is not a call (`K-BND-002`).
        let depth = frames.len() + usize::from(task != TaskId::MAIN || !self.session_cell);
        if depth as u64 > u64::from(self.bounds.call_depth) {
            return Err(bound(Bound::CallDepth, u64::from(self.bounds.call_depth)).into());
        }
        self.reserve(super::FRAME_BYTES.saturating_add(code.slots.len() as u64 * 8))?;
        let FrameStorage {
            mut slots,
            mut control,
            mut args,
        } = self.storage.frames.pop().unwrap_or_default();
        slots.resize(code.slots.len(), SlotState::Empty);
        control.push(Control::Block {
            block: code.body,
            next: 0,
        });
        let mut frame = Frame {
            code: code_id,
            slots,
            control,
            awaiting: None,
            library: library.map(|lib| {
                // Only a native implementation's formula, charged when the
                // call ends, reads them (`K-CHG-007`).
                if exe.lib(lib).native {
                    args.extend_from_slice(&self.regs[base..]);
                }
                LibraryCall { lib, args }
            }),
            inline,
        };
        for (capture, cell) in code.captures.iter().zip(captures) {
            frame.slots[capture.inner_at as usize] = SlotState::Cell(*cell);
        }
        if let Some(stmt) = awaited_by {
            self.frame(task)?.awaiting = Some(stmt);
        }
        self.task(task)?.frames.push(frame);
        for (index, param) in code.params.iter().enumerate() {
            let value = self
                .regs
                .get_mut(base + index)
                .map_or(Value::Absent, |arg| std::mem::replace(arg, Value::Absent));
            self.bind(task, *param, value)?;
        }
        Ok(())
    }

    /// Binds a new variable in the top frame (`K-FORM-004`).
    pub(super) fn bind(&mut self, task: TaskId, local: Local, value: Value) -> Result<(), Halt> {
        let state = if local.shared {
            SlotState::Cell(self.alloc(Obj::Variable(value))?)
        } else {
            self.reserve(value_bytes(&value))?;
            SlotState::Value(value)
        };
        self.frame(task)?.slots[local.at as usize] = state;
        Ok(())
    }

    pub(super) fn read_var(&mut self, task: TaskId, exe: &Executable, var: &Var) -> Eval<Value> {
        if let Var::Local(local) = var
            && let Some(SlotState::Value(value)) = self
                .tasks
                .get(task.0 as usize)
                .and_then(|task| task.frames.last())
                .map(|frame| &frame.slots[local.at as usize])
        {
            return Ok(value.clone());
        }
        self.read_other_var(task, exe, var)
    }

    /// The rest of [`Self::read_var`]: a variable a closure shares, a
    /// session binding, and one that is not bound. Apart, so that reading
    /// a local stays small.
    #[inline(never)]
    fn read_other_var(&mut self, task: TaskId, exe: &Executable, var: &Var) -> Eval<Value> {
        let unbound = |name: &lash_kernel_doc::Name| {
            Err(Interrupt::Raise(Value::Error(Arc::new(
                lash_kernel_doc::ErrorValue {
                    kind: "unbound_variable".to_owned(),
                    message: format!("no variable `{name}` is bound here"),
                    data: Value::text(name.as_str()),
                },
            ))))
        };
        match var {
            Var::Local(local) => {
                let frame = self.frame(task)?;
                match &frame.slots[local.at as usize] {
                    SlotState::Value(value) => Ok(value.clone()),
                    SlotState::Cell(cell) => {
                        let cell = *cell;
                        match self.heap.variable(cell) {
                            Some(value) => Ok(value.clone()),
                            None => Err(fault("a shared variable has no cell").into()),
                        }
                    }
                    SlotState::Empty => {
                        unbound(&exe.code(frame.code).slots[local.slot as usize].name)
                    }
                }
            }
            Var::Session(name) => match self.session.get(name) {
                Some(value) => Ok(value.clone()),
                None => unbound(name),
            },
            Var::Unbound(name) => unbound(name),
        }
    }

    fn write_var(&mut self, task: TaskId, exe: &Executable, var: &Var, value: Value) -> Eval<()> {
        let unbound = |name: &lash_kernel_doc::Name| {
            Err(Interrupt::Raise(Value::Error(Arc::new(
                lash_kernel_doc::ErrorValue {
                    kind: "unbound_variable".to_owned(),
                    message: format!("no variable `{name}` is bound here"),
                    data: Value::text(name.as_str()),
                },
            ))))
        };
        self.reserve(value_bytes(&value))?;
        match var {
            Var::Local(local) => {
                let frame = self.frame(task)?;
                match &mut frame.slots[local.at as usize] {
                    SlotState::Value(held) => *held = value,
                    SlotState::Cell(cell) => {
                        let cell = *cell;
                        if let Some(Obj::Variable(held)) = self.heap.get_mut(cell) {
                            *held = value;
                        }
                    }
                    SlotState::Empty => {
                        return unbound(&exe.code(frame.code).slots[local.slot as usize].name);
                    }
                }
                Ok(())
            }
            Var::Session(name) => match self.session.get_mut(name) {
                Some(held) => {
                    *held = value;
                    Ok(())
                }
                None => unbound(name),
            },
            Var::Unbound(name) => unbound(name),
        }
    }

    fn exec(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        id: StmtId,
        stmt: &Stmt,
    ) -> Eval<Ran> {
        let out = self.regs.len();
        match stmt {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } => match value {
                // A value at once completes the statement, which no action
                // holds open.
                Rhs::Expr(expr) => {
                    self.eval(task, host, exe, expr)?;
                    self.complete_stmt(task, host, exe, stmt, out)?;
                }
                Rhs::Action(action) => match self.act(task, exe, id, action, out)? {
                    Flow::Done => self.complete_stmt(task, host, exe, stmt, out)?,
                    Flow::Step => return Ok(Ran::Step),
                },
            },
            Stmt::Do(action) => {
                if self.act(task, exe, id, action, out)? == Flow::Step {
                    return Ok(Ran::Step);
                }
            }
            Stmt::Remove(member) => self.remove(task, host, exe, member)?,
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let block = match self.eval_value(task, host, exe, condition)? {
                    Value::Bool(true) => *then_block,
                    Value::Bool(false) => *else_block,
                    _ => return raise("type_error", "an `if` condition must be a bool"),
                };
                // An empty block would end as soon as it began.
                if !exe.block(block).stmts.is_empty() {
                    self.push_block(task, block)?;
                    return Ok(Ran::Step);
                }
            }
            Stmt::For { iterable, .. } => {
                self.eval(task, host, exe, iterable)?;
                return Ok(ran(self.for_start(task, host, exe, id, out)?));
            }
            Stmt::While { .. } => {
                self.frame(task)?.control.push(Control::While {
                    stmt: id,
                    started: 0,
                });
                return Ok(ran(self.next_iteration(task, host, exe)?));
            }
            Stmt::Break => return Ok(Ran::Depart(Completion::Break)),
            Stmt::Continue => return Ok(Ran::Depart(Completion::Continue)),
            Stmt::Return(value) => {
                let value = self.eval_value(task, host, exe, value)?;
                if task == TaskId::MAIN && self.task(task)?.frames.len() == 1 {
                    // The run's result leaves the run (`K-EFF-002`): a value
                    // that cannot is refused here, where a `catch` sees it.
                    self.copy_out(&value)?;
                }
                return Ok(Ran::Depart(Completion::Return(value)));
            }
            Stmt::Try { body, .. } => {
                self.frame(task)?.control.push(Control::Try {
                    stmt: id,
                    phase: TryPhase::Body,
                });
                self.push_block(task, *body)?;
                return Ok(Ran::Step);
            }
            Stmt::Throw(value) => {
                let value = self.eval_value(task, host, exe, value)?;
                return Err(Interrupt::Raise(value));
            }
            Stmt::Print(value) => {
                let value = self.eval_value(task, host, exe, value)?;
                let datum = self.copy_out(&value)?;
                host.print(&datum);
            }
            Stmt::Finish(value) => {
                let value = self.eval_value(task, host, exe, value)?;
                return Err(Halt::Finish(self.copy_out(&value)?).into());
            }
            Stmt::Fail(value) => {
                let value = self.eval_value(task, host, exe, value)?;
                return Err(Halt::Fail(self.copy_out(&value)?).into());
            }
        }
        Ok(Ran::Done)
    }

    /// Copies a value out of the run, charging its deep size
    /// (`K-EFF-002`, `K-CHG-002`).
    pub(super) fn copy_out(&mut self, value: &Value) -> Eval<lash_kernel_doc::Datum> {
        self.charge(deep_size(&self.heap, value))?;
        Ok(copy_out(&self.heap, value)?)
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
            Incoming::Value(value) => Ok(value),
            Incoming::Raise(value) => Err(Interrupt::Raise(value)),
            Incoming::Join(joined) => self.join_result(joined),
            Incoming::Outcome(answered) => self.resume_outcome(task, exe, answered.outcome),
        }
    }

    fn resume_outcome(&mut self, task: TaskId, exe: &Executable, outcome: Outcome) -> Eval<Value> {
        match outcome {
            Outcome::Elapsed => Ok(Value::Null),
            Outcome::Failed(error) => {
                let datum = lash_kernel_doc::Datum::Error(Box::new(error));
                Err(Interrupt::Raise(self.decode(
                    exe,
                    &datum,
                    &lash_kernel_doc::Type::Any,
                )?))
            }
            Outcome::Completed(datum) => {
                let awaiting = self.frame(task)?.awaiting;
                let ty = awaiting.and_then(|stmt| match exe.stmt(stmt) {
                    Stmt::Let {
                        value: Rhs::Action(action),
                        ..
                    }
                    | Stmt::Assign {
                        value: Rhs::Action(action),
                        ..
                    }
                    | Stmt::Do(action) => match &action.kind {
                        crate::compile::ActionKind::Perform { result, .. } => Some(result),
                        _ => None,
                    },
                    _ => None,
                });
                match ty {
                    Some(ty) => self.decode(exe, &datum, ty),
                    None => Err(fault("an outcome arrived at no `perform`").into()),
                }
            }
        }
    }

    /// The result of a task that has ended, as a `join` on its handle
    /// gives it: its value, or a raise of its error, which is then
    /// observed (`K-TASK-016`).
    pub(super) fn join_result(&mut self, joined: TaskId) -> Eval<Value> {
        let joined = self.task(joined)?;
        match &joined.state {
            TaskState::Ended(Ok(value)) => Ok(value.clone()),
            TaskState::Ended(Err(value)) => {
                let value = value.clone();
                joined.observed = true;
                Err(Interrupt::Raise(value))
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
        let decoder = Decoder {
            policy: self.program.document.manifest.numbers,
            declared: &|name| exe.declared.contains_key(name),
        };
        let decoded = match decoder.decode(datum, ty) {
            Ok(decoded) => decoded,
            Err(problem) => {
                return raise(
                    "effect_result",
                    format!("the result does not fit the stated type: {problem}"),
                );
            }
        };
        let value = self.materialise(&decoded)?;
        self.charge(deep_size(&self.heap, &value))?;
        Ok(value)
    }

    /// Completes the statement the top frame waits in with the value its
    /// action yielded, in `value`: binds it, writes it, or drops it
    /// (`K-EVAL-003`). Gives the statement.
    pub(super) fn finish_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Reg,
    ) -> Eval<StmtId> {
        let Some(stmt) = self.frame(task)?.awaiting.take() else {
            return Err(fault("a value arrived at no statement").into());
        };
        self.complete_stmt(task, host, exe, exe.stmt(stmt), value)?;
        Ok(stmt)
    }

    /// Completes a statement with the value of its right-hand side, in
    /// `value`.
    fn complete_stmt(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        stmt: &Stmt,
        value: Reg,
    ) -> Eval<()> {
        match stmt {
            Stmt::Let {
                target: Target::Slot(slot),
                ..
            } => {
                let value = self.take_top(value);
                self.bind(task, *slot, value)?;
            }
            Stmt::Let {
                target: Target::Session(name),
                ..
            } => {
                let value = self.take_top(value);
                self.reserve(value_bytes(&value))?;
                self.session.insert(name.clone(), value);
            }
            Stmt::Assign { place, .. } => match place {
                Place::Var(var) => {
                    let value = self.take_top(value);
                    self.write_var(task, exe, var, value)?;
                }
                Place::Member(member) => self.write_member(task, host, exe, member, value)?,
            },
            _ => {}
        }
        Ok(())
    }

    /// Writes a field or an index (`K-FORM-005`, `K-FORM-006`). The target
    /// and the index are evaluated here, after the right-hand side.
    fn write_member(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        member: &Member,
        value: Reg,
    ) -> Eval<()> {
        self.pin(value)?;
        let target = self.regs.len();
        match member {
            Member::Field(target_expr, field) => {
                self.eval(task, host, exe, target_expr)?;
                self.write_field(target, field, value)
            }
            Member::Index(target_expr, index) => {
                self.eval(task, host, exe, target_expr)?;
                self.eval(task, host, exe, index)?;
                self.write_index(target, target + 1, value)
            }
        }
    }

    /// `remove` (`K-FORM-007`).
    fn remove(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        member: &Member,
    ) -> Eval<()> {
        let target = self.regs.len();
        match member {
            Member::Field(target_expr, field) => {
                self.eval(task, host, exe, target_expr)?;
                self.remove_field(target, field)
            }
            Member::Index(target_expr, index) => {
                self.eval(task, host, exe, target_expr)?;
                self.eval(task, host, exe, index)?;
                self.remove_index(target, target + 1)
            }
        }
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
