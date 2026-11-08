//! The core's dispatcher (cutover contract §1, §3, §4): one
//! `loop { match op }` over the running task's frames, billing the event
//! contract as it goes.
//!
//! A block head prepays its fixed cost when it fits before both the limit
//! and the slice's end; otherwise the block runs event by event, each op
//! billing its own event at its start. The per-op check is one flag,
//! `exact`, so a prepaid block's ops pay nothing more than the dispatch.
//! Under `Charge::Off` the same machinery runs over the pace tables, with
//! no limit and no dynamic fee.

pub(crate) mod calls;
pub(crate) mod charge;
pub(crate) mod datum;
pub(crate) mod frames;
pub(crate) mod loops;
pub(crate) mod machine;
pub(crate) mod members;
pub(crate) mod plans;

use std::collections::HashMap;
use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::arena::native::{NativeError, NewObject};
use lash_kernel_doc::{Datum, Measure, TaskId, Value};
use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::names::FunctionId;
use lash_kernel_value::sites::Site;

use crate::bytecode::charge_tables::RESULT;
use crate::bytecode::op::Op;
use crate::bytecode::program::{Constant, Program, Unit, UnitId, UnitKind};
use crate::heap::{Heap, MemoryRefused};
use crate::{Bound, ChargeLimiter, Host, LimitDecision};
use charge::{Meter, Refused};
use frames::{Bill, Context, Frame, Handler};

/// Why [`Vm::run`] returned.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The task's bottom frame returned this value, copied out.
    Returned(Datum),
    /// A raise no handler of the task caught.
    Uncaught(Value),
    /// `finish` ended the run with this copied-out value.
    Finish(Datum),
    /// `fail` ended the run with this copied-out value.
    Fail(Datum),
    /// The top frame stands after action `n` of its unit, the action's
    /// operands on the stack (`K-EVAL-008`).
    Action(u32),
    /// The slice ended at a step's end (`K-MACH-002`).
    Slice,
    /// A bound refused the run.
    Bound(Box<Exceeded>),
    /// Infrastructure: the heap or a native broke an invariant, or the
    /// program reached code the core could not compile.
    Fault(String),
}

impl Stop {
    pub(crate) fn bound(exceeded: Exceeded) -> Self {
        Self::Bound(Box::new(exceeded))
    }
}

/// A bound a run passed.
#[derive(Clone, Debug)]
pub(crate) struct Exceeded {
    pub bound: Bound,
    pub limit: u64,
    /// The library function whose guard refused, or the unit running.
    pub function: Option<Arc<str>>,
    pub site: Option<Site>,
}

/// How an op leaves the straight line.
#[derive(Debug)]
pub(crate) enum Flow {
    /// A guest raise, not yet caught.
    Raise(Value),
    /// Boxed: a stop is rare, and the hot path's `Result` stays small.
    Stop(Box<Stop>),
}

impl From<Stop> for Flow {
    fn from(stop: Stop) -> Self {
        Self::Stop(Box::new(stop))
    }
}

pub(crate) type Run<T> = Result<T, Flow>;

pub(crate) fn fault(what: impl Into<String>) -> Flow {
    Flow::from(Stop::Fault(what.into()))
}

/// The run's limits on values (`K-VAL-034`).
pub(crate) fn value_depth() -> usize {
    KernelDataLimits::standard().value_depth()
}

