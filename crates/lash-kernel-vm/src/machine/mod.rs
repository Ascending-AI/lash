//! The kernel machine: one run of one document as a set of tasks.
//!
//! Each task is a stack of frames, and each frame a stack of the blocks,
//! loops and `try` statements it is inside. A frame stands between
//! statements, or inside a statement at an action nested in its
//! expression: a call whose callee's frame is above it, a wait, or a
//! `spawn`. There it holds the operands its statement has evaluated as the
//! roles of its continuation (`K-EVAL-008`), so a task's whole state is
//! that stack and the variables: no value lives in a register past its
//! step.

mod actions;
mod checkpoint;
mod exec;
mod frames;
mod issued;
mod loops;
mod native_heap;
mod observing;
mod operation_entry;
mod ops;
#[cfg(test)]
mod park_laws;
mod queue;
mod registers;
mod session;
mod steps;
mod stream;
mod tasks;
mod versions;

use std::sync::Arc;

use lash_kernel_doc::{Datum, DocumentId, EntityId, Name, TaskId, Value};

use crate::compile::Layout;
use crate::compile::{BlockId, CodeId, Executable, LibId, StmtId};
use crate::data::{Raised, copy_out};
use crate::heap::value_bytes;
use crate::interface::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, End, Host, Machine, MachineError,
    Meters, Outcome, Park, Program, RunError, StartError, Step, WaitId,
};
use crate::observe::{Event, GuestPhase, Observer};
use lash_kernel_doc::heap::{ErrorRef, HeapFault, PageId};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::tree::{Access, Tree};
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, EndedTask, FrameRecord, HeapLifecycle, HeapWriteError,
    IncomingRecord as Incoming, JoinerRecord as Joiner, ListJoinRecord as ListJoin, SemanticHeap,
    TaskStateRecord as TaskState, WaitPhase, WaitRecord,
};
use loops::Cursor;
use session::{keyed, open_tree};

/// What a frame is accounted before its variables.
const FRAME_BYTES: u64 = 64;
/// What ends a run from inside a statement.
/// Rare payloads are boxed so successful frame lookups carry a small result.
#[derive(Debug)]
pub(crate) enum Halt {
    Bound(Box<BoundExceeded>),
    Finish(Box<Datum>),
    Fail(Box<Datum>),
    /// The machine's own state is inconsistent.
    Fault(String),
    /// The host failed a read-bound `perform` (`K-EFF-012`).
    Host(crate::interface::HostFault),
    Heap(Box<HeapFault>),
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
    Halt(Box<Halt>),
}

impl Interrupt {
    fn raised(value: Value) -> Self {
        Self::Raise(Box::new(value))
    }
    fn in_function(self, function: &lash_kernel_doc::FunctionName) -> Self {
        match self {
            Self::Halt(halt) => Self::from((*halt).in_function(function)),
            other => other,
        }
    }
}

type Eval<T> = Result<T, Interrupt>;

