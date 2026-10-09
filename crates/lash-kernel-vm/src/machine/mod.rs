//! The kernel machine: one run of one document as a set of tasks.
//!
//! Each task is a stack of frames, and each frame a stack of the blocks,
//! loops and `try` statements it is inside. A task pauses only at an
//! action, between statements (`K-STMT-004`), so its whole state is that
//! stack and the variables: no value lives anywhere else while it waits.

mod actions;
mod eval;
mod exec;
mod session;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorValue, Identity, JoinMode, Name, ObjectId, TaskId, TaskIdentity, Value,
};

use crate::compile::{BlockId, CodeId, Executable, LibId, StmtId};
use crate::data::{Raised, copy_out};
use crate::heap::{Heap, Obj, object_bytes, refs, value_bytes};
use crate::interface::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, End, ExportError, Host, ImportError,
    Machine, MachineError, Meters, Outcome, Park, Program, Request, RunError, Start, StartError,
    Step, WaitId,
};
use crate::{Layout, Unparked};

/// What a frame is accounted before its variables.
const FRAME_BYTES: u64 = 64;
/// The deepest library bodies may nest inside expressions (`K-BND-002`).
const MAX_INLINE_DEPTH: u32 = 64;

/// What ends a run from inside a statement.
#[derive(Debug)]
pub(crate) enum Halt {
    Bound(BoundExceeded),
    Finish(Datum),
    Fail(Datum),
    /// The machine's own state is inconsistent.
    Fault(String),
}

/// Why a statement did not complete.
#[derive(Debug)]
pub(crate) enum Interrupt {
    Raise(Value),
    Halt(Halt),
}

type Eval<T> = Result<T, Interrupt>;

impl From<Halt> for Interrupt {
    fn from(halt: Halt) -> Self {
        Self::Halt(halt)
    }
}

impl From<Raised> for Interrupt {
    fn from(raised: Raised) -> Self {
        Self::Raise(error(raised.kind, raised.message))
    }
}

pub(crate) fn error(kind: &str, message: impl Into<String>) -> Value {
    Value::Error(Arc::new(ErrorValue::new(kind, message)))
}

fn raise<T>(kind: &'static str, message: impl Into<String>) -> Eval<T> {
    Err(Interrupt::Raise(error(kind, message)))
}

fn fault(problem: &str) -> Halt {
    Halt::Fault(problem.to_string())
}

fn bound(bound: Bound, limit: u64) -> Halt {
    Halt::Bound(BoundExceeded { bound, limit })
}

/// How a run that reached its end got there.
enum Ending {
    Returned(Value),
    Finished(Datum),
    Failed(Datum),
}

/// How control leaves a statement or a block.
#[derive(Clone, Debug)]
enum Completion {
    Normal,
    Break,
    Continue,
    Return(Value),
    Throw(Value),
}

#[derive(Clone, Debug)]
enum SlotState {
    Empty,
    Value(Value),
    /// A variable a closure shares: the heap cell that holds it.
    Cell(ObjectId),
}

/// A loop's position in what it iterates (`K-ITER-002`, `K-ITER-003`).
#[derive(Clone, Debug)]
enum Cursor {
    List(ObjectId, usize),
    Tuple(Arc<[Value]>, usize),
    /// A map or a set, and the sequence number of the entry last visited.
    Table(ObjectId, Option<u64>),
}

#[derive(Clone, Debug)]
enum TryPhase {
    Body,
    Catch,
    /// The `finally` block runs; this departure resumes when it completes.
    Finally(Completion),
}

#[derive(Clone, Debug)]
enum Control {
    Block {
        block: BlockId,
        next: usize,
    },
    For {
        stmt: StmtId,
        cursor: Cursor,
        started: u64,
    },
    While {
        stmt: StmtId,
        started: u64,
    },
    Try {
        stmt: StmtId,
        phase: TryPhase,
    },
}

#[derive(Debug)]
struct LibraryCall {
    lib: LibId,
    args: Vec<Value>,
}

#[derive(Debug)]
struct Frame {
    code: CodeId,
    slots: Vec<SlotState>,
    control: Vec<Control>,
    /// The statement whose action this frame waits on.
    awaiting: Option<StmtId>,
    /// The library call this frame is the body of, charged when it ends.
    library: Option<LibraryCall>,
    /// The frame was called from inside an expression and runs to its end
    /// there.
    inline: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Request(WaitId),
    Join(TaskId),
    JoinMany(u64),
}

