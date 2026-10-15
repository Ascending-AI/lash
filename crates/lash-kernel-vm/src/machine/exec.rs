//! Statements and control: blocks, loops, `try`, calls and returns.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorValue, Measure, ObjectId, TaskId, Value};
use lash_kernel_exec::LocalRef;

use super::ops::Reg;
use super::stream::{Ran, Running, unbound};
use super::{
    Completion, Control, Eval, Frame, FrameStorage, Halt, Incoming, Interrupt, KernelMachine,
    LibraryCall, SlotState, TaskState, TryPhase, bound, fault, raise,
};
use crate::compile::{BlockId, CodeId, Executable, Form, LibId, Plan, Source, StmtId, Var};
use crate::data::{copy_out, deep_size, magnitude, nested_size, size};
use crate::heap::{Key, Obj, Table, value_bytes};
use crate::interface::{Bound, Host, Outcome};
use crate::observe::Event;

/// A function about to run in a new frame.
pub(super) struct Call<'a> {
    pub(super) code: CodeId,
    /// The register the call's arguments start at.
    pub(super) args: Reg,
    /// How many arguments there are.
    pub(super) count: usize,
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
    pub(super) fn new(code: CodeId, args: Reg, count: usize) -> Self {
        Self {
            code,
            args,
            count,
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
        let code = frame.code;
        match frame.control.last_mut() {
            Some(Control::Block { block, next }) => {
                let stmts = &exe.block(*block).stmts;
                match stmts.get(*next) {
                    Some(&id) => {
                        *next += 1;
                        let last = self.steps.verify && *next == stmts.len();
                        self.charge(1)?;
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
                        match self.run(host, &running, start, self.top)? {
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
                    let (catch, finally) = match &exe.stmt(*stmt).form {
                        Form::Try { catch, finally, .. } => (*catch, *finally),
                        _ => (None, None),
                    };
                    match (&*phase, catch, finally, &departure) {
                        (TryPhase::Body, Some((binding, block)), _, Completion::Throw(value)) => {
                            let value = value.clone();
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
            let evaluates = self.charging && exe.lib(call.lib).native;
            let units = self.in_formula(call.lib, evaluates, |machine| {
                machine.call_units(exe, call.lib, &call.args, result.as_ref().ok())
            });
            self.charge_call(exe, call.lib, units)?;
        }
        self.observe(Event::Pop { task });
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
                let base = self.top;
                self.put(base, value);
                self.pin(base)?;
                match self.finish_action(task, host, exe, base) {
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
    /// (`K-FN-004`). The arguments are moved out of their registers.
    pub(super) fn push_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        call: Call<'_>,
    ) -> Eval<()> {
        let Call {
            code: code_id,
            args: base,
            count: given,
            captures,
            library,
            inline,
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
                    args.extend_from_slice(&self.regs[base..base + given]);
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
        self.observe(Event::Push {
            task,
            code: code_id,
        });
        for (index, param) in code.params.iter().enumerate() {
            let value = match index < given {
                true => std::mem::replace(&mut self.regs[base + index], Value::Absent),
                false => Value::Absent,
            };
            self.bind(task, param.at, param.shared, value)?;
        }
        Ok(())
    }

    /// Binds a new variable at frame position `at` of the top frame, in a
    /// heap cell when a closure shares it (`K-FORM-004`).
    pub(super) fn bind(
        &mut self,
        task: TaskId,
        at: u32,
        shared: bool,
        value: Value,
    ) -> Result<(), Halt> {
        let state = if shared {
            SlotState::Cell(self.alloc(Obj::Variable(value))?)
        } else {
            self.reserve(value_bytes(&value))?;
            SlotState::Value(value)
        };
        self.frame(task)?.slots[at as usize] = state;
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
            Var::Session(name) => match self.session.get(name) {
                Some(value) => Ok(value.clone()),
                None => unbound(name),
            },
            Var::Unbound(name) => unbound(name),
        }
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
                let ty = awaiting.and_then(|stmt| match &exe.stmt(stmt).form {
                    Form::Action(action) => match &action.kind {
                        crate::compile::ActionKind::Perform { effect, result, .. } => {
                            Some((effect, result))
                        }
                        _ => None,
                    },
                    _ => None,
                });
                match ty {
                    Some((effect, ty)) => self.decode_result(exe, &datum, ty, effect),
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
            max_depth: self.heap.value_depth,
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
        let value = self.materialise(&decoded)?;
        self.charge(deep_size(&self.heap, &value))?;
        Ok(value)
    }

    /// Completes the statement the top frame waits in with the value its
    /// action yielded, in register `base`: runs the statement on from its
    /// action, which binds, writes or drops the value (`K-EVAL-003`). Gives
    /// the statement.
    pub(super) fn finish_action(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        base: Reg,
    ) -> Eval<StmtId> {
        let Some(id) = self.frame(task)?.awaiting.take() else {
            return Err(fault("a value arrived at no statement").into());
        };
        let running = Running::new(task, exe, id);
        match self.run(host, &running, running.stmt.resume(), base)? {
            Ran::Done => Ok(id),
            _ => Err(fault("an action's statement did not complete").into()),
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