impl From<Halt> for Interrupt {
    fn from(halt: Halt) -> Self {
        Self::Halt(Box::new(halt))
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
        Self::Heap(Box::new(error))
    }
}
impl From<HeapFault> for Interrupt {
    fn from(error: HeapFault) -> Self {
        Self::from(Halt::from(error))
    }
}
impl From<HeapWriteError> for Interrupt {
    fn from(error: HeapWriteError) -> Self {
        match error {
            HeapWriteError::Raised { kind, message } => {
                Self::raised(super::machine::error(kind, message))
            }
            other => Self::from(write_halt(other)),
        }
    }
}
impl From<lash_kernel_doc::NativeError> for Interrupt {
    fn from(error: lash_kernel_doc::NativeError) -> Self {
        use lash_kernel_doc::NativeError;
        match error {
            NativeError::Raised(error) => Self::raised(Value::Error(error)),
            NativeError::Memory => Self::from(Halt::Memory),
            NativeError::Heap(error) => Self::from(Halt::from(error)),
            NativeError::Guard(error) => Self::from(Halt::Fault(format!(
                "an immutable constructor returned a guard: {error}"
            ))),
            NativeError::Decline => {
                Self::from(Halt::Fault("an immutable constructor declined".into()))
            }
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
    Halt::Bound(Box::new(BoundExceeded {
        bound,
        limit,
        function: None,
    }))
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
    Return(Box<Value>),
    ReturnVia { value: Box<Value>, stmt: StmtId },
    Throw(Box<Value>),
}

#[derive(Clone, Debug)]
enum SlotState {
    Empty,
    Value(Value),
    /// A variable a closure shares: the heap cell that holds it.
    Cell(EntityId),
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
        header: loops::Header,
        guard: Option<Box<loops::Guard>>,
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

/// Activation storage and immutable entry references belong to this page
/// access, rather than the executable shared by independent machines.
#[derive(Default)]
struct Storage {
    #[expect(clippy::vec_box, reason = "reuse the frame allocation across calls")]
    frames: Vec<Box<Frame>>,
    entry_sites: Vec<Option<lash_kernel_state::paged::SiteRef>>,
    #[cfg(test)]
    binding_canonicalizations: usize,
    #[cfg(test)]
    formula_sources: std::cell::RefCell<Vec<usize>>,
    /// Values moved into or out of a register or a frame slot.
    #[cfg(test)]
    value_moves: usize,
    #[cfg(test)]
    activation_records: Vec<CodeId>,
    #[cfg(test)]
    activation_sites: Vec<CodeId>,
}

struct Frame {
    code: CodeId,
    /// The selected Kir version; cold frames start with empty caches.
    scalar_blocks: bool,
    slots: frames::Slots,
    record: FrameRecord,
    access: Access<'static>,
    exe: Arc<Executable>,
    control: Vec<Control>,
    /// The statement whose action this frame waits on.
    awaiting: Option<StmtId>,
    /// The action nested in the awaited statement the frame stands at, by
    /// its resume entry in the statement's stream under this layout; the
    /// record's `pending` saves it by site (`K-EVAL-008`). `None` waits in
    /// the statement's own action.
    nested: Option<lash_kernel_exec::ActIx>,
    /// The library call this frame is the body of, charged when it ends.
    library: Option<LibraryCall>,
    /// The departure an entry's run-root cleanup holds while it runs: the
    /// body's result or raise, which ends the run once the cleanup returns.
    cleaning: Option<Result<Value, Value>>,
    /// Non-owning comparison with the last row written (FIG-6586).
    published: Option<frames::Publication>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    Request(WaitId),
    Join(TaskId),
    JoinMany(u64),
}

struct Task {
    counters: tasks::Occurrences,
    record: lash_kernel_state::paged::TaskRecord,
    frames: frames::Frames,
    /// Non-owning comparison with the last row written (FIG-6586).
    published: Option<tasks::Publication>,
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
    /// The waits issued since the last park, once found.
    issued: Option<issued::Issued>,
    /// The effects and sleeps requested since the last park, in request
    /// order.
    // Issued requests are retained in waits until reporting.
    /// The waits withdrawn since the last park, and every one ever.
    withdrawn: queue::Queue<u64>,
    withdrawn_ever: Tree<u64>,
    /// The mode each effect is performed in, bound at the start and saved
    /// with the run (`K-EFF-012`).
    modes: Tree<lash_kernel_state::paged::NameRef>,
    /// The session's saved functions ([`FunctionRecord`]s by declared
    /// name), when the run is a session cell.
    ///
    /// [`FunctionRecord`]: lash_kernel_state::paged::FunctionRecord
    functions: lash_kernel_state::paged::KeyedRoot,
    /// The bindings the host projected into the cell, which its carry
    /// leaves out.
    host: Vec<Name>,
    /// How the cell's end saves the functions its carry would drop; none
    /// drops them.
    save: Option<Box<dyn crate::interface::CellSave>>,
    session_cell: bool,
    /// What the statement being run holds outside any variable.
    pins: Pins,
    fresh: Vec<EntityId>,
    charging: bool,
    scalar_blocks: bool,
    charged: u64,
    /// The cost table of the kernel version the document states.
    costs: crate::costs::Costs,
    live_tasks: u32,
    census_refinements: u64,
    /// The registers: the values of the expressions of the statement being
    /// run and the arguments of the calls and actions it makes. No step
    /// reads one an earlier step wrote. Initialized storage is reused,
    /// while each step's end releases its logical values.
    regs: registers::Registers,
    storage: Storage,
    steps: steps::StepCheck,
    end: Option<End>,
    ended: bool,
    /// What observes the run, when anything does: never part of its state.
    observer: Option<Box<Observer>>,
    /// Statement starts in this run segment, never captured in parked state.
    ran: Vec<u64>,
    /// What a law has the run do at each statement boundary (FIG-6318).
    #[cfg(test)]
    pub(crate) boundary: Option<Box<dyn FnMut(u64) -> Boundary>>,
    #[cfg(test)]
    boundaries: u64,
}

/// What a law's hook has the run do at a statement boundary (FIG-6318).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Boundary {
    /// A collection through the machine's collection point.
    Collect,
}

impl KernelMachine {
    /// Saves the functions the cell's end would drop through `save`
    /// (`K-SES-004`).
    pub fn set_cell_save(&mut self, save: Box<dyn crate::interface::CellSave>) {
        self.save = Some(save);
    }
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
    /// Values being transferred between an existing register and its caller.
    borrowed: Vec<Value>,
    /// Local costs of immediate pins, which have no edges to trace.
    bytes: u64,
    #[cfg(test)]
    scalar_root_insertions: u64,
    #[cfg(test)]
    descriptor_pins: bool,
    #[cfg(test)]
    force_precision: bool,
    #[cfg(test)]
    scalar_locations: Vec<Option<u64>>,
    #[cfg(test)]
    reservations: Vec<(u64, u64)>,
}

impl Pins {
    #[inline(always)]
    fn scalar(value: &Value) -> bool {
        matches!(
            value,
            Value::Null
                | Value::Absent
                | Value::Bool(_)
                | Value::Float(_)
                | Value::Int(lash_kernel_doc::heap::IntRef::Inline(_))
        )
    }

    /// Holds `value` until the statement ends, and gives the bytes to
    /// reserve for it.
    #[inline(always)]
    fn pin(&mut self, value: &Value) -> Result<u64, Halt> {
        if matches!(value, Value::Bool(_)) {
            return Ok(0);
        }
        let bytes = value_bytes(value);
        self.hold(value, bytes)?;
        Ok(bytes)
    }

    /// Holds `value`, of `bytes` bytes.
    #[inline(always)]
    fn hold(&mut self, value: &Value, bytes: u64) -> Result<(), Halt> {
        if matches!(value, Value::Bool(_)) {
            return Ok(());
        }
        #[cfg(test)]
        {
            if self.force_precision && Self::scalar(value) {
                self.scalar_locations.push(Some(bytes));
            }
            // The precision law's oracle retains every original descriptor.
            if self.descriptor_pins {
                self.root(value);
                return Ok(());
            }
        }
        if Self::scalar(value) {
            self.bytes = self.bytes.checked_add(bytes).ok_or_else(|| {
                HeapFault::Malformed("statement scalar holding cost overflow".into())
            })?;
        } else {
            self.root(value);
        }
        Ok(())
    }

    #[inline(always)]
    fn root(&mut self, value: &Value) {
        #[cfg(test)]
        if Self::scalar(value) {
            self.scalar_root_insertions += 1;
        }
        self.values.push(value.clone());
    }

    fn clear(&mut self) {
        #[cfg(test)]
        if self.force_precision {
            self.scalar_locations.push(None);
        }
        self.values.clear();
        self.borrowed.clear();
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
            other => write_halt(other),
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
    pub(crate) fn verify_step_structure(&mut self) {
        self.steps.verify = true;
    }

    /// Checks the ops of every statement of the document the run executes
    /// (`lash_kernel_exec::verify_stream`): each range is well formed and
    /// spans no Boundary. Gives how many statements it checked; the
    /// library's are [`crate::PreparedLibrary::verify_stream`]'s.
    pub(crate) fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError> {
        self.exe.verify_stream()
    }

    /// How many statement ends the step-structure check has passed.
    pub(crate) fn verified_steps(&self) -> u64 {
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
                lash_kernel_state::kernel_internals::metadata::metadata_key(&name),
                lash_kernel_state::paged::NameRef::new(&name, self.heap.pages())?,
                self.heap.pages(),
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
                    &lash_kernel_state::kernel_internals::metadata::metadata_key(&name),
                    self.heap.pages(),
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
            self.tasks.after_refinement(self.heap.task_root().clone())?;
            self.census_refinements = refinements;
        }
        Ok(())
    }
    /// Publishes what the run holds as the census's persistent and
    /// statement roots, with every frame's coordinates: what a seal, a
    /// park or a carry needs.
    fn publish_roots(&mut self) -> Result<(), Halt> {
        self.publish(true)
    }

    /// Publishes what the run holds, leaving frames' coordinates where they
    /// were last published: the accounting and the trace before an
    /// allocation, a reservation or a call read held values only, so a
    /// frame that only moved is not written (FIG-6318).
    fn publish_holdings(&mut self) -> Result<(), Halt> {
        self.publish(false)
    }

    fn publish(&mut self, coordinates: bool) -> Result<(), Halt> {
        native_heap::Roots {
            tasks: &mut self.tasks,
            joins: &self.joins,
            registers: &self.regs,
            pinned: &self.pins.values,
            borrowed: &self.pins.borrowed,
            pinned_bytes: self.pins.bytes,
            fresh: &self.fresh,
            refinements: &mut self.census_refinements,
        }
        .publish(&mut self.heap, coordinates)
        .map_err(|error| self.heap_halt(error))
    }

    /// The law's forced collection hook at a statement boundary.
    #[cfg(test)]
    fn at_boundary(&mut self) -> Result<(), MachineError> {
        let at = self.boundaries;
        self.boundaries += 1;
        let Some(hook) = &mut self.boundary else {
            return Ok(());
        };
        let Boundary::Collect = hook(at);
        self.publish_holdings().map_err(machine_error)?;
        self.heap.collect().map_err(MachineError::Heap)?;
        self.reconcile_sweep().map_err(machine_error)
    }

    /// The collection point: under pressure, publishes what the run holds
    /// and refines exactly with `bytes` pending, so the verdict is given
    /// here, over current roots (FIG-6318).
    pub(super) fn refine_for(&mut self, bytes: u64) -> Result<(), Halt> {
        self.publish_holdings()?;
        self.heap.refine(bytes).map_err(|e| self.heap_halt(e))
    }

    /// Makes a heap write. One that needs an exact refinement while the
    /// run's roots are unpublished refuses before it changes anything; the
    /// run refines at its collection point and makes the write again,
    /// which the refinement left room for (FIG-6318).
    pub(super) fn heap_write<T, E: From<Halt>>(
        &mut self,
        mut write: impl FnMut(&mut PagedHeap) -> Result<T, E>,
    ) -> Result<T, E> {
        self.heap.take_refusal();
        match write(&mut self.heap) {
            Err(error) => match self.heap.take_refusal() {
                Some(bytes) => {
                    self.refine_for(bytes)?;
                    write(&mut self.heap)
                }
                None => Err(error),
            },
            done => done,
        }
    }

    /// The end a halt outside a step gives the run: a bound ends it as the
    /// guest's own error, anything else is the machine's.
    fn ended_by(&mut self, halt: Halt) -> Result<Step, MachineError> {
        match halt {
            Halt::Bound(exceeded) => Ok(self.finish_with(End::Error(RunError::Bound(*exceeded)))),
            Halt::Memory => Ok(self.finish_with(End::Error(RunError::Bound(BoundExceeded {
                bound: Bound::Memory,
                limit: self.bounds.memory,
                function: None,
            })))),
            other => Err(machine_error(other)),
        }
    }

    /// Runs one statement of `task`, or resumes the one it waits in.
    fn step(&mut self, task: TaskId, host: &mut dyn Host, exe: &Executable) -> Result<(), Halt> {
        self.tasks.protect(Some(task));
        let result = self.step_protected(task, host, exe);
        self.tasks.protect(None);
        result
    }

    fn step_protected(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Result<(), Halt> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::step();
        // What the run holds is published only where something can read
        // it: the slice's yield, park, end or seal, a native call handed
        // the heap, and a reservation or write under pressure. From here
        // the installed roots are stale, and a write that needs a
        // refinement refuses rather than trace them (FIG-6318).
        self.heap.execution_roots_moved();
        if self.observer.is_some() {
            self.observe_step(task, exe);
        }
        self.pins.clear();
        self.fresh.clear();
        if self.task(task)?.take_requeue() {
            // The one more turn a `then yield` wait takes once its outcome
            // is in, as `yield` takes it (`K-TASK-005`, `K-TASK-006`): it
            // charges nothing and runs no statement.
            self.ready.push_back(task)?;
            self.current = None;
            return Ok(());
        }
        let outcome = match self.task(task)?.take_incoming() {
            Some(incoming) => {
                self.refresh_charging(task, exe)?;
                self.resume(task, exe, incoming)
                    .and_then(|value| {
                        self.finish_action(task, host, exe, value, exec::ResultOrigin::Delivered)
                    })
                    .and_then(|ran| self.resumed(task, exe, ran))
            }
            None => self.advance(task, host, exe),
        };
        self.settle(task, host, exe, outcome)
    }

    fn refresh_charging(&mut self, task: TaskId, exe: &Executable) -> Result<(), Halt> {
        let (charging, scalar_blocks) = self
            .task(task)?
            .frames
            .last_mut()?
            .map_or((true, false), |frame| {
                (exe.code(frame.code).charged, frame.scalar_blocks)
            });
        self.charging = charging;
        self.scalar_blocks = scalar_blocks;
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
            Err(Interrupt::Raise(value)) => Completion::Throw(value),
            Err(Interrupt::Halt(halt)) => return Err(*halt),
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
        let access = self.heap.pages().clone();
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
                        &lash_kernel_state::kernel_internals::directory::ordinal_key(order),
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
        woken.wake(incoming, false);
        self.ready.push_back(task)?;
        Ok(())
    }

    /// Examines the other tasks at the run's end and gives the end
    /// (`K-TASK-018`). `ender` is the task that ran `finish` or `fail`.
    fn conclude(&mut self, ender: Option<TaskId>, ending: Ending) -> Result<End, Halt> {
        let mut unfinished = Vec::new();
        let mut unobserved = Vec::new();
        let access = self.heap.pages().clone();
        self.tasks.visit(&mut |id, task| {
            if id != TaskId::MAIN && ender != Some(id) && !task.passed {
                match task.state {
                    TaskState::Ended(_) if task.failed && !task.observed => {
                        unobserved.push(task.identity)
                    }
                    TaskState::Ended(_) => (),
                    _ => unfinished.push((task.identity, task.frames.clone())),
                }
            }
            true
        })?;
        if !unfinished.is_empty() || !unobserved.is_empty() {
            let mut sources = Vec::new();
            let unfinished = unfinished
                .into_iter()
                .map(|(id, frames)| {
                    let identity = id.resolve(&access)?;
                    if let Some(site) = tasks::source_body(&frames, &self.exe, &access)? {
                        sources.push((identity.clone(), site));
                    }
                    Ok(identity)
                })
                .collect::<Result<Vec<_>, lash_kernel_doc::heap::HeapFault>>()?;
            let unobserved = unobserved
                .into_iter()
                .map(|id| id.resolve(&access))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
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
                .map(|id| id.resolve(self.heap.pages()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(MachineError::Heap)?;
            return Ok(self.finish_with(End::Error(RunError::Deadlock { waiting })));
        }
        let requests = self.report_issued()?;
        let mut withdrawn = Vec::new();
        while let Some(wait) = self.withdrawn.pop_front().map_err(MachineError::Heap)? {
            withdrawn.push(WaitId(wait));
        }
        self.tasks
            .project_occurrences()
            .map_err(MachineError::Heap)?;
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
                    None => {
                        if let Err(halt) = self.publish_holdings() {
                            return self.ended_by(halt);
                        }
                        return self.park(host);
                    }
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
                    let operation = self
                        .task(task)
                        .ok()
                        .and_then(|task| task.frames.operation().ok().flatten());
                    // A completion charge already names its owning call.
                    // In particular, a native spawned root can halt while
                    // the spawner's library frame is still the running one.
                    if let Some(lib) = operation
                        && (exceeded.bound != Bound::Charge || exceeded.function.is_none())
                    {
                        exceeded.function = Some(self.exe.lib(lib).definition.name.clone());
                    } else if exceeded.function.is_none()
                        && let Some(lib) = self
                            .frame(task)
                            .ok()
                            .and_then(|frame| frame.library.as_ref().map(|call| call.lib))
                    {
                        exceeded.function = Some(self.exe.lib(lib).definition.name.clone());
                    }
                    return Ok(self.finish_with(End::Error(RunError::Bound(*exceeded))));
                }
                Err(Halt::Heap(error)) => return Err(MachineError::Heap(*error)),
                Err(Halt::Memory) => {
                    return Ok(self.finish_with(End::Error(RunError::Bound(BoundExceeded {
                        bound: Bound::Memory,
                        limit: self.bounds.memory,
                        function: None,
                    }))));
                }
                Err(Halt::Finish(result)) => {
                    return match self.conclude(Some(task), Ending::Finished(*result)) {
                        Ok(end) => Ok(self.finish_with(end)),
                        Err(halt) => self.ended_by(halt),
                    };
                }
                Err(Halt::Fail(reason)) => {
                    return match self.conclude(Some(task), Ending::Failed(*reason)) {
                        Ok(end) => Ok(self.finish_with(end)),
                        Err(halt) => self.ended_by(halt),
                    };
                }
            }
            self.regs.clear();
            self.pins.clear();
            self.fresh.clear();
            self.heap
                .release_statement_roots()
                .map_err(MachineError::Heap)?;
            #[cfg(test)]
            self.at_boundary()?;
            if let Some(end) = self.end.take() {
                if let Err(halt) = self.publish_holdings() {
                    return self.ended_by(halt);
                }
                return Ok(self.finish_with(end));
            }
            let more = self.current.is_some() || !self.ready.is_empty();
            if more && self.charged.saturating_sub(before) >= slice {
                if let Err(halt) = self.publish_holdings() {
                    return self.ended_by(halt);
                }
                if host.cancel_requested() {
                    return Ok(self.finish_with(End::Cancelled));
                }
                return Ok(Step::Slice);
            }
        }
    }
}

/// The halt a heap write's error gives a run.
fn write_halt(error: HeapWriteError) -> Halt {
    match error {
        HeapWriteError::Memory => Halt::Memory,
        HeapWriteError::Fault(error) => Halt::from(error),
        HeapWriteError::Raised { kind, message } => {
            Halt::Fault(format!("an allocation returned {kind}: {message}"))
        }
        // Every write a run makes goes through `heap_write`, which retries
        // a refused one.
        HeapWriteError::NeedsPrecision { bytes } => Halt::from(HeapFault::Malformed(format!(
            "a write refused for a refinement of {bytes} bytes outside the write wrapper"
        ))),
    }
}

fn machine_error(halt: Halt) -> MachineError {
    match halt {
        Halt::Heap(error) => MachineError::Heap(*error),
        Halt::Host(error) => MachineError::Host(error),
        other => MachineError::Fault {
            problem: format!("{other:?}"),
        },
    }
}

#[cfg(test)]
impl KernelMachine {
    /// Sets `task`'s state as its next publication writes it: what a
    /// continuation law plants for a state the grammar refuses.
    pub(crate) fn tamper_task_state(&mut self, task: TaskId, state: TaskState) -> Result<(), Halt> {
        self.task(task)?.record.state = state;
        Ok(())
    }