#[derive(Debug)]
enum TaskState {
    /// Running, or in the ready queue.
    Ready,
    Waiting(Wait),
    Ended(Result<Value, Value>),
}

/// What a task's pending statement resumes with.
#[derive(Debug)]
enum Incoming {
    Value(Value),
    Raise(Value),
    Outcome(Outcome),
    /// The result of this task, which has ended.
    Join(TaskId),
}

#[derive(Debug)]
struct Task {
    identity: TaskIdentity,
    frames: Vec<Frame>,
    state: TaskState,
    incoming: Option<Incoming>,
    /// The tasks waiting on this handle alone, in the order they joined.
    joiners: Vec<TaskId>,
    /// A `join` on the handle raised the task's error.
    observed: bool,
    /// The task is or was a member of a list `join` that has returned or
    /// raised (`K-TASK-016`).
    passed: bool,
    /// The task ended in an error.
    failed: bool,
    /// How many times the task has run each `spawn`, `perform` and `sleep`.
    occurrences: BTreeMap<StmtId, u64>,
}

#[derive(Debug)]
struct ListJoin {
    joiner: TaskId,
    mode: JoinMode,
    members: Vec<TaskId>,
}

#[derive(Debug)]
struct PendingWait {
    task: TaskId,
    sleep: bool,
    handed_out: bool,
}

/// A kernel machine. It implements [`Machine`]; an embedder drives it with
/// `run` and `deliver`.
pub struct KernelMachine {
    program: Program,
    exe: Arc<Executable>,
    bounds: Bounds,
    heap: Heap,
    tasks: Vec<Task>,
    ready: VecDeque<TaskId>,
    current: Option<TaskId>,
    joins: BTreeMap<u64, ListJoin>,
    next_join: u64,
    waits: BTreeMap<WaitId, PendingWait>,
    next_wait: u64,
    /// The effects and sleeps requested since the last park.
    requests: Vec<Request>,
    /// The waits withdrawn since the last park, and every one ever.
    withdrawn: Vec<WaitId>,
    withdrawn_ever: BTreeSet<WaitId>,
    /// The session's bindings, when the run is a session cell.
    session: BTreeMap<Name, Value>,
    session_cell: bool,
    /// What the statement being run holds outside any variable.
    pins: Vec<Value>,
    fresh: Vec<ObjectId>,
    charging: bool,
    charged: u64,
    live_tasks: u32,
    inline_depth: u32,
    inline_result: Option<Result<Value, Value>>,
    end: Option<End>,
    ended: bool,
}

impl std::fmt::Debug for KernelMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelMachine")
            .field("tasks", &self.tasks.len())
            .field("charged", &self.charged)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Roots {
    seeds: Vec<Identity>,
    bytes: u64,
}

impl Roots {
    fn value(&mut self, value: &Value) {
        self.bytes = self.bytes.saturating_add(value_bytes(value));
        refs(value, &mut self.seeds);
    }

    fn object(&mut self, object: ObjectId) {
        self.seeds.push(Identity::Object(object));
    }

    fn task(&mut self, task: TaskId) {
        self.seeds.push(Identity::Task(task));
    }
}

impl KernelMachine {
    /// Starts a run whose executable is laid out as `layout` says. A test
    /// uses it to show that the layout changes nothing a run can observe;
    /// [`Machine::start`] is layout 0.
    pub fn start_with_layout(
        program: Program,
        bounds: Bounds,
        start: Start,
        layout: Layout,
    ) -> Result<Self, StartError> {
        session::start(program, bounds, start, layout)
    }

    fn task(&mut self, task: TaskId) -> Result<&mut Task, Halt> {
        self.tasks
            .get_mut(task.0 as usize)
            .ok_or_else(|| fault("a handle names no task"))
    }

    fn frame(&mut self, task: TaskId) -> Result<&mut Frame, Halt> {
        self.task(task)?
            .frames
            .last_mut()
            .ok_or_else(|| fault("a running task has no frame"))
    }

    fn charge(&mut self, units: u64) -> Result<(), Halt> {
        if self.charging {
            self.charged = self.charged.saturating_add(units);
            if self.charged > self.bounds.charge {
                return Err(bound(Bound::Charge, self.bounds.charge));
            }
        }
        Ok(())
    }