/// The machine state of one instance over a heap `H`: the program, the
/// heap, the meter and the running task's context. The scheduler keeps the
/// other tasks' contexts in [`Vm::parked`] and swaps them in.
pub(crate) struct Vm<H: Heap> {
    /// The instance's own program, shared only so the dispatch loop can
    /// borrow it beside the machine's mutable state.
    pub program: Arc<Program>,
    pub heap: H,
    /// The program's constants, materialized in this instance's heap.
    constants: Vec<Value>,
    /// The host bindings main's free names read and write.
    pub globals: Vec<Value>,
    /// The running task's frames and stack.
    pub cx: Context,
    /// The contexts of the tasks not running, for the collector.
    pub parked: Vec<Context>,
    /// Values the scheduler holds outside any context (results, operands
    /// of issued actions), for the collector.
    pub held: Vec<Value>,
    meter: Meter,
    /// The run is under `Charge::Off`: the pace tables, no fee.
    paced: bool,
    /// The current block runs event by event.
    exact: bool,
    /// The next op is the first of a run call: its step is not tested.
    fresh: bool,
    pub call_depth: u32,
    pub limiter: Option<Box<dyn ChargeLimiter>>,
    /// The total a charge refusal reported, once one has.
    pub refused: Option<u64>,
    /// Each closure function's body unit.
    closures: HashMap<FunctionId, UnitId>,
    /// Each function value's trampoline.
    functions: HashMap<FunctionId, UnitId>,
    /// The other unfinished tasks, for `tasks.unfinished` (`K-TASK-021`).
    pub unfinished: Vec<TaskId>,
}

impl<H: Heap> Vm<H> {
    /// An instance of `program` over `heap`, its constants placed and
    /// admitted. `limit` is the charge limit under `Charge::Exact`;
    /// `paced` selects `Charge::Off`.
    pub(crate) fn new(
        program: Arc<Program>,
        mut heap: H,
        used: u64,
        limit: u64,
        paced: bool,
        call_depth: u32,
    ) -> Result<Self, Stop> {
        let mut constants = Vec::with_capacity(program.constants.len());
        for constant in &program.constants {
            let object = match constant {
                Constant::Immediate(value) => {
                    constants.push(*value);
                    continue;
                }
                Constant::Text(text) => NewObject::LongText(text.clone()),
                Constant::BigInt(int) => NewObject::BigInt(int.clone()),
                Constant::Bytes(bytes) => NewObject::Bytes(bytes.clone()),
                Constant::Function(callable) => NewObject::Function(*callable),
            };
            if let Err(refused) = heap.admit(crate::heap::slots::schedule(&object)) {
                return Err(Stop::bound(memory(refused)));
            }
            let value = heap
                .create(object)
                .map_err(|error| Stop::Fault(error.to_string()))?;
            constants.push(value);
        }
        let mut closures = HashMap::new();
        for (at, unit) in program.units.iter().enumerate() {
            if let UnitKind::Closure(function) = unit.kind {
                closures.insert(function, at as UnitId);
            }
        }
        let functions = program
            .functions
            .iter()
            .map(|entry| (entry.function, entry.unit))
            .collect();
        let globals = vec![Value::Absent; program.globals.len()];
        Ok(Self {
            program,
            heap,
            constants,
            globals,
            cx: Context::default(),
            parked: Vec::new(),
            held: Vec::new(),
            meter: Meter::new(used, if paced { u64::MAX } else { limit }),
            paced,
            exact: true,
            fresh: false,
            call_depth,
            limiter: None,
            refused: None,
            closures,
            functions,
            unfinished: Vec::new(),
        })
    }

    /// What the run has paid (pace units under `Charge::Off`).
    pub(crate) fn charged(&self) -> u64 {
        self.meter.total(self.through())
    }

    pub(crate) fn charge_limit(&self) -> u64 {
        self.meter.limit()
    }

    pub(crate) fn paced(&self) -> bool {
        self.paced
    }

    /// Starts unit `unit` as the running context's bottom frame, its
    /// `args` its parameters.
    pub(crate) fn start(
        &mut self,
        unit: UnitId,
        args: &[Value],
        closure: Option<ArenaHandle>,
    ) -> Run<()> {
        self.cx.stack.extend_from_slice(args);
        self.enter(unit, args.len(), closure, None)
    }

    // --- The meter.

    /// The fixed charge the running block has billed through the op last
    /// dispatched.
    fn through(&self) -> u64 {
        if !self.meter.prepaid() {
            return 0;
        }
        let Some(frame) = self.cx.frames.last() else {
            return 0;
        };
        let unit = &self.program.units[frame.unit as usize];
        let at = (frame.pc as usize).saturating_sub(1);
        let table = if self.paced {
            &unit.pace_through
        } else {
            &unit.through
        };
        table.get(at).copied().unwrap_or(0)
    }

