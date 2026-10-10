//! Statements and control: blocks, loops, `try`, calls and returns.

use std::sync::Arc;

use lash_kernel_doc::{Formula, Measure, ObjectId, Operand, TaskId, Value};

use super::{
    Completion, Control, Cursor, Eval, Frame, Halt, Incoming, Interrupt, KernelMachine,
    LibraryCall, SlotState, TaskState, TryPhase, bound, fault, raise,
};
use crate::compile::{
    BlockId, CodeId, Executable, LibId, Member, Place, Rhs, Slot, Stmt, StmtId, Target, Var,
};
use crate::data::{Decoder, copy_out, deep_size, magnitude, size};
use crate::heap::{Key, Obj, value_bytes};
use crate::interface::{Bound, Host, Outcome};

/// A function about to run in a new frame.
pub(super) struct Call<'a> {
    pub(super) code: CodeId,
    pub(super) args: Vec<Value>,
    /// The cells of the variables a closure shares, in its code's order.
    pub(super) captures: &'a [ObjectId],
    /// The library function the code is the body of.
    pub(super) library: Option<LibId>,
    /// The call sits inside an expression and runs to its end there.
    pub(super) inline: bool,
}

impl<'a> Call<'a> {
    pub(super) fn new(code: CodeId, args: Vec<Value>) -> Self {
        Self {
            code,
            args,
            captures: &[],
            library: None,
            inline: false,
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
        self.refresh_charging(task, exe);
        let frame = self.frame(task)?;
        match frame.control.last_mut() {
            Some(Control::Block { block, next }) => {
                let stmts = &exe.blocks[block.0 as usize].stmts;
                match stmts.get(*next) {
                    Some(stmt) => {
                        *next += 1;
                        self.charge(1)?;
                        self.exec(task, host, exe, *stmt)
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
            let code = &exe.codes[frame.code.0 as usize];
            for slot in &exe.blocks[block.0 as usize].declares {
                frame.slots[code.positions[*slot as usize] as usize] = SlotState::Empty;
            }
        }
        Ok(())
    }

    fn push_block(&mut self, task: TaskId, block: BlockId) -> Result<(), Halt> {
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
                let finally = match &exe.stmts[stmt.0 as usize] {
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
                    self.pins.push(value.clone());
                }
                Completion::Break | Completion::Continue => {}
            }
            let frame = self.frame(task)?;
            // The statement the call stood in is left behind.
            frame.awaiting = None;
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
                            Ok(()) => Completion::Normal,
                            Err(Interrupt::Raise(value)) => Completion::Throw(value),
                            Err(Interrupt::Halt(halt)) => return Err(halt),
                        };
                    }
                    _ => {
                        frame.control.pop();
                    }
                },
                Control::Try { stmt, phase } => {
                    let (catch, finally) = match &exe.stmts[stmt.0 as usize] {
                        Stmt::Try { catch, finally, .. } => (*catch, *finally),
                        _ => (None, None),
                    };
                    match (&*phase, catch, finally, &departure) {
                        (TryPhase::Body, Some((binding, block)), _, Completion::Throw(value)) => {
                            let value = value.clone();
                            *phase = TryPhase::Catch;
                            self.push_block(task, block)?;
                            self.bind(task, exe, binding, value)?;
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
            self.charge_call(exe, call.lib, &call.args, result.as_ref().ok())?;
        }
        if frame.inline {
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
                self.pin(&value)?;
                match self.finish_action(task, host, exe, value) {
                    Ok(()) => None,
                    Err(Interrupt::Raise(value)) => Some(Completion::Throw(value)),
                    Err(Interrupt::Halt(halt)) => return Err(halt),
                }
            }
            Err(value) => Some(Completion::Throw(value)),
        })
    }

    /// Charges a library call its definition's formula (`K-CHG-003`). A
    /// call that raised has a result of size 0.
    pub(super) fn charge_call(
        &mut self,
        exe: &Executable,
        lib: LibId,
        args: &[Value],
        result: Option<&Value>,
    ) -> Result<(), Halt> {
        let definition = &exe.libs[lib.0 as usize].definition;
        let units = self.formula(
            &definition.charge,
            &definition.signature.params,
            args,
            result,
        );
        self.charge(units)
            .map_err(|halt| halt.in_function(&definition.name))
    }