    /// Accounts `bytes` more memory, collecting first when the heap is due
    /// and ending the run when what is live does not leave room.
    fn reserve(&mut self, bytes: u64) -> Result<(), Halt> {
        if self.heap.memory.saturating_add(bytes) > self.heap.collect_at {
            self.collect();
            if self.heap.memory.saturating_add(bytes) > self.bounds.memory {
                return Err(bound(Bound::Memory, self.bounds.memory));
            }
        }
        self.heap.memory = self.heap.memory.saturating_add(bytes);
        Ok(())
    }

    /// Allocates an object and keeps it live until the statement ends.
    fn alloc(&mut self, object: Obj) -> Result<ObjectId, Halt> {
        let bytes = object_bytes(&object);
        let id = self.heap.insert(object);
        self.fresh.push(id);
        self.reserve(bytes)?;
        Ok(id)
    }

    /// Keeps a value the statement made live until the statement ends.
    fn pin(&mut self, value: &Value) -> Result<(), Halt> {
        self.pins.push(value.clone());
        self.reserve(value_bytes(value))
    }

    /// Frees every object the run cannot reach, and the result of every
    /// ended task whose handle it cannot reach, and recounts its memory.
    fn collect(&mut self) {
        let mut roots = Roots::default();
        for task in &self.tasks {
            match &task.incoming {
                Some(Incoming::Value(value) | Incoming::Raise(value)) => roots.value(value),
                Some(Incoming::Join(joined)) => roots.task(*joined),
                _ => {}
            }
            if let TaskState::Waiting(Wait::Join(joined)) = task.state {
                roots.task(joined);
            }
            for frame in &task.frames {
                roots.bytes = roots.bytes.saturating_add(FRAME_BYTES);
                for slot in &frame.slots {
                    match slot {
                        SlotState::Empty => {}
                        SlotState::Value(value) => roots.value(value),
                        SlotState::Cell(id) => roots.object(*id),
                    }
                }
                if let Some(call) = &frame.library {
                    call.args.iter().for_each(|arg| roots.value(arg));
                }
                for control in &frame.control {
                    match control {
                        Control::For {
                            cursor: Cursor::List(id, _) | Cursor::Table(id, _),
                            ..
                        } => roots.object(*id),
                        Control::For {
                            cursor: Cursor::Tuple(items, _),
                            ..
                        } => items.iter().for_each(|item| roots.value(item)),
                        Control::Try {
                            phase:
                                TryPhase::Finally(Completion::Return(value) | Completion::Throw(value)),
                            ..
                        } => roots.value(value),
                        _ => {}
                    }
                }
            }
        }
        for join in self.joins.values() {
            join.members.iter().for_each(|member| roots.task(*member));
        }
        self.session.values().for_each(|value| roots.value(value));
        self.pins.iter().for_each(|value| roots.value(value));
        if let Some(Ok(value) | Err(value)) = &self.inline_result {
            roots.value(value);
        }
        self.fresh.iter().for_each(|id| roots.object(*id));
        // An ended task's result lives as long as its handle can be
        // reached.
        let tasks = &self.tasks;
        let mut reached = BTreeSet::new();
        let mut results = 0u64;
        let live = self.heap.collect(roots.seeds, &mut |task, seeds| {
            let state = tasks.get(task.0 as usize).map(|task| &task.state);
            if let Some(TaskState::Ended(Ok(value) | Err(value))) = state
                && reached.insert(task)
            {
                results = results.saturating_add(value_bytes(value));
                refs(value, seeds);
            }
        });
        for (index, task) in self.tasks.iter_mut().enumerate() {
            if matches!(task.state, TaskState::Ended(_)) && !reached.contains(&TaskId(index as u64))
            {
                task.state = TaskState::Ended(Ok(Value::Null));
            }
        }
        let bytes = roots.bytes.saturating_add(results).saturating_add(live);
        self.heap.settle(bytes, self.bounds.memory);
    }