    /// Leaves the running block early at the op last dispatched: its
    /// reservation is cancelled once.
    fn cancel(&mut self) {
        let through = self.through();
        self.meter.unbill(through);
        self.exact = true;
    }

    /// Asks the limiter about a refusal, retrying what it left unbilled
    /// while the limiter grows the limit.
    fn refusal(&mut self, mut refused: Refused) -> Run<()> {
        loop {
            let decision = match self.limiter.as_mut() {
                Some(limiter) => limiter.limit_reached(refused.total, self.meter.limit()),
                None => LimitDecision::Refuse,
            };
            match decision {
                LimitDecision::Grow(more) if more > 0 => {
                    self.meter.grow(more);
                    match self.meter.retry(refused.rest) {
                        Ok(()) => return Ok(()),
                        Err(again) => refused = again,
                    }
                }
                _ => {
                    self.refused = Some(refused.total);
                    return Err(Flow::from(Stop::bound(
                        self.exceeded(Bound::Charge, self.meter.limit()),
                    )));
                }
            }
        }
    }

    /// A bound passed in the running frame.
    fn exceeded(&self, bound: Bound, limit: u64) -> Exceeded {
        let unit = self
            .cx
            .frames
            .last()
            .map(|frame| &self.program.units[frame.unit as usize]);
        Exceeded {
            bound,
            limit,
            function: unit.map(|unit| unit.name.clone()),
            site: unit.map(|unit| unit.site.clone()),
        }
    }

    /// Bills op `pc`'s fixed event on the exact path.
    #[inline]
    fn bill(&mut self, unit: &Unit, pc: usize) -> Run<()> {
        if self.paced {
            let pace = unit.pace[pc];
            if pace != 0
                && let Err(refused) = self.meter.units(u64::from(pace))
            {
                self.refusal(refused)?;
            }
            return Ok(());
        }
        let event = unit.event[pc];
        if event.units != 0
            && let Err(refused) = self.meter.units(event.units)
        {
            self.refusal(refused)?;
        }
        if event.members != 0
            && let Err(refused) = self.meter.atomic(event.members)
        {
            self.refusal(refused)?;
        }
        Ok(())
    }

    /// Bills a dynamic fee (a completion formula, a copy-out): the running
    /// block continues event by event. Under `Charge::Off` it bills nothing.
    pub(crate) fn fee(&mut self, units: u64) -> Run<()> {
        if self.paced {
            return Ok(());
        }
        let through = self.through();
        self.exact = true;
        if let Err(refused) = self.meter.fee(units, through) {
            self.refusal(refused)?;
        }
        Ok(())
    }

    /// Bills a completion formula (`K-CHG-003`): over the call's arguments
    /// and its result, of size 0 when the call raised.
    fn complete(&mut self, charge: u32, args: &[Value], result: Option<&Value>) -> Run<()> {
        if self.paced {
            return Ok(());
        }
        let units = self.formula(charge, args, result)?;
        self.fee(units)
    }

    /// Evaluates formula `charge` over `args` and `result`.
    fn formula(&self, charge: u32, args: &[Value], result: Option<&Value>) -> Run<u64> {
        let formula = &self.program.charges[charge as usize];
        if let Some(units) = formula.constant() {
            return Ok(units);
        }
        let arena = self.heap.arena();
        let mut failed = None;
        let units = formula.evaluate(&mut |position, how| {
            let value = if position == RESULT {
                result
            } else {
                args.get(usize::from(position))
            };
            let Some(value) = value else {
                return 0;
            };
            let measured = match how {
                Measure::Size => crate::data::size(arena, value),
                Measure::DeepSize => crate::data::deep_size(arena, value),
                Measure::NestedSize => crate::data::nested_size(arena, value),
                Measure::Magnitude => Ok(crate::data::magnitude(arena, value)),
            };
            measured.unwrap_or_else(|error| {
                failed = Some(error);
                0
            })
        });
        match failed {
            Some(error) => Err(fault(error.to_string())),
            None => Ok(units),
        }
    }