    /// The tasks that stand delivered (`K-TASK-005`): their `then yield`
    /// outcome is in and their turn not yet taken.
    pub(crate) fn delivered_tasks(&mut self) -> Vec<TaskId> {
        let mut delivered = Vec::new();
        self.publish_holdings().expect("the run publishes");
        self.tasks
            .visit(&mut |id, task| {
                if matches!(task.state, TaskState::Resuming { requeue: true, .. }) {
                    delivered.push(id);
                }
                true
            })
            .expect("the task rows read");
        delivered
    }

    /// The shared kernel equality domain at a projection point.
    pub(crate) fn semantic_checkpoint(&mut self) -> crate::conformance::Checkpoint {
        let sealed = self
            .seal(lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })
            .expect("seal")
            .expect("unbounded seal");
        let access = self.heap.pages().clone();
        struct Source(Access<'static>);
        impl lash_kernel_state::paged::PageSource for Source {
            fn read(
                &mut self,
                pages: &[lash_kernel_doc::heap::PageRef],
            ) -> Result<Vec<Result<lash_kernel_state::paged::PageAnswer, HeapFault>>, HeapFault>
            {
                Ok(pages.iter().map(|page| self.0.read(*page)).collect())
            }
            fn seal(
                &mut self,
                _: lash_kernel_state::paged::PageBody,
            ) -> Result<lash_kernel_doc::heap::PageRef, HeapFault> {
                Err(HeapFault::Malformed(
                    "comparison reader cannot write".into(),
                ))
            }
        }
        crate::conformance::checkpoint(&sealed.root, Box::new(Source(access)))
            .expect("semantic checkpoint")
    }