    /// Runs one statement of `task`, or resumes the one it waits in.
    fn step(&mut self, task: TaskId, host: &mut dyn Host) -> Result<(), Halt> {
        self.pins.clear();
        self.fresh.clear();
        let exe = Arc::clone(&self.exe);
        let outcome = match self.task(task)?.incoming.take() {
            Some(incoming) => {
                self.refresh_charging(task, &exe);
                match self.resume(task, &exe, incoming) {
                    Ok(value) => {
                        // The value is in no variable until the statement
                        // binds it.
                        self.pin(&value)?;
                        self.finish_action(task, host, &exe, value)
                            .map(|()| Completion::Normal)
                    }
                    Err(interrupt) => Err(interrupt),
                }
            }
            None => self.advance(task, host, &exe),
        };
        self.settle(task, host, &exe, outcome)
    }

    fn refresh_charging(&mut self, task: TaskId, exe: &Executable) {
        self.charging = self
            .tasks
            .get(task.0 as usize)
            .and_then(|task| task.frames.last())
            .is_none_or(|frame| exe.codes[frame.code.0 as usize].charged);
    }

    fn settle(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        outcome: Eval<Completion>,
    ) -> Result<(), Halt> {
        let completion = match outcome {
            Ok(completion) => completion,
            Err(Interrupt::Raise(value)) => Completion::Throw(value),
            Err(Interrupt::Halt(halt)) => return Err(halt),
        };
        self.complete(task, host, exe, completion)
    }