    // --- The heap.

    /// Admits `units`, collecting once before it refuses (contract §2).
    fn admit(&mut self, units: u64) -> Run<()> {
        if self.heap.admit(units).is_ok() {
            return Ok(());
        }
        self.collect();
        self.heap
            .admit(units)
            .map_err(|refused| Flow::from(Stop::bound(memory(refused))))
    }

    /// Places `object`, admitted first.
    fn alloc(&mut self, object: NewObject) -> Run<Value> {
        self.admit(crate::heap::slots::schedule(&object))?;
        self.heap.create(object).map_err(|error| self.native(error))
    }

    fn cell(&mut self, value: Value) -> Run<ArenaHandle> {
        self.admit(crate::heap::slots::CELL)?;
        Ok(self.heap.cell(value))
    }

    /// Mark-sweep from every value the machine holds.
    pub(crate) fn collect(&mut self) {
        let Self {
            heap,
            constants,
            globals,
            cx,
            parked,
            held,
            ..
        } = self;
        heap.collect(&mut |visit| {
            constants.iter().for_each(&mut *visit);
            globals.iter().for_each(&mut *visit);
            held.iter().for_each(&mut *visit);
            cx.roots(&mut *visit);
            for other in parked.iter() {
                other.roots(&mut *visit);
            }
        });
    }

    /// The raise or stop a native error is.
    fn native(&mut self, error: NativeError) -> Flow {
        match error {
            NativeError::Raised(value) => Flow::Raise(value),
            NativeError::Refused { kind, message } => self.error(kind, &message),
            NativeError::Guard(guard) => Flow::from(Stop::bound(Exceeded {
                bound: Bound::Charge,
                limit: guard.limit,
                function: None,
                site: None,
            })),
            other => fault(other.to_string()),
        }
    }

    /// The raise of a new error of `kind` and `message` (`K-ERR-002`).
    pub(crate) fn error(&mut self, kind: &str, message: &str) -> Flow {
        if let Err(flow) = self.admit(crate::heap::slots::ERROR) {
            return flow;
        }
        match self.heap.error(kind, message) {
            Ok(value) => Flow::Raise(value),
            Err(error) => fault(error.to_string()),
        }
    }

    /// Copies `value` out of the run, billing its deep size first
    /// (`K-EFF-002`, `K-CHG-005`).
    pub(crate) fn copy_out(&mut self, value: &Value) -> Run<Datum> {
        if !self.paced {
            let size = crate::data::deep_size(self.heap.arena(), value)
                .map_err(|error| fault(error.to_string()))?;
            self.fee(size)?;
        }
        match crate::data::copy_out(self.heap.arena(), value_depth(), value) {
            Ok(datum) => Ok(datum),
            Err(crate::data::CopyError::Raised(raised)) => {
                Err(self.error(raised.kind, &raised.message))
            }
            Err(crate::data::CopyError::Heap(error)) => Err(fault(error.to_string())),
        }
    }

    // --- Frames.

    /// Pushes a frame of `unit` whose `argc` arguments are the top of the
    /// stack: missing parameters are absent (`K-FN-004`), and each local a
    /// closure shares is boxed into a cell (`K-CLO-001`).
    fn enter(
        &mut self,
        unit: UnitId,
        argc: usize,
        closure: Option<ArenaHandle>,
        bill: Option<Bill>,
    ) -> Run<()> {
        if self.cx.frames.len() >= self.call_depth as usize {
            return Err(Flow::from(Stop::bound(
                self.exceeded(Bound::CallDepth, u64::from(self.call_depth)),
            )));
        }
        let program = Arc::clone(&self.program);
        let code = &program.units[unit as usize];
        let base = self.cx.stack.len() - argc;
        self.cx.stack.resize(
            base + usize::from(code.locals.max(argc as u16)),
            Value::Absent,
        );
        for &slot in &code.boxed {
            let at = base + usize::from(slot);
            let value = self.cx.stack[at];
            let cell = self.cell(value)?;
            self.cx.stack[at] = Value::Handle(cell);
        }
        self.cx.frames.push(Frame {
            unit,
            pc: 0,
            base: base as u32,
            closure,
            bill,
        });
        Ok(())
    }

