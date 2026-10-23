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

/// Borrowed public ABI carried to the activation that owns it, including when
/// a standing segment is replaced after another yield.
#[derive(Clone, Copy)]
pub(super) struct ExportBoundary<'a> {
    pub(super) convention: &'a lash_kernel_doc::StartConvention,
    pub(super) result: &'a lash_vocabulary::types::Type,
}

impl<'a> From<&'a lash_kernel_doc::FunctionExport> for ExportBoundary<'a> {
    fn from(export: &'a lash_kernel_doc::FunctionExport) -> Self {
        Self {
            convention: &export.convention,
            result: &export.signature.result,
        }
    }
}

/// Arguments and the continuation owned by one call activation.
#[derive(Clone, Copy)]
pub(super) struct Invocation<'a> {
    pub(super) args: Reg,
    pub(super) count: usize,
    pub(super) awaiting: Option<StmtId>,
    pub(super) export: Option<ExportBoundary<'a>>,
}

impl<'a> Invocation<'a> {
    pub(super) fn new(args: Reg, count: usize) -> Self {
        Self {
            args,
            count,
            awaiting: None,
            export: None,
        }
    }
    pub(super) fn awaited_by(mut self, awaiting: Option<StmtId>) -> Self {
        self.awaiting = awaiting;
        self
    }
    pub(super) fn with_boundary(mut self, export: Option<ExportBoundary<'a>>) -> Self {
        self.export = export;
        self
    }
}

/// A function about to run in a new frame.
pub(super) struct Call<'a> {
    pub(super) code: CodeId,
    pub(super) link: Option<lash_vocabulary::content_ids::LinkId>,
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
    /// The public contract belongs to this activation, before any callbacks enter.
    pub(super) export: Option<ExportBoundary<'a>>,
}

impl<'a> Call<'a> {
    pub(super) fn new(code: CodeId, args: Reg, count: usize) -> Self {
        Self {
            code,
            link: None,
            args,
            count,
            captures: &[],
            library: None,
            awaited_by: None,
            export: None,
        }
    }

    pub(super) fn exported(mut self, export: Option<&'a lash_kernel_doc::FunctionExport>) -> Self {
        self.export = export.map(ExportBoundary::from);
        self
    }

    pub(super) fn with_boundary(mut self, boundary: Option<ExportBoundary<'a>>) -> Self {
        self.export = boundary;
        self
    }