    /// Marks a task ended, wakes what waits on it and, for `main`, ends
    /// the run (`K-TASK-008`, `K-TASK-018`).
    fn end_task(&mut self, task: TaskId, result: Result<Value, Value>) -> Result<(), Halt> {
        let ended = self.task(task)?;
        ended.frames.clear();
        ended.failed = result.is_err();
        ended.state = TaskState::Ended(result.clone());
        let joiners = std::mem::take(&mut ended.joiners);
        self.live_tasks = self.live_tasks.saturating_sub(1);
        if self.current == Some(task) {
            self.current = None;
        }
        if task == TaskId::MAIN {
            self.end = Some(match result {
                Ok(value) => self.conclude(None, Ending::Returned(value)),
                Err(value) => End::Error(RunError::Uncaught(
                    copy_out(&self.heap, &value).unwrap_or_else(Raised::into_datum),
                )),
            });
            return Ok(());
        }
        for joiner in joiners {
            self.wake(joiner, Incoming::Join(task))?;
        }
        let deciding: Vec<u64> = self
            .joins
            .iter()
            .filter(|(_, join)| join.members.contains(&task))
            .map(|(order, _)| *order)
            .collect();
        for order in deciding {
            let Some(join) = self.joins.get(&order) else {
                continue;
            };
            let (mode, members, joiner) = (join.mode, join.members.clone(), join.joiner);
            if let Some(decision) = self.decide(mode, &members, Some(task))? {
                self.joins.remove(&order);
                self.pass(&members)?;
                self.wake(
                    joiner,
                    match decision {
                        Ok(value) => Incoming::Value(value),
                        Err(value) => Incoming::Raise(value),
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Makes a waiting task ready, at the back of the queue (`K-TASK-005`).
    fn wake(&mut self, task: TaskId, incoming: Incoming) -> Result<(), Halt> {
        let woken = self.task(task)?;
        woken.state = TaskState::Ready;
        woken.incoming = Some(incoming);
        self.ready.push_back(task);
        Ok(())
    }

    /// Examines the other tasks at the run's end and gives the end
    /// (`K-TASK-018`). `ender` is the task that ran `finish` or `fail`.
    fn conclude(&mut self, ender: Option<TaskId>, ending: Ending) -> End {
        let mut unfinished = Vec::new();
        let mut unobserved = Vec::new();
        for (index, task) in self.tasks.iter().enumerate().skip(1) {
            if ender == Some(TaskId(index as u64)) || task.passed {
                continue;
            }
            match &task.state {
                TaskState::Ended(_) if task.failed && !task.observed => {
                    unobserved.push(task.identity.clone());
                }
                TaskState::Ended(_) => {}
                _ => unfinished.push(task.identity.clone()),
            }
        }
        if !unfinished.is_empty() || !unobserved.is_empty() {
            return End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            });
        }
        match ending {
            Ending::Returned(value) => match copy_out(&self.heap, &value) {
                Ok(result) => End::Finished(self.finished(result)),
                Err(raised) => End::Error(RunError::Uncaught(raised.into_datum())),
            },
            Ending::Finished(result) => End::Finished(self.finished(result)),
            Ending::Failed(reason) => End::Failed(reason),
        }
    }

    /// Hands the embedder what the tasks asked for, or ends a run nothing
    /// can wake (`K-TASK-023`, `K-TASK-024`).
    fn park(&mut self, host: &mut dyn Host) -> Step {
        if self.waits.is_empty() {
            let waiting = self
                .tasks
                .iter()
                .filter(|task| !matches!(task.state, TaskState::Ended(_)))
                .map(|task| task.identity.clone())
                .collect();
            return self.finish_with(End::Error(RunError::Deadlock { waiting }));
        }
        if host.cancel_requested() {
            return self.finish_with(End::Cancelled);
        }
        for request in &self.requests {
            let wait = match request {
                Request::Effect(effect) => effect.wait,
                Request::Sleep(sleep) => sleep.wait,
            };
            if let Some(pending) = self.waits.get_mut(&wait) {
                pending.handed_out = true;
            }
        }
        Step::Parked(Park {
            requests: std::mem::take(&mut self.requests),
            withdrawn: std::mem::take(&mut self.withdrawn),
        })
    }

    fn finish_with(&mut self, end: End) -> Step {
        self.ended = true;
        Step::Ended(end)
    }
}

impl Machine for KernelMachine {
    type Parked = Unparked;

    fn start(program: Program, bounds: Bounds, start: Start) -> Result<Self, StartError> {
        Self::start_with_layout(program, bounds, start, Layout::default())
    }

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        let before = self.charged;
        loop {
            let task = match self.current {
                Some(task) => task,
                None => match self.ready.pop_front() {
                    Some(task) => {
                        self.current = Some(task);
                        task
                    }
                    None => return Ok(self.park(host)),
                },
            };
            match self.step(task, host) {
                Ok(()) => {}
                Err(Halt::Fault(problem)) => {
                    self.ended = true;
                    return Err(MachineError::Fault { problem });
                }
                Err(Halt::Bound(exceeded)) => {
                    return Ok(self.finish_with(End::Error(RunError::Bound(exceeded))));
                }
                Err(Halt::Finish(result)) => {
                    let end = self.conclude(Some(task), Ending::Finished(result));
                    return Ok(self.finish_with(end));
                }
                Err(Halt::Fail(reason)) => {
                    let end = self.conclude(Some(task), Ending::Failed(reason));
                    return Ok(self.finish_with(end));
                }
            }
            if let Some(end) = self.end.take() {
                return Ok(self.finish_with(end));
            }
            let more = self.current.is_some() || !self.ready.is_empty();
            if more && self.charged.saturating_sub(before) >= slice {
                if host.cancel_requested() {
                    return Ok(self.finish_with(End::Cancelled));
                }
                return Ok(Step::Slice);
            }
        }
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.ended {
            return Err(DeliverError::Ended);
        }
        let Some(pending) = self.waits.get(&wait) else {
            return if self.withdrawn_ever.contains(&wait) {
                Ok(Delivered::Dropped)
            } else if wait.0 < self.next_wait {
                Err(DeliverError::AlreadyDelivered { wait })
            } else {
                Err(DeliverError::UnknownWait { wait })
            };
        };
        if !pending.handed_out {
            return Err(DeliverError::UnknownWait { wait });
        }
        if pending.sleep != matches!(outcome, Outcome::Elapsed) {
            return Err(DeliverError::WrongOutcome { wait });
        }
        let task = pending.task;
        self.waits.remove(&wait);
        if let Some(woken) = self.tasks.get_mut(task.0 as usize) {
            woken.state = TaskState::Ready;
            woken.incoming = Some(Incoming::Outcome(outcome));
            self.ready.push_back(task);
        }
        Ok(Delivered::Accepted)
    }

    fn export(&mut self) -> Result<Self::Parked, ExportError> {
        if self.ended {
            return Err(ExportError::Ended);
        }
        Err(ExportError::NoEncoding)
    }

    fn import(_: Program, _: Bounds, parked: Self::Parked) -> Result<Self, ImportError> {
        match parked {}
    }

    fn meters(&self) -> Meters {
        Meters {
            charged: self.charged,
            memory: self.heap.memory,
            live_tasks: self.live_tasks,
        }
    }
}