    #[inline]
    fn base(&self) -> usize {
        self.cx.frames.last().map_or(0, |frame| frame.base as usize)
    }

    #[inline]
    fn pop(&mut self) -> Value {
        self.cx.stack.pop().unwrap_or(Value::Absent)
    }

    #[inline]
    fn push(&mut self, value: Value) {
        self.cx.stack.push(value);
    }

    /// The cell local `slot` of the running frame holds.
    fn cell_at(&self, slot: u16) -> Run<ArenaHandle> {
        match self.cx.stack[self.base() + usize::from(slot)] {
            Value::Handle(cell) => Ok(cell),
            _ => Err(fault("a shared local holds no cell")),
        }
    }

    /// The cell of the running closure's capture `n`.
    fn capture(&self, n: u16) -> Run<ArenaHandle> {
        let closure = self
            .cx
            .frames
            .last()
            .and_then(|frame| frame.closure)
            .ok_or_else(|| fault("a capture outside a closure"))?;
        let (_, captures, _) = self
            .heap
            .arena()
            .closure(closure)
            .map_err(|error| fault(error.to_string()))?;
        captures
            .get(usize::from(n))
            .copied()
            .ok_or_else(|| fault("a capture past the closure's cells"))
    }

    /// Unwinds to the innermost handler of the running task: each frame
    /// left bills its completion formula with a result of size 0
    /// (`K-CHG-003`). Without a handler the task's raise is uncaught.
    fn raise(&mut self, value: Value) -> Run<()> {
        self.cancel();
        loop {
            let Some(top) = self.cx.frames.len().checked_sub(1) else {
                return Err(Flow::from(Stop::Uncaught(value)));
            };
            if let Some(handler) = self.cx.handlers.last().copied()
                && handler.frame as usize == top
            {
                self.cx.handlers.pop();
                self.cx.stack.truncate(handler.height as usize);
                self.cx.stack.push(value);
                if let Some(frame) = self.cx.frames.last_mut() {
                    frame.pc = handler.pc;
                }
                return Ok(());
            }
            let Some(frame) = self.cx.frames.pop() else {
                return Err(Flow::from(Stop::Uncaught(value)));
            };
            self.drop_handlers();
            if let Some(bill) = &frame.bill {
                self.complete(bill.charge, &bill.args, None)?;
            }
            self.cx.stack.truncate(frame.base as usize);
            if self.cx.frames.is_empty() {
                return Err(Flow::from(Stop::Uncaught(value)));
            }
        }
    }

    /// Drops the handlers of frames that are gone: a `return` inside a
    /// `try` leaves its handlers with its frame.
    fn drop_handlers(&mut self) {
        let frames = self.cx.frames.len();
        while self
            .cx
            .handlers
            .last()
            .is_some_and(|handler| handler.frame as usize >= frames)
        {
            self.cx.handlers.pop();
        }
    }

    /// The action's result arrives: the top frame continues with it.
    pub(crate) fn resume(&mut self, value: Value) {
        self.push(value);
    }

    /// The action raised: the top frame unwinds from it.
    pub(crate) fn resume_raise(&mut self, value: Value) -> Result<(), Stop> {
        match self.raise(value) {
            Ok(()) => Ok(()),
            Err(Flow::Stop(stop)) => Err(*stop),
            Err(Flow::Raise(_)) => Err(Stop::Fault("a raise while raising".into())),
        }
    }

    // --- The dispatch loop.