    pub(super) fn in_link(mut self, link: lash_vocabulary::content_ids::LinkId) -> Self {
        self.link = Some(link);
        self
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
        let link = self.current_link();
        let mut arena = self.arena.borrow_mut();
        Ok(match literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Float(value) => Value::Float(*value),
            Literal::Int(value) => arena.int(value.clone()),
            Literal::Text(value) => arena.text(value),
            Literal::Bytes(value) => arena.bytes(value.as_slice()),
            Literal::Function(function) => arena.function(lash_vocabulary::names::CallableId {
                link,
                function: *function,
            }),
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
        self.active_link = self.frame(task)?.link;
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
        let index = frame.control.len().wrapping_sub(1);
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
                let was = std::mem::replace(phase, TryPhase::Body);
                // A catch block that ends recovered from what it took.
                let caught = matches!(was, TryPhase::Catch);
                let completion = match (was, finally) {
                    (TryPhase::Finally(departure), _) => {
                        frame.control.pop();
                        departure
                    }
                    (_, Some(finally)) => {
                        *phase = TryPhase::Finally(Completion::Normal);
                        self.push_block(task, finally)?;
                        Completion::Normal
                    }
                    (_, None) => {
                        frame.control.pop();
                        Completion::Normal
                    }
                };
                if caught {
                    self.catch_left(task, index, None)?;
                }
                Ok(completion)
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
            if matches!(departure, Completion::Throw(_)) {
                let at = super::raises::unwinding(self.frame(task)?, exe);
                self.raise_unwinds(task, at);
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
            let index = frame.control.len().wrapping_sub(1);
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
                    match self.call_hook(task, exe, *policy, 0, 1, Some(stmt)) {
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
                                Err(interrupt) => {
                                    Completion::Throw(self.thrown_by(task, interrupt)?)
                                }
                            };
                        }
                        Err(interrupt) => {
                            departure = Completion::Throw(self.thrown_by(task, interrupt)?)
                        }
                    }
                    continue;
                }
                let mut result = match departure {
                    Completion::Return(value) => Ok(*value),
                    Completion::Throw(value) => Err(*value),
                    _ => return Err(fault("a break or a continue left its function")),
                };
                let settle = result.is_ok()
                    || !matches!(
                        self.frame(task)?.convention,
                        Some(
                            lash_kernel_doc::StartConvention::TsDirect { .. }
                                | lash_kernel_doc::StartConvention::TsArgsList { .. }
                        )
                    );
                if settle && let Some(cleanup) = self.root_cleanup(task, exe)? {
                    self.raise_held();
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
                            Err(interrupt) => Completion::Throw(self.thrown_by(task, interrupt)?),
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
                    // A departure out of a catch block decides what it
                    // took: a raise undoes the take, anything else keeps it.
                    let leaving = matches!(phase, TryPhase::Catch).then(|| match &departure {
                        Completion::Throw(value) => Some(**value),
                        _ => None,
                    });
                    match (&*phase, catch, finally, &departure) {
                        (TryPhase::Body, Some((binding, block)), _, Completion::Throw(value)) => {
                            let value = **value;
                            *phase = TryPhase::Catch;
                            self.push_block(task, block)?;
                            self.bind(task, binding.at, binding.shared, value)?;
                            self.raise_held();
                            self.catch_took(task, index, &value)?;
                            departure = Completion::Normal;
                        }
                        (TryPhase::Body | TryPhase::Catch, _, Some(finally), _) => {
                            *phase = TryPhase::Finally(departure);
                            self.push_block(task, finally)?;
                            self.raise_held();
                            departure = Completion::Normal;
                        }
                        // A `finally` that leaves by its own departure
                        // replaces the one it interrupted.
                        _ => {
                            frame.control.pop();
                        }
                    }
                    if let Some(raised) = leaving {
                        self.catch_left(task, index, raised.as_ref())?;
                    }
                }
            }
        }
    }

    /// The run-root cleanup to call as the entry the run started on is
    /// left: its start convention's, when the departing frame is that
    /// entry's root frame.
    fn root_cleanup(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<Option<crate::compile::LinkedHook>, Halt> {
        let is_root = task == TaskId::MAIN && self.task(task)?.frames.len() == 1;
        let code = self.frame(task)?.code;
        match self.frame(task)?.convention.as_ref() {
            Some(
                lash_kernel_doc::StartConvention::PyDirect { cleanup }
                | lash_kernel_doc::StartConvention::TsDirect { await_fn: cleanup }
                | lash_kernel_doc::StartConvention::TsArgsList {
                    await_fn: cleanup, ..
                },
            ) => Ok(match cleanup {
                lash_kernel_doc::FunctionTarget::Local(function) => exe
                    .declared
                    .get(function)
                    .copied()
                    .map(crate::compile::LinkedHook::Code)
                    .or_else(|| {
                        exe.lib_of(function)
                            .map(crate::compile::LinkedHook::Library)
                    }),
                lash_kernel_doc::FunctionTarget::Import(slot) => {
                    Some(crate::compile::LinkedHook::Import(*slot, None))
                }
            }),
            Some(
                lash_kernel_doc::StartConvention::Direct
                | lash_kernel_doc::StartConvention::PyArgsList { .. },
            ) => Ok(None),
            None => Ok(if is_root {
                exe.root_cleanup(code)
            } else {
                None
            }),
        }
    }

    /// Calls the run-root cleanup `cleanup` with no source scope left, the
    /// frame holding the body's departure `result` while it runs. Answers
    /// what then ends the run: `result`, or the cleanup's raise; `None`
    /// while the cleanup's body runs, whose return `leave_frame` takes.
    fn clean_up(
        &mut self,
        task: TaskId,
        exe: &Executable,
        cleanup: crate::compile::LinkedHook,
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
        let settle = matches!(
            frame.convention,
            Some(
                lash_kernel_doc::StartConvention::TsDirect { .. }
                    | lash_kernel_doc::StartConvention::TsArgsList { .. }
            )
        );
        frame.cleaning = Some(result);
        let count = if settle {
            let value = frame
                .cleaning
                .as_ref()
                .and_then(|value| value.as_ref().ok())
                .copied()
                .ok_or_else(|| fault("settlement has no return value"))?;
            self.put(0, value);
            1
        } else {
            0
        };
        match self.call_hook(task, exe, cleanup, 0, count, None) {
            Ok(super::ops::Flow::Step) => Ok(None),
            Ok(super::ops::Flow::Done) => {
                let value = self.take(0);
                let original = self.frame(task)?.cleaning.take();
                Ok(if settle { Some(Ok(value)) } else { original })
            }
            Err(interrupt) => {
                let value = self.thrown_by(task, interrupt)?;
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
        let result = if let (Ok(value), Some(ty)) = (&result, &frame.public_result) {
            let lash_vocabulary::sites::Unit::Library(function) = exe.code(frame.code).site.unit
            else {
                return Err(fault("public result frame has no defining callable"));
            };
            match self.check_public_result(
                exe,
                value,
                ty,
                lash_vocabulary::CallableId {
                    link: frame.link,
                    function,
                },
            ) {
                Ok(()) => result,
                Err(error) => Err(*self.thrown_by(task, error)?),
            }
        } else {
            result
        };
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
        let spelled_as_json = task == TaskId::MAIN
            && self.task(task)?.frames.is_empty()
            && matches!(
                frame.convention,
                Some(
                    lash_kernel_doc::StartConvention::TsArgsList { .. }
                        | lash_kernel_doc::StartConvention::TsDirect { .. }
                )
            );
        self.observe(Event::Pop { task });
        self.recycle(frame);
        if self.task(task)?.frames.is_empty() {
            self.end_task(task, result)?;
            if spelled_as_json && let Some(super::End::Finished(finished)) = &mut self.end {
                undefined_as_json(&mut finished.result);
            }
            return Ok(None);
        }
        self.active_link = self.frame(task)?.link;
        // The run-root cleanup has returned or raised: the body's departure,
        // or the cleanup's raise, leaves the entry.
        if let Some(departed) = self.frame(task)?.cleaning.take() {
            let settled = matches!(
                self.frame(task)?.convention,
                Some(
                    lash_kernel_doc::StartConvention::TsDirect { .. }
                        | lash_kernel_doc::StartConvention::TsArgsList { .. }
                )
            );
            let result = if result.is_ok() && !settled {
                departed
            } else {
                result
            };
            return self.leave_frame(task, host, exe, result);
        }
        Ok(match result {
            Ok(value) => match self
                .segment_returned(task, host, exe, value, ResultOrigin::Returned)
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
                Err(interrupt) => Some(Completion::Throw(self.thrown_by(task, interrupt)?)),
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
            .map_err(|halt| halt.in_function(&exe.lib(lib).header.name))
    }

    /// Drop all guest holdings before retaining the activation's storage.
    pub(super) fn recycle(&mut self, mut frame: Box<Frame>) {
        frame.control.clear();
        frame.slots.clear();
        frame.pending.clear();
        frame.awaiting = None;
        frame.nested = None;
        frame.library = None;
        frame.convention = None;
        frame.public_result = None;
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
                frame.link = self.program.link;
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
                    link: self.program.link,
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
                    convention: None,
                    public_result: None,
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
            link,
            code: code_id,
            args: base,
            count: given,
            captures,
            awaited_by,
            export,
        } = call;
        let code = exe.code(code_id);
        // Native start frames hold the completion formula just like library
        // fallback frames, whether reached by a root, import, or spawn.
        let library = library
            .or_else(|| match &code.site.unit {
                lash_vocabulary::sites::Unit::Library(function)
                    if exe.native_starts.get(function) == Some(&code_id) =>
                {
                    exe.lib_of(function)
                }
                _ => None,
            })
            .filter(|_| !code.charged);
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
        let defining_link = match link {
            Some(link) => link,
            None => self
                .task(task)?
                .frames
                .last_mut()?
                .map(|frame| frame.link)
                .unwrap_or(self.program.link),
        };
        let mut frame = self.fresh_frame(exe, code_id)?;
        frame.link = defining_link;
        frame.convention = export.map(|export| export.convention.clone());
        frame.public_result = export.map(|export| export.result.clone());
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
        for ((_, local), cell) in code.free.iter().zip(captures) {
            frame.set_slot(local.at, SlotState::Cell(*cell));
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

    /// A TypeScript public return still belongs to its settlement hook. Only
    /// the settled value crosses the run boundary; a promise may hold tasks.
    pub(super) fn return_leaves_run(&mut self, task: TaskId) -> Result<bool, Halt> {
        if task != TaskId::MAIN || self.task(task)?.frames.len() != 1 {
            return Ok(false);
        }
        Ok(!matches!(
            self.frame(task)?.convention,
            Some(
                lash_kernel_doc::StartConvention::TsDirect { .. }
                    | lash_kernel_doc::StartConvention::TsArgsList { .. }
            )
        ))
    }

    /// Host and data-only import results fit the public settled signature,
    /// whose types can differ from the function's private ABI.
    pub(super) fn check_public_result(
        &mut self,
        exe: &Executable,
        value: &Value,
        ty: &lash_vocabulary::types::Type,
        callable: lash_vocabulary::CallableId,
    ) -> Eval<()> {
        if let Some(mismatch) =
            self.public_value_fits(exe, *value, ty, self.limits.value_depth())?
        {
            self.raise_typed(
                "entry_type_mismatch",
                &lash_vocabulary::EntryTypeMismatch {
                    callable,
                    mismatch: mismatch.within("output"),
                },
            )
        } else {
            Ok(())
        }
    }

    pub(super) fn raise_typed<T>(
        &mut self,
        kind: &str,
        payload: &impl serde::Serialize,
    ) -> Eval<T> {
        let data = lash_kernel_doc::diagnostic_datum(payload).map_err(|e| fault(&e.to_string()))?;
        let datum = lash_kernel_doc::Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
            kind: kind.into(),
            message: kind.into(),
            data,
        }));
        self.raise_datum(datum)
    }

    fn raise_datum<T>(&mut self, datum: lash_kernel_doc::Datum) -> Eval<T> {
        let frozen = self.heap.freeze_datum(&datum)?;
        let page = self.heap.instantiate(&frozen)?;
        let value = self.admit_page(&page)?;
        Err(Interrupt::raised(value))
    }

    pub(super) fn decode_refusal<T>(&mut self, refusal: lash_kernel_doc::DecodeRefusal) -> Eval<T> {
        let error = refusal.error_datum().map_err(|e| fault(&e.to_string()))?;
        self.raise_datum(lash_kernel_doc::Datum::Error(Box::new(error)))
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

    /// Charges `units` of a `perform`'s boundary work, whatever code the
    /// `perform` stands in (`K-CHG-002`, `K-CHG-007`): its atoms, its
    /// arguments' copy-out, the bytes of each function value crossing in
    /// them, a read-bound result's decode and a delivered outcome's
    /// consumption. Nothing else is charged through it: a library body's
    /// own forms stay covered by its completion formula.
    pub(super) fn charge_boundary(&mut self, units: u64) -> Result<(), Halt> {
        if self.metered() {
            self.charge(units)
        } else {
            Ok(())
        }
    }

    /// Whether the run counts charge (`Charge::Exact`): what a boundary
    /// charge is measured under.
    fn metered(&self) -> bool {
        self.bounds.charge == crate::Charge::Exact
    }

    /// Copies the argument at `index` of a perform of `effect` out of the
    /// run, walked with the type the effect's requirement in the manifest
    /// declares for it: a function in it crosses as a function value where
    /// that type admits one ([`crate::crossing`]). The traversal is a
    /// boundary charge, as a copy-out is, and so is each byte of a function
    /// value's canonical form.
    pub(super) fn copy_out_tool_arg(
        &mut self,
        effect: &lash_vocabulary::names::OperationUse,
        index: usize,
        value: &Value,
    ) -> Eval<lash_kernel_doc::Datum> {
        if self.metered() {
            let size = self.deep_size(value)?;
            self.charge_boundary(size)?;
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
        let instance = self
            .program
            .instances
            .get(&effect.link)
            .ok_or_else(|| fault("operation Link is not loaded"))?;
        let document = Arc::clone(
            self.program
                .documents
                .get(&instance.document)
                .ok_or_else(|| fault("operation document is not loaded"))?,
        );
        let library = self.program.library.clone();
        let mut heap = crate::crossing::CrossingHeap {
            heap: &mut self.heap,
        };
        let mut run = crate::crossing::CrossingRun {
            captions: &self.captions,
            function_origins: &self.function_origins,
            documents: &self.program.documents,
            instances: &self.program.instances,
            catalog: library.registry().as_ref(),
            helpers: library.helpers(),
            selection: &self.program.selection,
            data: self.limits,
        };
        // Admission declared every operation a document or a body it
        // reaches performs; an argument past its parameters, or of an effect
        // the manifest does not list, is data.
        let param = document
            .imports
            .get(&effect.slot)
            .and_then(|import| match &import.kind {
                lash_vocabulary::names::ImportKind::Operation { signature } => {
                    signature.params.get(index)
                }
                _ => None,
            });
        let datum = match param {
            Some(param) => crate::crossing::copy_out_tool_arg(
                &mut heap,
                &mut run,
                value,
                param.name.as_str(),
                &param.ty,
            )?,
            None => crate::crossing::copy_out_tool_arg(
                &mut heap,
                &mut run,
                value,
                &format!("args[{index}]"),
                &lash_vocabulary::types::Type::Json,
            )?,
        };
        self.charge_boundary(crossed_bytes(&datum))?;
        Ok(datum)
    }

    /// Whether `value` reaches a closure or a function reference: what an
    /// effect argument crosses as a function value.
    fn reaches_function(&mut self, value: &Value) -> Result<bool, Halt> {
        fn function(value: &Value) -> bool {
            matches!(value, Value::Closure(_) | Value::Function(_))
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
        task: TaskId,
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
            CommittedOutcomeRecord::Rejected(frozen) => (
                self.heap.instantiate(&frozen).map_err(Interrupt::from)?,
                true,
            ),
            // Consuming the outcome is the `perform`'s boundary charge, in
            // the code it stands in, sealed since delivery or not.
            CommittedOutcomeRecord::Completed(frozen) | CommittedOutcomeRecord::Failed(frozen) => {
                let charge = if self.metered() {
                    self.heap.frozen_charge(&frozen)?
                } else {
                    0
                };
                let value = self.heap.instantiate(&frozen).map_err(Interrupt::from)?;
                self.charge_boundary(charge)?;
                (value, matches!(outcome, CommittedOutcomeRecord::Failed(_)))
            }
        };
        // run cannot seal inside a statement; installation and consumption
        // therefore publish together at the statement's safe point.
        self.waits.remove(&key, self.heap.pages())?;
        let value = self.admit_page(&value)?;
        if failed {
            self.wait_raised(task, wait, &value);
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
        let TaskState::Ended(ended) = &joined.state else {
            return Err(fault("a join resumed before its task ended").into());
        };
        match ended.result() {
            Some(Ok(value)) => {
                let value = value.clone();
                Ok(self.admit_page(&value)?)
            }
            Some(Err(value)) => {
                let value = value.clone();
                joined.observed = true;
                Err(Interrupt::raised(self.admit_page(&value)?))
            }
            None => Err(fault("a join read a retired task result").into()),
        }
    }

    /// Decodes an inbound datum by a type and builds it as a fresh graph,
    /// charging its deep size in charged code (`K-EFF-003`, `K-EFF-007`,
    /// `K-CHG-002`).
    pub(super) fn decode(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_vocabulary::types::Type,
    ) -> Eval<Value> {
        self.decode_as(exe, datum, ty, "the result", self.charging)
    }

    /// [`Self::decode`] of the error a read-bound `perform` was answered
    /// with: a boundary charge, in whatever code the `perform` stands.
    pub(super) fn decode_answer(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_vocabulary::types::Type,
    ) -> Eval<Value> {
        let metered = self.metered();
        self.decode_as(exe, datum, ty, "the result", metered)
    }

    /// [`Self::decode_answer`] of the result `effect` answered a read-bound
    /// `perform` with: a raise that it does not fit names the effect.
    pub(super) fn decode_result(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_vocabulary::types::Type,
        effect: &lash_vocabulary::names::OperationUse,
    ) -> Eval<Value> {
        let metered = self.metered();
        self.decode_as(
            exe,
            datum,
            ty,
            &format!("the result of `{effect:?}`"),
            metered,
        )
    }

    /// Decodes and builds `datum`, charging its deep size when `charged`.
    fn decode_as(
        &mut self,
        exe: &Executable,
        datum: &lash_kernel_doc::Datum,
        ty: &lash_vocabulary::types::Type,
        _what: &str,
        charged: bool,
    ) -> Eval<Value> {
        let decoder = lash_kernel_doc::Decoder {
            max_depth: self.limits.value_depth(),
            policy: exe
                .instances
                .get(&self.current_link())
                .map_or(self.program.document.numbers, |document| document.numbers),
            declared: &|callable| {
                exe.instances
                    .get(&callable.link)
                    .is_some_and(|doc| doc.functions.contains_key(&callable.function))
            },
        };
        let decoded = match decoder.decode(datum, ty) {
            Ok(decoded) => decoded,
            Err(refusal) => return self.decode_refusal(refusal),
        };
        let frozen = self.heap.freeze_datum(&decoded).map_err(Interrupt::from)?;
        let charge = if charged {
            self.heap.frozen_charge(&frozen)?
        } else {
            0
        };
        let value = self.heap.instantiate(&frozen).map_err(Interrupt::from)?;
        let value = self.admit_page(&value)?;
        if charged {
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
            if self.return_leaves_run(task)? {
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

/// `value` with each `undefined` spelled as `JSON.stringify` spells it: a
/// record or map member that holds it is left out, and anywhere else,
/// the value itself or a list or tuple element, it is `null`.
fn undefined_as_json(value: &mut lash_kernel_doc::Datum) {
    use lash_kernel_doc::Datum;
    match value {
        Datum::Absent => *value = Datum::Null,
        Datum::List(items) | Datum::Tuple(items) => items.iter_mut().for_each(undefined_as_json),
        Datum::Record(fields) => {
            fields.retain(|(_, field)| !matches!(field, Datum::Absent));
            for (_, field) in fields {
                undefined_as_json(field);
            }
        }
        Datum::Map(entries) => {
            entries.retain(|(_, entry)| !matches!(entry, Datum::Absent));
            for (_, entry) in entries {
                undefined_as_json(entry);
            }
        }
        _ => {}
    }
}