    /// The phase of every published frame row that stands at a nested
    /// action, by task and frame index.
    pub(crate) fn pending_phases(
        &mut self,
    ) -> Vec<(TaskId, usize, lash_kernel_state::paged::PhaseRecord)> {
        let mut phases = Vec::new();
        let access = self.heap.pages().clone();
        self.publish_holdings().expect("the run publishes");
        self.tasks
            .visit(&mut |id, task| {
                for index in 0..task.frames.len() {
                    if let Ok(Some((_, frame))) = task.frames.rank_get(index, &access)
                        && let Some(pending) = &frame.pending
                    {
                        phases.push((id, index as usize, pending.phase));
                    }
                }
                true
            })
            .expect("the task rows read");
        phases
    }

    /// The header of every published loop row: where each loop stood.
    pub(crate) fn loop_headers(&mut self) -> Vec<lash_kernel_state::paged::HeaderRecord> {
        let mut headers = Vec::new();
        let access = self.heap.pages().clone();
        self.publish(true).expect("the run publishes");
        self.tasks
            .visit(&mut |_, task| {
                for index in 0..task.frames.len() {
                    if let Ok(Some((_, frame))) = task.frames.rank_get(index, &access) {
                        for at in 0..frame.loops.len() {
                            if let Ok(Some((_, row))) = frame.loops.rank_get(at, &access) {
                                headers.push(row.header);
                            }
                        }
                    }
                }
                true
            })
            .expect("the task rows read");
        headers
    }

    /// Applies `edit` to the saved continuation of `task`'s frame
    /// `index`: a continuation law's tamper with the grammar.
    pub(crate) fn tamper_pending(
        &mut self,
        task: TaskId,
        index: usize,
        edit: impl FnOnce(&mut Option<lash_kernel_state::paged::PendingRecord>, &Access<'_>),
    ) -> Result<(), String> {
        let task = self.task(task).map_err(|halt| format!("{halt:?}"))?;
        let mut edit = Some(edit);
        let edited = task
            .frames
            .pending_mut(index as u64, &mut |pending, access| {
                if let Some(edit) = edit.take() {
                    edit(pending, access);
                }
            })
            .map_err(|fault| format!("{fault:?}"))?;
        if edited {
            Ok(())
        } else {
            Err("the frame has no row".to_string())
        }
    }
}