    pub(super) fn formula(
        &self,
        formula: &Formula,
        params: &[lash_kernel_doc::Param],
        args: &[Value],
        result: Option<&Value>,
    ) -> u64 {
        formula.evaluate(&mut |operand, measure| {
            let value = match operand {
                Operand::Result => result,
                Operand::Param(name) => params
                    .iter()
                    .position(|param| param.name == *name)
                    .and_then(|index| args.get(index)),
            };
            match (value, measure) {
                (None, _) => 0,
                (Some(value), Measure::Size) => size(&self.heap, value),
                (Some(value), Measure::DeepSize) => deep_size(&self.heap, value),
                (Some(value), Measure::Magnitude) => magnitude(value),
            }
        })
    }

    /// Pushes a frame that runs `code` with `args` bound to its
    /// parameters, in order; a parameter with no argument is absent
    /// (`K-FN-004`).
    pub(super) fn push_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: Call<'_>,
    ) -> Eval<()> {
        let Call {
            code: code_id,
            args,
            captures,
            library,
            inline,
        } = call;
        let code = &exe.codes[code_id.0 as usize];
        if args.len() > code.params.len() {
            return raise(
                "arity",
                format!(
                    "the function takes {} argument(s); {} given",
                    code.params.len(),
                    args.len()
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
        let mut frame = Frame {
            code: code_id,
            slots: vec![SlotState::Empty; code.slots.len()],
            control: vec![Control::Block {
                block: code.body,
                next: 0,
            }],
            awaiting: None,
            library: library.map(|lib| LibraryCall {
                lib,
                args: args.clone(),
            }),
            inline,
        };
        for (capture, cell) in code.captures.iter().zip(captures) {
            frame.slots[code.positions[capture.inner as usize] as usize] = SlotState::Cell(*cell);
        }
        self.task(task)?.frames.push(frame);
        let mut args = args.into_iter();
        for param in &code.params {
            let value = args.next().unwrap_or(Value::Absent);
            self.bind(task, exe, *param, value)?;
        }
        Ok(())
    }

    /// Binds a new variable in the top frame (`K-FORM-004`).
    pub(super) fn bind(
        &mut self,
        task: TaskId,
        exe: &Executable,
        slot: Slot,
        value: Value,
    ) -> Result<(), Halt> {
        let code = self.frame(task)?.code;
        let code = &exe.codes[code.0 as usize];
        let state = if code.slots[slot as usize].shared {
            SlotState::Cell(self.alloc(Obj::Variable(value))?)
        } else {
            self.reserve(value_bytes(&value))?;
            SlotState::Value(value)
        };
        self.frame(task)?.slots[code.positions[slot as usize] as usize] = state;
        Ok(())
    }

    pub(super) fn read_var(&mut self, task: TaskId, exe: &Executable, var: &Var) -> Eval<Value> {
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
            Var::Local(slot) => {
                let frame = self.frame(task)?;
                let code = &exe.codes[frame.code.0 as usize];
                match &frame.slots[code.positions[*slot as usize] as usize] {
                    SlotState::Value(value) => Ok(value.clone()),
                    SlotState::Cell(cell) => {
                        let cell = *cell;
                        match self.heap.variable(cell) {
                            Some(value) => Ok(value.clone()),
                            None => Err(fault("a shared variable has no cell").into()),
                        }
                    }
                    SlotState::Empty => unbound(&code.slots[*slot as usize].name),
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
            Var::Local(slot) => {
                let frame = self.frame(task)?;
                let code = &exe.codes[frame.code.0 as usize];
                match &mut frame.slots[code.positions[*slot as usize] as usize] {
                    SlotState::Value(held) => *held = value,
                    SlotState::Cell(cell) => {
                        let cell = *cell;
                        if let Some(Obj::Variable(held)) = self.heap.get_mut(cell) {
                            *held = value;
                        }
                    }
                    SlotState::Empty => return unbound(&code.slots[*slot as usize].name),
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
    ) -> Eval<Completion> {
        match &exe.stmts[id.0 as usize] {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } => {
                let value = match value {
                    Rhs::Expr(expr) => Some(self.eval(task, host, exe, expr)?),
                    Rhs::Action(action) => self.act(task, exe, id, action)?,
                };
                if let Some(value) = value {
                    self.frame(task)?.awaiting = Some(id);
                    self.finish_action(task, host, exe, value)?;
                }
            }
            Stmt::Do(action) => {
                self.act(task, exe, id, action)?;
            }
            Stmt::Remove(member) => self.remove(task, host, exe, member)?,
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let block = match self.eval(task, host, exe, condition)? {
                    Value::Bool(true) => *then_block,
                    Value::Bool(false) => *else_block,
                    _ => return raise("type_error", "an `if` condition must be a bool"),
                };
                self.push_block(task, block)?;
            }
            Stmt::For { iterable, .. } => {
                let cursor = match self.eval(task, host, exe, iterable)? {
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
                    stmt: id,
                    cursor,
                    started: 0,
                });
                self.next_iteration(task, host, exe)?;
            }
            Stmt::While { .. } => {
                self.frame(task)?.control.push(Control::While {
                    stmt: id,
                    started: 0,
                });
                self.next_iteration(task, host, exe)?;
            }
            Stmt::Break => return Ok(Completion::Break),
            Stmt::Continue => return Ok(Completion::Continue),
            Stmt::Return(value) => {
                let value = self.eval(task, host, exe, value)?;
                if task == TaskId::MAIN && self.task(task)?.frames.len() == 1 {
                    // The run's result leaves the run (`K-EFF-002`): a value
                    // that cannot is refused here, where a `catch` sees it.
                    self.copy_out(&value)?;
                }
                return Ok(Completion::Return(value));
            }
            Stmt::Try { body, .. } => {
                self.frame(task)?.control.push(Control::Try {
                    stmt: id,
                    phase: TryPhase::Body,
                });
                self.push_block(task, *body)?;
            }
            Stmt::Throw(value) => {
                let value = self.eval(task, host, exe, value)?;
                return Err(Interrupt::Raise(value));
            }
            Stmt::Print(value) => {
                let value = self.eval(task, host, exe, value)?;
                let datum = self.copy_out(&value)?;
                host.print(&datum);
            }
            Stmt::Finish(value) => {
                let value = self.eval(task, host, exe, value)?;
                return Err(Halt::Finish(self.copy_out(&value)?).into());
            }
            Stmt::Fail(value) => {
                let value = self.eval(task, host, exe, value)?;
                return Err(Halt::Fail(self.copy_out(&value)?).into());
            }
        }
        Ok(Completion::Normal)
    }

    /// Copies a value out of the run, charging its deep size
    /// (`K-EFF-002`, `K-CHG-002`).
    pub(super) fn copy_out(&mut self, value: &Value) -> Eval<lash_kernel_doc::Datum> {
        self.charge(deep_size(&self.heap, value))?;
        Ok(copy_out(&self.heap, value)?)
    }

    /// Tests the innermost loop's continuation and starts its next
    /// iteration or ends it (`K-ITER-002`, `K-ITER-003`, `K-FORM-013`).
    fn next_iteration(&mut self, task: TaskId, host: &mut dyn Host, exe: &Executable) -> Eval<()> {
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
                let Stmt::For { binding, body, .. } = &exe.stmts[stmt.0 as usize] else {
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
                        self.bind(task, exe, *binding, element)?;
                    }
                    None => {
                        frame.control.pop();
                    }
                }
            }
            Some(Control::While { stmt, .. }) => {
                let Stmt::While {
                    condition, body, ..
                } = &exe.stmts[stmt.0 as usize]
                else {
                    return Err(fault("a `while` loop is not at a `while`").into());
                };
                match self.eval(task, host, exe, condition)? {
                    Value::Bool(true) => {
                        if let Some(Control::While { started, .. }) =
                            self.frame(task)?.control.last_mut()
                        {
                            *started += 1;
                        }
                        self.push_block(task, *body)?;
                    }
                    Value::Bool(false) => {
                        self.frame(task)?.control.pop();
                    }
                    _ => return raise("type_error", "a `while` condition must be a bool"),
                }
            }
            _ => return Err(fault("no loop is innermost").into()),
        }
        Ok(())
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
                let ty = awaiting.and_then(|stmt| match &exe.stmts[stmt.0 as usize] {
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
    /// action yielded: binds it, writes it, or drops it (`K-EVAL-003`).
    pub(super) fn finish_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
    ) -> Eval<()> {
        let Some(stmt) = self.frame(task)?.awaiting.take() else {
            return Err(fault("a value arrived at no statement").into());
        };
        match &exe.stmts[stmt.0 as usize] {
            Stmt::Let {
                target: Target::Slot(slot),
                ..
            } => self.bind(task, exe, *slot, value)?,
            Stmt::Let {
                target: Target::Session(name),
                ..
            } => {
                self.reserve(value_bytes(&value))?;
                self.session.insert(name.clone(), value);
            }
            Stmt::Assign { place, .. } => match place {
                Place::Var(var) => self.write_var(task, exe, var, value)?,
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
        value: Value,
    ) -> Eval<()> {
        self.pin(&value)?;
        match member {
            Member::Field(target, field) => {
                let Value::Record(record) = self.eval(task, host, exe, target)? else {
                    return raise("type_error", "only a record has fields to assign");
                };
                self.write_record_field(record, field, value)?;
            }
            Member::Index(target, index) => {
                let target = self.eval(task, host, exe, target)?;
                let index = self.eval(task, host, exe, index)?;
                match target {
                    Value::List(list) => {
                        let length = self.heap.list(list).map_or(0, Vec::len);
                        let position =
                            super::eval::position(&index)?.filter(|position| *position <= length);
                        let Some(position) = position else {
                            return raise(
                                "index_out_of_range",
                                format!("a list of {length} is assigned at 0 to {length}"),
                            );
                        };
                        self.reserve(value_bytes(&value))?;
                        if let Some(Obj::List(items)) = self.heap.get_mut(list) {
                            match items.get_mut(position) {
                                Some(held) => *held = value,
                                None => items.push(value),
                            }
                        }
                    }
                    Value::Record(record) => {
                        let Value::Text(field) = index else {
                            return raise("type_error", "a record index must be text");
                        };
                        self.write_record_field(record, &field, value)?;
                    }
                    Value::Map(map) => {
                        let key = super::eval::key(&index)?;
                        self.reserve(value_bytes(&value).saturating_add(value_bytes(&index)))?;
                        if let Some(Obj::Map(table)) = self.heap.get_mut(map) {
                            table.insert(key, index, value);
                        }
                    }
                    Value::Set(set) => {
                        let key = super::eval::key(&index)?;
                        let Value::Bool(member) = value else {
                            return raise(
                                "type_error",
                                "a set's member is assigned `true` to add it or `false` to remove it",
                            );
                        };
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
            }
        }
        Ok(())
    }

    /// Field and text-index assignment share a record's insertion order.
    fn write_record_field(
        &mut self,
        record: lash_kernel_doc::ObjectId,
        field: &str,
        value: Value,
    ) -> Eval<()> {
        self.reserve(value_bytes(&value).saturating_add(field.len() as u64))?;
        if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
            match fields.iter_mut().find(|(name, _)| name == field) {
                Some(held) => held.1 = value,
                None => fields.push((field.to_string(), value)),
            }
        }
        Ok(())
    }

    /// `remove` (`K-FORM-007`).
    fn remove(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        member: &Member,
    ) -> Eval<()> {
        match member {
            Member::Field(target, field) => {
                let Value::Record(record) = self.eval(task, host, exe, target)? else {
                    return raise("type_error", "only a record has fields to remove");
                };
                if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
                    fields.retain(|(name, _)| name != field);
                }
            }
            Member::Index(target, index) => {
                let target = self.eval(task, host, exe, target)?;
                let index = self.eval(task, host, exe, index)?;
                match target {
                    Value::List(list) => {
                        let length = self.heap.list(list).map_or(0, Vec::len);
                        let position =
                            super::eval::position(&index)?.filter(|position| *position < length);
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
                        if let Some(Obj::Record(fields)) = self.heap.get_mut(record) {
                            fields.retain(|(name, _)| name != field.as_ref());
                        }
                    }
                    Value::Map(table) | Value::Set(table) => {
                        let key: Key = super::eval::key(&index)?;
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
            }
        }
        Ok(())
    }
}
