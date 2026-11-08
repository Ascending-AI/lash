//! The core's dispatcher (cutover contract §1, §3): one `loop { match op }`
//! over a task's frames, billing the event contract as it goes.
//!
//! A block head prepays its fixed cost when it fits; otherwise the block
//! runs event by event, each op billing its own event at its start. The
//! per-op check is one flag, `exact`, so a prepaid block's ops pay nothing
//! more than the dispatch.

pub(crate) mod calls;
pub(crate) mod charge;
pub(crate) mod frames;
pub(crate) mod plans;

use std::sync::Arc;

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::native::{NativeError, NewObject};

use crate::bytecode::op::Op;
use crate::bytecode::program::{Constant, Program, UnitId};
use crate::heap::{Heap, MemoryRefused};
use charge::{Meter, Refused};
use frames::Frame;

/// Why the dispatch loop stopped short of a value.
#[derive(Debug)]
pub(crate) enum Flow {
    /// A guest raise, not yet caught by any handler of the task.
    Raise(Value),
    /// The charge limit refused an event.
    Charge(Refused),
    /// The memory bound refused an allocation after one collection.
    Memory(MemoryRefused),
    /// `finish` ended the run.
    Finish(Value),
    /// `fail` ended the run.
    Fail(Value),
    /// The arena or a native broke an invariant: infrastructure.
    Fault(NativeError),
    /// The compiler emitted an op this core does not run yet. The
    /// validator refuses such a program before it starts.
    Unsupported(Op),
}

impl Flow {
    pub(crate) fn native(error: NativeError) -> Self {
        match error {
            NativeError::Raised(value) => Self::Raise(value),
            other => Self::Fault(other),
        }
    }
}

/// One task's machine state over a heap `H`.
pub(crate) struct Vm<H: Heap> {
    /// The instance's own program; shared only so the dispatch loop can
    /// borrow it beside the machine's mutable state.
    program: Arc<Program>,
    heap: H,
    /// The program's constants, materialized in this instance's heap.
    constants: Vec<Value>,
    stack: Vec<Value>,
    frames: Vec<Frame>,
    meter: Meter,
    /// The current block runs event by event.
    exact: bool,
}

impl<H: Heap> Vm<H> {
    /// An instance of `program` over `heap`, its constants placed and
    /// admitted.
    pub(crate) fn new(program: Program, mut heap: H, limit: u64) -> Result<Self, Flow> {
        let mut constants = Vec::with_capacity(program.constants.len());
        for constant in &program.constants {
            let object = match constant {
                Constant::Immediate(value) => {
                    constants.push(*value);
                    continue;
                }
                Constant::LongText(text) | Constant::Field(text) => {
                    NewObject::LongText(text.clone())
                }
                Constant::BigInt(int) => NewObject::BigInt(int.clone()),
                Constant::Bytes(bytes) => NewObject::Bytes(bytes.clone()),
            };
            heap.admit(crate::heap::slots::schedule(&object))
                .map_err(Flow::Memory)?;
            constants.push(heap.create(object).map_err(Flow::native)?);
        }
        let main = program.main;
        let locals = program.units[main as usize].locals;
        let mut vm = Self {
            program: Arc::new(program),
            heap,
            constants,
            stack: Vec::new(),
            frames: Vec::new(),
            meter: Meter::new(0, limit),
            exact: true,
        };
        vm.push_frame(main, 0, None);
        vm.stack.resize(usize::from(locals), Value::Absent);
        Ok(vm)
    }

    fn push_frame(
        &mut self,
        unit: UnitId,
        base: u32,
        closure: Option<lash_kernel_doc::arena::ArenaHandle>,
    ) {
        self.frames.push(Frame {
            unit,
            pc: 0,
            base,
            closure,
            covered: false,
        });
    }

    /// The headroom a native's nursery may hold before it asks.
    fn room(&self) -> u64 {
        u64::MAX - self.heap.held()
    }

    /// Mark-sweep from every value the machine holds.
    fn collect(&mut self) {
        let Self {
            heap,
            constants,
            stack,
            frames,
            ..
        } = self;
        heap.collect(&mut |visit| {
            constants.iter().for_each(&mut *visit);
            stack.iter().for_each(&mut *visit);
            for frame in frames.iter() {
                if let Some(closure) = frame.closure {
                    visit(&Value::Closure(closure));
                }
            }
        });
    }

    /// What the run has paid at the current op.
    pub(crate) fn charged(&self) -> u64 {
        let Some(frame) = self.frames.last() else {
            return self.meter.total(0);
        };
        let unit = &self.program.units[frame.unit as usize];
        let through = unit.through.get(frame.pc as usize).copied().unwrap_or(0);
        self.meter.total(through)
    }

