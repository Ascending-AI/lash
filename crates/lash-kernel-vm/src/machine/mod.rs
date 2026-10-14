//! The kernel machine: one run of one document as a set of tasks.
//!
//! Each task is a stack of frames, and each frame a stack of the blocks,
//! loops and `try` statements it is inside. A task pauses only at an
//! action, between statements (`K-STMT-004`), so its whole state is that
//! stack and the variables: no value lives anywhere else while it waits.

mod actions;
mod checkpoint;
mod exec;
mod frames;
mod observing;
mod ops;
mod queue;
mod session;
mod steps;
mod stream;
mod tasks;

use std::sync::Arc;

use lash_kernel_doc::{Datum, DocumentId, EntityId, Name, TaskId, Value};

use crate::Layout;
use crate::compile::{BlockId, CodeId, Executable, LibId, StmtId};
use crate::data::{Raised, copy_out};
use crate::heap::value_bytes;
use crate::interface::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, End, Host, Machine, MachineError,
    Meters, Outcome, Park, Program, RunError, StartError, Step, WaitId,
};
use crate::observe::{Event, GuestPhase, Observer};
use lash_kernel_doc::heap::{ErrorRef, HeapFault, LogicalBytes, PageId, TupleRef};
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::{Access, Tree};
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, EndedTask, FrameRecord, HeapLifecycle, HeapWriteError,
    IncomingRecord as Incoming, JoinerRecord as Joiner, ListJoinRecord as ListJoin, PagedHeap,
    SemanticHeap, TableCursor, TaskStateRecord as TaskState, WaitPhase, WaitRecord,
};
use session::{keyed, open_tree};

/// What a frame is accounted before its variables.
const FRAME_BYTES: u64 = 64;
/// What ends a run from inside a statement.
#[derive(Debug)]
pub(crate) enum Halt {
    Bound(BoundExceeded),
    Finish(Datum),
    Fail(Datum),
    /// The machine's own state is inconsistent.
    Fault(String),
    /// The host failed a read-bound `perform` (`K-EFF-012`).
    Host(crate::interface::HostFault),
    Heap(HeapFault),
    Memory,
}

impl Halt {
    /// Preserve the innermost library definition that caused a bound.
    fn in_function(mut self, function: &lash_kernel_doc::FunctionName) -> Self {
        if let Self::Bound(exceeded) = &mut self {
            exceeded.function.get_or_insert_with(|| function.clone());
        }
        self
    }
}

/// Why a statement did not complete.
#[derive(Debug)]
pub(crate) enum Interrupt {
    Raise(Box<Value>),
    Halt(Halt),
}

impl Interrupt {
    fn raised(value: Value) -> Self {
        Self::Raise(Box::new(value))
    }
    fn in_function(self, function: &lash_kernel_doc::FunctionName) -> Self {
        match self {
            Self::Halt(halt) => Self::Halt(halt.in_function(function)),
            other => other,
        }
    }
}

type Eval<T> = Result<T, Interrupt>;

impl From<Halt> for Interrupt {
    fn from(halt: Halt) -> Self {
        Self::Halt(halt)
    }
}

impl From<Raised> for Interrupt {
    fn from(raised: Raised) -> Self {
        Self::raised(error(raised.kind, raised.message))
    }
}

pub(crate) fn error(kind: &str, message: impl Into<String>) -> Value {
    Value::Error(Arc::new(ErrorRef::new(kind, &message.into())))
}

impl From<HeapFault> for Halt {
    fn from(error: HeapFault) -> Self {
        Self::Heap(error)
    }
}
impl From<HeapFault> for Interrupt {
    fn from(error: HeapFault) -> Self {
        Self::Halt(error.into())
    }
}
impl From<HeapWriteError> for Interrupt {
    fn from(error: HeapWriteError) -> Self {
        match error {
            HeapWriteError::Raised { kind, message } => {
                Self::raised(super::machine::error(kind, message))
            }
            HeapWriteError::Memory => Self::Halt(Halt::Memory),
            HeapWriteError::Fault(error) => Self::Halt(Halt::Heap(error)),
        }
    }
}
impl From<lash_kernel_doc::NativeError> for Interrupt {
    fn from(error: lash_kernel_doc::NativeError) -> Self {
        use lash_kernel_doc::NativeError;
        match error {
            NativeError::Raised(error) => Self::raised(Value::Error(error)),
            NativeError::Memory => Self::Halt(Halt::Memory),
            NativeError::Heap(error) => Self::Halt(Halt::Heap(error)),
            NativeError::Guard(error) => Self::Halt(Halt::Fault(format!(
                "an immutable constructor returned a guard: {error}"
            ))),
        }
    }
}
impl From<crate::data::CopyError> for Interrupt {
    fn from(error: crate::data::CopyError) -> Self {
        match error {
            crate::data::CopyError::Raised(error) => error.into(),
            crate::data::CopyError::Heap(error) => error.into(),
        }
    }
}