    /// Runs the running context until it stops: its bottom frame returns,
    /// it reaches an action, the slice of `slice` units ends, or the run
    /// ends.
    pub(crate) fn run(&mut self, host: &mut dyn Host, slice: u64) -> Stop {
        self.meter.slice(slice);
        self.fresh = true;
        loop {
            match self.dispatch(host) {
                Ok(stop) => return stop,
                Err(Flow::Raise(value)) => {
                    if let Err(flow) = self.raise(value) {
                        return match flow {
                            Flow::Stop(stop) => *stop,
                            Flow::Raise(_) => Stop::Fault("a raise while raising".into()),
                        };
                    }
                }
                Err(Flow::Stop(stop)) => {
                    self.cancel();
                    return *stop;
                }
            }
        }
    }

    fn dispatch(&mut self, host: &mut dyn Host) -> Run<Stop> {
        let program = Arc::clone(&self.program);
        loop {
            let Some(frame) = self.cx.frames.last() else {
                return Err(fault("a context with no frame ran"));
            };
            let (unit, pc, base) = (frame.unit, frame.pc as usize, frame.base as usize);
            let unit = &program.units[unit as usize];
            let op = unit.ops[pc];
            let fresh = std::mem::take(&mut self.fresh);
            if let Op::Block(_) = op {
                // The block before ended: what it reserved is spent, and
                // the charge is exact at the boundary.
                self.meter.leave();
                self.exact = true;
            }
            if self.exact {
                if unit.step[pc] && !fresh && self.meter.spent() {
                    return Ok(Stop::Slice);
                }
                self.bill(unit, pc)?;
            }
            if let Some(frame) = self.cx.frames.last_mut() {
                frame.pc += 1;
            }
            match op {
                Op::Block(block) => {
                    let block = &unit.blocks[block as usize];
                    let cost = if self.paced { block.pace } else { block.charge };
                    self.exact = !self.meter.enter(cost);
                }
                Op::Nop => {}
                Op::Const(n) => self.push(self.constants[n as usize]),
                Op::Local(n) => self.push(self.cx.stack[base + usize::from(n)]),
                Op::SetLocal(n) => {
                    let value = self.pop();
                    self.cx.stack[base + usize::from(n)] = value;
                }
                Op::CellGet(n) => {
                    let cell = self.cell_at(n)?;
                    let value = self
                        .heap
                        .cell_get(cell)
                        .map_err(|error| self.native(error))?;
                    self.push(value);
                }
                Op::CellSet(n) => {
                    let cell = self.cell_at(n)?;
                    let value = self.pop();
                    self.heap
                        .cell_set(cell, value)
                        .map_err(|error| self.native(error))?;
                }
                Op::Box(n) => {
                    let at = base + usize::from(n);
                    let value = self.cx.stack[at];
                    let cell = self.cell(value)?;
                    self.cx.stack[at] = Value::Handle(cell);
                }
                Op::CaptureGet(n) => {
                    let cell = self.capture(n)?;
                    let value = self
                        .heap
                        .cell_get(cell)
                        .map_err(|error| self.native(error))?;
                    self.push(value);
                }
                Op::CaptureSet(n) => {
                    let cell = self.capture(n)?;
                    let value = self.pop();
                    self.heap
                        .cell_set(cell, value)
                        .map_err(|error| self.native(error))?;
                }
                Op::Global(n) => self.push(self.globals[usize::from(n)]),
                Op::SetGlobal(n) => {
                    let value = self.pop();
                    self.globals[usize::from(n)] = value;
                }
                Op::Pop => {
                    self.pop();
                }
                Op::Jump(to) => self.jump(to),
                Op::JumpIfFalse(to) => match self.pop() {
                    Value::Bool(true) => {}
                    Value::Bool(false) => self.jump(to),
                    _ => return Err(self.error("type_error", "an `if` condition must be a bool")),
                },
                Op::WhileFalse(to) => match self.pop() {
                    Value::Bool(true) => {}
                    Value::Bool(false) => self.jump(to),
                    _ => return Err(self.error("type_error", "a `while` condition must be a bool")),
                },
                Op::MakeTuple(n) => self.make_tuple(n as usize)?,
                Op::MakeList(n) => {
                    let values = self.take(n as usize);
                    let value = self.alloc(NewObject::List(values))?;
                    self.push(value);
                }
                Op::MakeSet(n) => {
                    let values = self.take(n as usize);
                    for value in &values {
                        self.key(value)?;
                    }
                    let value = self.alloc(NewObject::Set(values))?;
                    self.push(value);
                }
                Op::MakeMap(n) => {
                    let flat = self.take(2 * n as usize);
                    let pairs: Vec<(Value, Value)> = flat
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&[key, value]| (key, value))
                        .collect();
                    for (key, _) in &pairs {
                        self.key(key)?;
                    }
                    let value = self.alloc(NewObject::Map(pairs))?;
                    self.push(value);
                }
                Op::MakeRecord(shape) => {
                    let shape = Arc::clone(&unit.records[shape as usize]);
                    let values = self.take(shape.len());
                    let pairs = shape
                        .iter()
                        .map(|field| self.constants[*field as usize])
                        .zip(values)
                        .collect();
                    let value = self.alloc(NewObject::Record(pairs))?;
                    self.push(value);
                }
                Op::MakeClosure(n) => {
                    let site = &unit.closures[n as usize];
                    let mut captures = Vec::with_capacity(site.captures.len());
                    for capture in &site.captures {
                        captures.push(match capture {
                            crate::bytecode::program::Capture::Local(slot) => {
                                self.cell_at(*slot)?
                            }
                            crate::bytecode::program::Capture::Capture(k) => self.capture(*k)?,
                        });
                    }
                    self.admit(crate::heap::slots::closure_units(captures.len()))?;
                    let closure = self.heap.closure(Arc::clone(&site.identity), captures);
                    self.push(Value::Closure(closure));
                }
                Op::Index => {
                    let index = self.pop();
                    let target = self.pop();
                    let value = self.index(target, index)?;
                    self.push(value);
                }
                Op::Field(field) => {
                    let target = self.pop();
                    let name = self.constants[field as usize];
                    let value = self.field(target, name)?;
                    self.push(value);
                }
                Op::SetIndex => {
                    let index = self.pop();
                    let target = self.pop();
                    let value = self.pop();
                    self.set_index(target, index, value)?;
                }
                Op::SetIndexPlaceFirst => {
                    let value = self.pop();
                    let index = self.pop();
                    let target = self.pop();
                    self.set_index(target, index, value)?;
                }
                Op::SetField(field) => {
                    let target = self.pop();
                    let value = self.pop();
                    self.set_field(target, self.constants[field as usize], value)?;
                }
                Op::SetFieldPlaceFirst(field) => {
                    let value = self.pop();
                    let target = self.pop();
                    self.set_field(target, self.constants[field as usize], value)?;
                }
                Op::RemoveIndex => {
                    let index = self.pop();
                    let target = self.pop();
                    self.remove_index(target, index)?;
                }
                Op::RemoveField(field) => {
                    let target = self.pop();
                    self.remove_field(target, self.constants[field as usize])?;
                }
                Op::Call(site) => self.call(&program, unit, site)?,
                Op::Return => {
                    let value = self.pop();
                    if let Some(stop) = self.ret(value, true)? {
                        return Ok(stop);
                    }
                }
                Op::Fall => {
                    if let Some(stop) = self.ret(Value::Null, false)? {
                        return Ok(stop);
                    }
                }
                Op::Raise => {
                    let value = self.pop();
                    return Err(Flow::Raise(value));
                }
                Op::TryEnter(to) => {
                    let frame = (self.cx.frames.len() - 1) as u32;
                    let height = self.cx.stack.len() as u32;
                    self.cx.handlers.push(Handler {
                        frame,
                        pc: to,
                        height,
                    });
                }
                Op::TryExit => {
                    self.cx.handlers.pop();
                }
                Op::ForOpen(n) => self.for_open(unit, n)?,
                Op::ForNext(n) => self.for_next(unit, n)?,
                Op::ProtoOpen(n) => self.proto_open(unit, n)?,
                Op::ProtoStep(n) => self.proto_step(unit, n)?,
                Op::ProtoGuard(n) => self.proto_guard(unit, n)?,
                Op::ProtoChecked(n) => self.proto_checked(unit, n)?,
                Op::ProtoPull(n) => self.proto_pull(unit, n)?,
                Op::ProtoMore(n) => self.proto_more(unit, n)?,
                Op::Action(n) => {
                    self.meter.leave();
                    self.exact = true;
                    return Ok(Stop::Action(n));
                }
                Op::Print => {
                    let value = self.pop();
                    let datum = self.copy_out(&value)?;
                    host.print(&datum);
                }
                Op::Finish => {
                    let value = self.pop();
                    let datum = self.copy_out(&value)?;
                    return Ok(Stop::Finish(datum));
                }
                Op::Fail => {
                    let value = self.pop();
                    let datum = self.copy_out(&value)?;
                    return Ok(Stop::Fail(datum));
                }
                Op::Unavailable(reason) => {
                    let reason = match &program.constants[reason as usize] {
                        Constant::Text(text) => text.to_string(),
                        Constant::Immediate(Value::Text(text)) => text.as_str().to_owned(),
                        _ => String::from("a body the core cannot run"),
                    };
                    return Err(fault(reason));
                }
            }
        }
    }

    /// Returns `value` from the running frame, billing the call's
    /// completion formula. Gives the stop when the task's bottom frame
    /// returned.
    fn ret(&mut self, value: Value, explicit: bool) -> Run<Option<Stop>> {
        self.meter.leave();
        self.exact = true;
        // A return that leaves the run copies its value out where it
        // stands, so a raise of the copy is caught by the frame's own
        // handlers; falling off the end copies nothing.
        let leaving = match self.cx.frames.len() {
            1 if explicit => Some(self.copy_out(&value)?),
            1 => Some(Datum::Null),
            _ => None,
        };
        let Some(frame) = self.cx.frames.pop() else {
            return Err(fault("a return with no frame"));
        };
        self.drop_handlers();
        if let Some(bill) = &frame.bill {
            self.complete(bill.charge, &bill.args, Some(&value))?;
        }
        self.cx.stack.truncate(frame.base as usize);
        if let Some(datum) = leaving {
            return Ok(Some(Stop::Returned(datum)));
        }
        self.push(value);
        Ok(None)
    }

    #[inline]
    fn jump(&mut self, to: u32) {
        if let Some(frame) = self.cx.frames.last_mut() {
            frame.pc = to;
        }
    }

    /// The top `n` operands, in order.
    fn take(&mut self, n: usize) -> Vec<Value> {
        let from = self.cx.stack.len() - n;
        self.cx.stack.split_off(from)
    }

    /// Refuses a value that is no legal key (`K-KEY-002`).
    fn key(&mut self, value: &Value) -> Run<()> {
        match self.heap.arena().key(value) {
            Ok(_) => Ok(()),
            Err(error) => Err(self.native(error)),
        }
    }

    /// A tuple of the top `n` operands, refused past the value depth
    /// (`K-VAL-034`).
    fn make_tuple(&mut self, n: usize) -> Run<()> {
        let values = self.take(n);
        let arena = self.heap.arena();
        let depth = values
            .iter()
            .map(|value| arena.summary_of(value).depth)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let limit = value_depth();
        if depth as usize > limit {
            return Err(self.error(
                "too_deep",
                &format!("a value exceeds the configured {limit}-level value depth"),
            ));
        }
        let value = self.alloc(NewObject::Tuple(values))?;
        self.push(value);
        Ok(())
    }
}

fn memory(refused: MemoryRefused) -> Exceeded {
    Exceeded {
        bound: Bound::Memory,
        limit: refused.held.saturating_add(refused.wanted),
        function: None,
        site: None,
    }
}