    /// Runs until the main frame returns or the run stops.
    pub(crate) fn run(&mut self) -> Result<Value, Flow> {
        let program = Arc::clone(&self.program);
        loop {
            let Some(frame) = self.frames.last_mut() else {
                return Ok(self.stack.pop().unwrap_or(Value::Null));
            };
            let unit = &program.units[frame.unit as usize];
            let pc = frame.pc as usize;
            let op = unit.ops[pc];
            frame.pc += 1;
            let base = frame.base as usize;
            let covered = frame.covered;
            if self.exact && !covered && !matches!(op, Op::Block(_)) {
                let event = unit.event[pc];
                if event != 0 {
                    self.meter.bill(event).map_err(Flow::Charge)?;
                }
            }
            match op {
                Op::Block(block) => {
                    self.meter.leave();
                    let block = &unit.blocks[block as usize];
                    self.exact = covered || !self.meter.enter(block.charge);
                    if self.exact && !covered {
                        let event = unit.event[pc];
                        if event != 0 {
                            self.meter.bill(event).map_err(Flow::Charge)?;
                        }
                    }
                }
                Op::Const(n) => self.stack.push(self.constants[n as usize]),
                Op::Local(n) => self.stack.push(self.stack[base + usize::from(n)]),
                Op::SetLocal(n) => {
                    let value = self.pop();
                    self.stack[base + usize::from(n)] = value;
                }
                Op::Pop => {
                    self.pop();
                }
                Op::Dup => {
                    let top = self.top();
                    self.stack.push(top);
                }
                Op::Jump(to) => self.jump(to),
                Op::JumpIfFalse(to) => match self.pop() {
                    Value::Bool(true) => {}
                    Value::Bool(false) => self.jump(to),
                    _ => {
                        let through = unit.through[pc];
                        return Err(self.raise_kind(
                            through,
                            "type_error",
                            "a condition is not a bool",
                        ));
                    }
                },
                Op::CallNative(native) => {
                    let through = unit.through[pc];
                    match self.call_native(&program, native) {
                        Ok(calls::Answer::Value(value)) => self.stack.push(value),
                        Ok(calls::Answer::Declined) => {
                            return Err(self.leave_block(through, Flow::Unsupported(op)));
                        }
                        Err(flow) => return Err(self.leave_block(through, flow)),
                    }
                }
                Op::CallUnit(callee) => {
                    let params = usize::from(program.units[callee as usize].params);
                    let locals = usize::from(program.units[callee as usize].locals);
                    let base = self.stack.len() - params;
                    self.stack.resize(base + locals, Value::Absent);
                    self.push_frame(callee, base as u32, None);
                }
                Op::Return => {
                    let value = self.pop();
                    self.meter.leave();
                    let done = self.frames.pop().map(|frame| frame.base as usize);
                    if let Some(base) = done {
                        self.stack.truncate(base);
                    }
                    self.stack.push(value);
                    self.exact = true;
                }
                Op::MakeList(n) => self.construct(n, NewObject::List, unit.through[pc])?,
                Op::MakeTuple(n) => self.construct(n, NewObject::Tuple, unit.through[pc])?,
                Op::Raise => {
                    let value = self.pop();
                    let through = unit.through[pc];
                    return Err(self.leave_block(through, Flow::Raise(value)));
                }
                Op::Finish => {
                    let value = self.pop();
                    let through = unit.through[pc];
                    return Err(self.leave_block(through, Flow::Finish(value)));
                }
                Op::Fail => {
                    let value = self.pop();
                    let through = unit.through[pc];
                    return Err(self.leave_block(through, Flow::Fail(value)));
                }
                other => {
                    let through = unit.through[pc];
                    return Err(self.leave_block(through, Flow::Unsupported(other)));
                }
            }
        }
    }

    /// Leaves the current block early at an op that has billed `through`
    /// of it: its reservation is cancelled once.
    fn leave_block(&mut self, through: u64, flow: Flow) -> Flow {
        self.meter.unbill(through);
        self.exact = true;
        flow
    }

    fn raise_kind(&mut self, through: u64, kind: &str, message: &str) -> Flow {
        match self.heap.error(kind, message) {
            Ok(error) => self.leave_block(through, Flow::Raise(error)),
            Err(fault) => self.leave_block(through, Flow::Fault(fault)),
        }
    }

    /// Builds a sequence of the top `n` operands.
    fn construct(
        &mut self,
        n: u32,
        make: fn(Vec<Value>) -> NewObject,
        through: u64,
    ) -> Result<(), Flow> {
        let from = self.stack.len() - n as usize;
        let object = make(self.stack.split_off(from));
        if let Err(refused) = self.heap.admit(crate::heap::slots::schedule(&object)) {
            self.collect();
            if self
                .heap
                .admit(crate::heap::slots::schedule(&object))
                .is_err()
            {
                return Err(self.leave_block(through, Flow::Memory(refused)));
            }
        }
        match self.heap.create(object) {
            Ok(value) => {
                self.stack.push(value);
                Ok(())
            }
            Err(fault) => Err(self.leave_block(through, Flow::native(fault))),
        }
    }

    #[inline]
    fn jump(&mut self, to: u32) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = to;
        }
    }

    #[inline]
    fn pop(&mut self) -> Value {
        self.stack.pop().unwrap_or(Value::Absent)
    }

    #[inline]
    fn top(&self) -> Value {
        self.stack.last().copied().unwrap_or(Value::Absent)
    }
}