fn raise<T>(kind: &'static str, message: impl Into<String>) -> Eval<T> {
    Err(Interrupt::raised(error(kind, message)))
}

fn fault(problem: &str) -> Halt {
    Halt::Fault(problem.to_string())
}

fn bound(bound: Bound, limit: u64) -> Halt {
    Halt::Bound(BoundExceeded {
        bound,
        limit,
        function: None,
    })
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
    Cell(EntityId),
}

/// A loop's position in what it iterates (`K-ITER-002`, `K-ITER-003`).
#[derive(Clone, Debug)]
enum Cursor {
    List(EntityId, usize),
    Tuple(TupleRef, usize),
    /// A map or a set, and the sequence number of the entry last visited.
    Table(Value, Option<TableCursor>),
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

struct LibraryCall {
    lib: LibId,
    args: Tree<Value>,
}

/// The vectors of a frame that has ended, kept for the next frame.
#[derive(Debug, Default)]
struct FrameStorage {
    control: Vec<Control>,
}

/// Storage the machine reuses from call to call, so that a call allocates
/// nothing of its own.
#[derive(Debug, Default)]
struct Storage {
    frames: Vec<FrameStorage>,
}

struct Frame {
    code: CodeId,
    record: FrameRecord,
    access: Access<'static>,
    exe: Arc<Executable>,
    control: Vec<Control>,
    /// The statement whose action this frame waits on.
    awaiting: Option<StmtId>,
    /// The library call this frame is the body of, charged when it ends.
    library: Option<LibraryCall>,
    /// The frame was called from inside an expression and runs to its end
    /// there.
    inline: bool,
}

impl Frame {
    fn slot(&self, at: u32) -> Result<SlotState, HeapFault> {
        let code = self.exe.code(self.code);
        let info = &code.slots[code.slots_by_at[at as usize] as usize];
        let key = &info.key;
        Ok(match self.record.bindings.get(key, &self.access)? {
            None => SlotState::Empty,
            Some(BindingRecord {
                value: BoundRecord::Value(value),
                ..
            }) => SlotState::Value(value),
            Some(BindingRecord {
                value: BoundRecord::Shared(id),
                ..
            }) => SlotState::Cell(id),
        })
    }
    fn set_slot(&mut self, at: u32, state: SlotState) -> Result<(), HeapFault> {
        let code = self.exe.code(self.code);
        let info = &code.slots[code.slots_by_at[at as usize] as usize];
        let key = &info.key;
        let value = match state {
            SlotState::Empty => {
                self.record.bindings.remove(key, &self.access)?;
                return Ok(());
            }
            SlotState::Value(value) => BoundRecord::Value(value),
            SlotState::Cell(id) => BoundRecord::Shared(id),
        };
        let (name, declared) = match self.record.bindings.get(key, &self.access)? {
            Some(old) => (old.name, old.declared),
            None => (
                lash_kernel_state::paged::NameRef::new(&info.name, &self.access)?,
                lash_kernel_state::paged::SiteRef::new(&info.declared, &self.access)?,
            ),
        };
        self.record.bindings.write(
            key.clone(),
            BindingRecord {
                name,
                declared,
                value,
            },
            &self.access,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Request(WaitId),
    Join(TaskId),
    JoinMany(u64),
}

struct Task {
    record: lash_kernel_state::paged::TaskRecord,
    frames: frames::Frames,
}

/// A kernel machine. It implements [`Machine`]; an embedder drives it with
/// `run` and `deliver`.
pub struct KernelMachine {
    program: Program,
    /// The document's identity, which a parked run pins, once it has been
    /// taken.
    document: Option<DocumentId>,
    exe: Arc<Executable>,
    bounds: Bounds,
    heap: PagedHeap,
    limits: lash_kernel_doc::KernelDataLimits,
    release: PageId,
    owner: PageId,
    tasks: tasks::Tasks,
    ready: queue::Queue<TaskId>,
    current: Option<TaskId>,
    joins: Tree<ListJoin>,
    next_join: u64,
    waits: Tree<WaitRecord>,
    next_wait: u64,
    /// The effects and sleeps requested since the last park, in request
    /// order.
    // Issued requests are retained in waits until reporting.
    /// The waits withdrawn since the last park, and every one ever.
    withdrawn: queue::Queue<u64>,
    withdrawn_ever: Tree<u64>,
    /// The mode each effect is performed in, bound at the start and saved
    /// with the run (`K-EFF-012`).
    modes: Tree<lash_kernel_state::paged::NameRef>,
    /// The session's bindings, when the run is a session cell.
    functions: lash_kernel_state::paged::KeyedRoot,
    session_cell: bool,
    /// What the statement being run holds outside any variable.
    pins: Pins,
    fresh: Vec<EntityId>,
    charging: bool,
    charged: u64,
    /// The cost table of the kernel version the document states.
    costs: crate::costs::Costs,
    live_tasks: u32,
    census_refinements: u64,
    inline_depth: u32,
    inline_result: Option<Result<Value, Value>>,
    /// The registers: the values of the expressions being evaluated and
    /// the arguments of the calls being made, in the windows of the
    /// statements running, innermost last. No step reads one an earlier
    /// step wrote, and a slice's end clears them.
    regs: Vec<Value>,
    /// Where the window of the next statement to run starts: 0, or above
    /// the registers of the expression an inline body runs inside.
    top: usize,
    storage: Storage,
    steps: steps::StepCheck,
    end: Option<End>,
    ended: bool,
    /// What observes the run, when anything does: never part of its state.
    observer: Option<Box<Observer>>,
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

/// What the statement being run holds outside any variable: each value
/// that names an object or a task, which a collection keeps, and the bytes
/// of the values that name nothing, which it only counts.
#[derive(Default)]
struct Pins {
    values: Vec<Value>,
    bytes: u64,
}

impl Pins {
    /// Holds `value` until the statement ends, and gives the bytes to
    /// reserve for it.
    #[inline(always)]
    fn pin(&mut self, value: &Value) -> u64 {
        let bytes = value_bytes(value);
        self.hold(value, bytes);
        bytes
    }

    /// Holds `value`, of `bytes` bytes.
    fn hold(&mut self, value: &Value, bytes: u64) {
        self.values.push(value.clone());
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn clear(&mut self) {
        self.values.clear();
        self.bytes = 0;
    }
}

impl KernelMachine {
    #[cfg(test)]
    pub(crate) fn accounting(&self) -> lash_kernel_state::paged::AccountingCertificate {
        self.heap.certificate()
    }
    #[cfg(test)]
    pub(crate) fn wait_count(&self) -> u64 {
        self.waits.len()
    }
    fn heap_halt(&self, error: HeapWriteError) -> Halt {
        match error {
            HeapWriteError::Memory => bound(Bound::Memory, self.bounds.memory),
            HeapWriteError::Fault(error) => Halt::Heap(error),
            HeapWriteError::Raised { kind, message } => {
                Halt::Fault(format!("an allocation returned {kind}: {message}"))
            }
        }
    }
}

impl KernelMachine {
    /// Has the run check, at the end of every statement's step, the step
    /// structure the executable states (the blocks the step leaves and the
    /// Boundary it ends at) against the one its frame shows, derived as
    /// the machine once derived it at run time. A run where the two
    /// disagree ends with a fault; the check changes nothing else a run
    /// does. Laws use it.
    pub fn verify_step_structure(&mut self) {
        self.steps.verify = true;
    }

    /// Checks the ops of every statement of the document the run executes
    /// (`lash_kernel_exec::verify_stream`): each range is well formed and
    /// spans no Boundary. Gives how many statements it checked; the
    /// library's are [`crate::PreparedLibrary::verify_stream`]'s.
    pub fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError> {
        self.exe.verify_stream()
    }

    /// How many statement ends the step-structure check has passed.
    pub fn verified_steps(&self) -> u64 {
        self.steps.verified
    }

    /// Binds `modes` to a run that has executed nothing: each read names an
    /// effect the document's manifest declares.
    fn bind_modes(&mut self, modes: crate::interface::EffectModes) -> Result<(), StartError> {
        for effect in modes.reads() {
            if !self.program.document.manifest.effects.contains_key(effect) {
                return Err(StartError::UndeclaredRead {
                    effect: effect.clone(),
                });
            }
            let name = Name::new(effect.as_str());
            self.modes.write(
                lash_kernel_state::paged::metadata::metadata_key(&name),
                lash_kernel_state::paged::NameRef::new(&name, &self.heap.access)?,
                &self.heap.access,
            )?;
        }
        Ok(())
    }
    fn effect_mode(
        &self,
        effect: &lash_kernel_doc::EffectName,
    ) -> Result<crate::interface::EffectMode, HeapFault> {
        let name = Name::new(effect.as_str());
        Ok(
            if self
                .modes
                .get(
                    &lash_kernel_state::paged::metadata::metadata_key(&name),
                    &self.heap.access,
                )?
                .is_some()
            {
                crate::interface::EffectMode::Read
            } else {
                crate::interface::EffectMode::Recorded
            },
        )
    }

    fn task(&mut self, task: TaskId) -> Result<&mut Task, Halt> {
        self.reconcile_sweep()?;
        self.tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("a handle names no task"))
    }

    fn frame(&mut self, task: TaskId) -> Result<&mut Frame, Halt> {
        self.task(task)?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("a running task has no frame"))
    }

    fn reconcile_sweep(&mut self) -> Result<(), Halt> {
        let refinements = self.heap.certificate().refinements;
        if refinements != self.census_refinements {
            self.tasks.after_refinement(self.heap.tasks.clone())?;
            self.census_refinements = refinements;
        }
        Ok(())
    }
    fn publish_roots(&mut self) -> Result<(), Halt> {
        self.reconcile_sweep()?;
        let tasks = self.tasks.roots()?;
        self.heap
            .set_execution_roots(
                tasks,
                self.joins.clone(),
                &self.regs,
                &self.pins.values,
                &self.fresh,
            )
            .map_err(|e| self.heap_halt(e))?;
        self.reconcile_sweep()
    }

    /// Runs one statement of `task`, or resumes the one it waits in.
    fn step(&mut self, task: TaskId, host: &mut dyn Host, exe: &Executable) -> Result<(), Halt> {
        if self.observer.is_some() {
            self.observe_step(task, exe);
        }
        self.pins.clear();
        self.fresh.clear();
        let outcome = match self.task(task)?.take_incoming() {
            Some(incoming) => {
                self.refresh_charging(task, exe)?;
                self.resume(task, exe, incoming).and_then(|value| {
                    // The value is in no variable until the statement binds
                    // it.
                    self.put(0, value);
                    self.pin(0)?;
                    let stmt = self.finish_action(task, host, exe, 0)?;
                    self.take_edge_of(task, exe, stmt)?;
                    Ok(Completion::Normal)
                })
            }
            None => self.advance(task, host, exe),
        };
        self.settle(task, host, exe, outcome)
    }

    fn refresh_charging(&mut self, task: TaskId, exe: &Executable) -> Result<(), Halt> {
        self.charging = self
            .task(task)?
            .frames
            .last_mut()?
            .is_none_or(|frame| exe.code(frame.code).charged);
        Ok(())
    }

    fn settle(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        outcome: Eval<Completion>,
    ) -> Result<(), Halt> {
        let completion = match outcome {
            // Most statements complete normally: nothing departs.
            Ok(Completion::Normal) => return Ok(()),
            Ok(completion) => completion,
            Err(Interrupt::Raise(value)) => Completion::Throw(*value),
            Err(Interrupt::Halt(halt)) => return Err(halt),
        };
        self.complete(task, host, exe, completion)
    }

    /// Marks a task ended, wakes what waits on it and, for `main`, ends
    /// the run (`K-TASK-008`, `K-TASK-018`).
    fn end_task(&mut self, task: TaskId, result: Result<Value, Value>) -> Result<(), Halt> {
        let result = match result {
            Ok(value) => Ok(self
                .heap
                .canonical_value(value)
                .map_err(|error| self.heap_halt(error))?),
            Err(value) => Err(self
                .heap
                .canonical_value(value)
                .map_err(|error| self.heap_halt(error))?),
        };
        self.observe(Event::TaskEnded(task));
        let ended = self.task(task)?;
        ended.frames.clear();
        ended.failed = result.is_err();
        ended.state = TaskState::Ended(match result.clone() {
            Ok(value) => EndedTask::Returned(value),
            Err(value) => EndedTask::Raised(value),
        });
        let joiners = std::mem::replace(&mut ended.joiners, Tree::new(true));
        let access = self.heap.access.clone();
        self.live_tasks = self.live_tasks.saturating_sub(1);
        if self.current == Some(task) {
            self.current = None;
        }
        if task == TaskId::MAIN {
            self.end = Some(match result {
                Ok(value) => self.conclude(None, Ending::Returned(value))?,
                Err(value) => End::Error(RunError::Uncaught(match copy_out(&self.heap, &value) {
                    Ok(value) => value,
                    Err(crate::data::CopyError::Raised(error)) => error.into_datum(),
                    Err(crate::data::CopyError::Heap(error)) => return Err(error.into()),
                })),
            });
            return Ok(());
        }
        for at in 0..joiners.len() {
            let (_, joiner) = joiners
                .rank_get(at, &access)?
                .ok_or_else(|| fault("a joiner row is missing"))?;
            match joiner {
                Joiner::Single(joiner) => self.wake(joiner, Incoming::Joined(task))?,
                Joiner::List(order) => {
                    let Some(join) = self.joins.get(
                        &lash_kernel_state::paged::directory::ordinal_key(order),
                        &access,
                    )?
                    else {
                        continue;
                    };
                    let (mode, members, joiner) = (join.mode, join.members.clone(), join.joiner);
                    if let Some(decision) = self.decide(mode, &members, Some(task))? {
                        self.remove_list_join(order)?;
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
            }
        }
        Ok(())
    }

    /// Makes a waiting task ready, at the back of the queue (`K-TASK-005`).
    fn wake(&mut self, task: TaskId, incoming: Incoming) -> Result<(), Halt> {
        let woken = self.task(task)?;
        woken.wake(incoming);
        self.ready.push_back(task)?;
        Ok(())
    }

    /// Examines the other tasks at the run's end and gives the end
    /// (`K-TASK-018`). `ender` is the task that ran `finish` or `fail`.
    fn conclude(&mut self, ender: Option<TaskId>, ending: Ending) -> Result<End, Halt> {
        let mut unfinished = Vec::new();
        let mut unobserved = Vec::new();
        let access = self.heap.access.clone();
        self.tasks.visit(&mut |id, task| {
            if id != TaskId::MAIN && ender != Some(id) && !task.passed {
                match task.state {
                    TaskState::Ended(_) if task.failed && !task.observed => {
                        unobserved.push(task.identity)
                    }
                    TaskState::Ended(_) => (),
                    _ => unfinished.push(task.identity),
                }
            }
            true
        })?;
        if !unfinished.is_empty() || !unobserved.is_empty() {
            let unfinished = unfinished
                .into_iter()
                .map(|id| id.resolve(&access))
                .collect::<Result<Vec<_>, _>>()?;
            let unobserved = unobserved
                .into_iter()
                .map(|id| id.resolve(&access))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            }));
        }
        Ok(match ending {
            Ending::Returned(value) => match copy_out(&self.heap, &value) {
                Ok(result) => End::Finished(Box::new(self.finished(result, false)?)),
                Err(crate::data::CopyError::Raised(raised)) => {
                    End::Error(RunError::Uncaught(raised.into_datum()))
                }
                Err(crate::data::CopyError::Heap(fault)) => return Err(fault.into()),
            },
            Ending::Finished(result) => End::Finished(Box::new(self.finished(result, true)?)),
            Ending::Failed(reason) => End::Failed(reason),
        })
    }

    fn park(&mut self, host: &mut dyn Host) -> Result<Step, MachineError> {
        if host.cancel_requested() {
            return Ok(self.finish_with(End::Cancelled));
        }
        if self.waits.is_empty() {
            let mut waiting = Vec::new();
            self.tasks
                .visit(&mut |_, task| {
                    if task.is_live() {
                        waiting.push(task.identity);
                    }
                    true
                })
                .map_err(MachineError::Heap)?;
            let waiting = waiting
                .into_iter()
                .map(|id| id.resolve(&self.heap.access))
                .collect::<Result<Vec<_>, _>>()
                .map_err(MachineError::Heap)?;
            return Ok(self.finish_with(End::Error(RunError::Deadlock { waiting })));
        }
        let mut requests = Vec::new();
        // Each report changes Issued to Admitted. Sealing alone changes neither.
        let access = self.heap.access.clone();
        let mut key = None;
        while let Some((next, mut wait)) = self
            .waits
            .after(key.as_deref(), &access)
            .map_err(MachineError::Heap)?
        {
            if let WaitPhase::Issued { request } = wait.phase {
                let raw: [u8; 8] = next
                    .as_slice()
                    .try_into()
                    .map_err(|_| MachineError::Fault {
                        problem: "wait key width".into(),
                    })?;
                let id = WaitId(u64::from_be_bytes(raw));
                requests.push(
                    self.report_request(id, wait.task, &request)
                        .map_err(MachineError::Heap)?,
                );
                wait.phase = WaitPhase::Admitted;
                self.waits
                    .write(next.clone(), wait, &access)
                    .map_err(MachineError::Heap)?;
            }
            key = Some(next);
        }
        let mut withdrawn = Vec::new();
        while let Some(wait) = self.withdrawn.pop_front().map_err(MachineError::Heap)? {
            withdrawn.push(WaitId(wait));
        }
        let (tasks, outstanding) = self.tasks.sealed_root().map_err(MachineError::Heap)?;
        let live = u64::from(self.live_tasks.saturating_sub(1));
        Ok(Step::Parked(Park {
            requests,
            withdrawn,
            outstanding,
            live,
            tasks,
        }))
    }

    fn finish_with(&mut self, end: End) -> Step {
        self.ended = true;
        Step::Ended(end)
    }

    /// Runs tasks until none is ready, the run ends, or `slice` units of
    /// charge have passed (`K-MACH-005`).
    fn run_slice(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        let before = self.charged;
        // The executable is the run's for its whole life.
        let exe = Arc::clone(&self.exe);
        loop {
            let task = match self.current {
                Some(task) => task,
                None => match self.ready.pop_front().map_err(MachineError::Heap)? {
                    Some(task) => {
                        self.current = Some(task);
                        task
                    }
                    None => return self.park(host),
                },
            };
            match self.step(task, host, &exe) {
                Ok(()) => {}
                Err(Halt::Fault(problem)) => {
                    self.ended = true;
                    return Err(MachineError::Fault { problem });
                }
                Err(Halt::Host(fault)) => {
                    self.ended = true;
                    return Err(MachineError::Host(fault));
                }
                Err(Halt::Bound(mut exceeded)) => {
                    if exceeded.function.is_none()
                        && let Some(lib) = self
                            .frame(task)
                            .ok()
                            .and_then(|frame| frame.library.as_ref().map(|call| call.lib))
                    {
                        exceeded.function = Some(self.exe.lib(lib).definition.name.clone());
                    }
                    return Ok(self.finish_with(End::Error(RunError::Bound(exceeded))));
                }
                Err(Halt::Heap(error)) => return Err(MachineError::Heap(error)),
                Err(Halt::Memory) => {
                    return Ok(self.finish_with(End::Error(RunError::Bound(BoundExceeded {
                        bound: Bound::Memory,
                        limit: self.bounds.memory,
                        function: None,
                    }))));
                }
                Err(Halt::Finish(result)) => {
                    let end = self
                        .conclude(Some(task), Ending::Finished(result))
                        .map_err(machine_error)?;
                    return Ok(self.finish_with(end));
                }
                Err(Halt::Fail(reason)) => {
                    let end = self
                        .conclude(Some(task), Ending::Failed(reason))
                        .map_err(machine_error)?;
                    return Ok(self.finish_with(end));
                }
            }
            self.publish_roots().map_err(machine_error)?;
            self.regs.clear();
            self.pins.clear();
            self.fresh.clear();
            self.heap
                .release_statement_roots()
                .map_err(MachineError::Heap)?;
            self.tasks.evict().map_err(MachineError::Heap)?;
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
}

fn machine_error(halt: Halt) -> MachineError {
    match halt {
        Halt::Heap(error) => MachineError::Heap(error),
        Halt::Host(error) => MachineError::Host(error),
        other => MachineError::Fault {
            problem: format!("{other:?}"),
        },
    }
}

#[cfg(test)]
mod park_laws;
