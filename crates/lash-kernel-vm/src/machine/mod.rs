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
pub(crate) mod islands;
mod issued;
mod loops;
mod observing;
mod operation_entry;
mod ops;
#[cfg(test)]
mod park_laws;
mod plans;
pub(crate) mod project;
mod queue;
mod refine;
mod regions;
pub(crate) mod registers;
mod segments;
mod session;
pub(crate) mod sites;
mod steps;
mod stream;
mod tasks;

use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{Datum, DocumentId, Name, TaskId, Value};

use crate::compile::Layout;
use crate::compile::{BlockId, CodeId, Executable, LibId, StmtId};
use crate::data::{CopyError, Raised};
use crate::interface::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, End, Host, Machine, MachineError,
    Meters, Outcome, Park, Program, RunError, StartError, Step, WaitId,
};
use crate::observe::{Event, GuestPhase, Observer};
use lash_kernel_doc::heap::{HeapFault, PageId};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::tree::Tree;
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, EndedTask, HeapLifecycle, HeapWriteError,
    IncomingRecord as Incoming, JoinerRecord as Joiner, ListJoinRecord as ListJoin, SemanticHeap,
    TaskStateRecord as TaskState, WaitPhase, WaitRecord,
};
use loops::Cursor;
use project::SharedArena;
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
    /// The machine raises an error it has not built yet: built in the
    /// arena where something takes the raise ([`KernelMachine::thrown`]).
    Refuse(Box<Refusal>),
    Halt(Box<Halt>),
}

/// An error the machine raises, before the arena holds it: its kind, its
/// message, and its data, null or a text.
#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) kind: std::borrow::Cow<'static, str>,
    pub(crate) message: String,
    pub(crate) data: Option<String>,
}

impl Interrupt {
    fn raised(value: Value) -> Self {
        Self::Raise(Box::new(value))
    }
    pub(crate) fn refused(
        kind: impl Into<std::borrow::Cow<'static, str>>,
        message: impl Into<String>,
    ) -> Self {
        Self::Refuse(Box::new(Refusal {
            kind: kind.into(),
            message: message.into(),
            data: None,
        }))
    }
    fn in_function(self, function: &lash_kernel_doc::FunctionName) -> Self {
        match self {
            Self::Halt(halt) => Self::from((*halt).in_function(function)),
            other => other,
        }
    }

    /// What answering a call of `lib` interrupted with: a bound passed there
    /// names `lib` (`K-LIB-006`). A generated plan's are left as they are:
    /// it names the bounds passed inside it as its body's frames would, and
    /// its admission names as the body's does.
    fn answering(self, lib: &crate::compile::Lib) -> Self {
        match lib.run {
            crate::compile::LibRun::Plan { .. } | crate::compile::LibRun::DeferredPlan { .. } => {
                self
            }
            _ => self.in_function(&lib.definition.name),
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
        Self::refused(raised.kind, raised.message)
    }
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
            HeapWriteError::Raised { kind, message } => Self::refused(kind, message),
            other => Self::from(write_halt(other)),
        }
    }
}
impl From<lash_kernel_doc::NativeError> for Interrupt {
    fn from(error: lash_kernel_doc::NativeError) -> Self {
        use lash_kernel_doc::NativeError;
        match error {
            NativeError::Raised(error) => Self::raised(error),
            NativeError::Refused { kind, message } => Self::refused(kind, message),
            NativeError::Guard(error) => Self::from(Halt::Fault(format!(
                "an arena operation returned a guard: {error}"
            ))),
            NativeError::Retry(retry) => Self::from(Halt::Fault(format!(
                "an arena operation outside a call needs {retry:?}"
            ))),
            NativeError::Decline => Self::from(Halt::Fault("an arena operation declined".into())),
            NativeError::Malformed(problem) => Self::from(Halt::Fault(problem.into())),
        }
    }
}
impl From<crate::data::CopyError> for Interrupt {
    fn from(error: crate::data::CopyError) -> Self {
        match error {
            CopyError::Raised(error) => error.into(),
            CopyError::Heap(error) => error.into(),
        }
    }
}

fn raise<T>(kind: &'static str, message: impl Into<String>) -> Eval<T> {
    Err(Interrupt::refused(kind, message))
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
pub(crate) enum SlotState {
    Empty,
    Value(Value),
    /// A variable a closure shares: the arena cell that holds it.
    Cell(ArenaHandle),
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
    /// The call's arguments, which its formula reads when it ends.
    args: Box<[Value]>,
}

/// Activation storage and immutable entry references belong to this page
/// access, rather than the executable shared by independent machines.
#[derive(Default)]
struct Storage {
    #[expect(clippy::vec_box, reason = "reuse the frame allocation across calls")]
    frames: Vec<Box<Frame>>,
    entry_sites: Vec<Option<lash_kernel_state::paged::SiteRef>>,
    /// Each closure code's site, which every closure of it shares.
    closure_sites: Vec<Option<Arc<lash_kernel_doc::Site>>>,
    /// A spent closure call's capture buffer, which the next one's cells
    /// are copied into.
    captures: Vec<ArenaHandle>,
    /// Spent continuations' role buffers, which the next saved ones fill.
    roles: Vec<Vec<frames::Role>>,
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
    // Rebuilt from the admitted code; never written to a frame row.
    charging: bool,
    slots: frames::Slots,
    /// Where the frame stands when no block of its says: its body's
    /// entry, a return policy's site, or its body while a run-root cleanup
    /// runs.
    statement: lash_kernel_state::paged::SiteRef,
    /// The continuation it saved where it stands at an action nested in
    /// its statement (`K-EVAL-008`).
    pending: frames::Continuation,
    control: Vec<Control>,
    /// The statement whose action this frame waits on.
    awaiting: Option<StmtId>,
    /// The action nested in the awaited statement the frame stands at, by
    /// its resume entry in the statement's stream under this layout; the
    /// `pending` continuation saves it by site (`K-EVAL-008`). `None` waits in
    /// the statement's own action.
    nested: Option<lash_kernel_exec::ActIx>,
    /// The library call this frame is the body of, charged when it ends.
    library: Option<LibraryCall>,
    /// The departure an entry's run-root cleanup holds while it runs: the
    /// body's result or raise, which ends the run once the cleanup returns.
    cleaning: Option<Result<Value, Value>>,
    /// What the frame held outside its variables when last projected, in
    /// control order (`frames::Frame::visit_held`).
    held: Vec<Value>,
    /// Whether the frame's row is its own: the frame was restored from it
    /// or projected into it.
    owned: bool,
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
    memory_stream: stream::Runner,
    heap: PagedHeap,
    /// The objects the run executes on; the heap is their projection.
    arena: SharedArena,
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
    /// The session bindings this cell read or wrote, by binding key: arena
    /// values over the heap's slots, given their page forms only at a
    /// projection point (seam C).
    session: session::Session,
    /// What the statement being run holds outside any variable.
    pins: Pins,
    /// A seal ran out of budget mid-projection: the run refuses to run
    /// until a seal completes it (seam G).
    projecting: bool,
    charging: bool,
    /// The charge region the running statement stands in, admitted whole
    /// (`stream::Admitted`): what it executed is charged when it closes or
    /// is left.
    region: Option<stream::Admitted>,
    /// The charge at which the running slice returns, at its next step's
    /// end: a region admitted whole ends strictly below it.
    slice_end: u64,
    /// The property read sites' caches: never saved, so a restore
    /// discards them.
    sites: sites::SiteCaches,
    /// How many generated plans are running: each holds its call's depth
    /// unit as the body's frame would (`K-BND-002`).
    plans: u32,
    /// The call islands' reused buffers: never saved.
    islands: islands::Scratch,
    /// The segments standing at callback sites of a call being offered.
    segments: segments::Yielding,
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

impl std::fmt::Debug for KernelMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelMachine")
            .field("tasks", &self.tasks.len())
            .field("charged", &self.charged)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// What the statement holds outside any variable: values that name an
/// object or task, which a collection keeps. Immediate scalars need no pin.
#[derive(Default)]
struct Pins {
    values: Vec<Value>,
    /// Values being transferred between an existing register and its caller.
    borrowed: Vec<Value>,
    /// Every object a native call's arguments reach goes back on its pages
    /// before the call.
    #[cfg(test)]
    force_cold: bool,
    /// How many times a native call answered `Retry::Cold`.
    #[cfg(test)]
    cold_reads: u64,
}

impl Pins {
    #[inline(always)]
    fn scalar(value: &Value) -> bool {
        matches!(
            value,
            Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_) | Value::Int(_)
        )
    }

    #[inline(always)]
    fn root(&mut self, value: &Value) {
        self.values.push(*value);
    }

    fn clear(&mut self) {
        self.values.clear();
        self.borrowed.clear();
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

    /// The task `task`: the running task through its cursor
    /// (`tasks::Tasks::cursor`), any other after a refinement since the
    /// last lookup is reconciled.
    #[inline(always)]
    fn task(&mut self, task: TaskId) -> Result<&mut Task, Halt> {
        if self.tasks.cursor(task) {
            debug_assert!(self.tasks.cursor_is_fresh(task));
            return self
                .tasks
                .cursor_task()
                .ok_or_else(|| fault("the task cursor names no task"));
        }
        self.resolve_task(task)
    }

    #[inline(never)]
    fn resolve_task(&mut self, task: TaskId) -> Result<&mut Task, Halt> {
        self.reconcile_sweep()?;
        self.tasks
            .get_mut(task.0 as usize)?
            .ok_or_else(|| fault("a handle names no task"))
    }

    /// The top frame of `task`: the running frame through its task's
    /// cursor, with no lookup or check between the register operations
    /// that reach it.
    #[inline(always)]
    fn frame(&mut self, task: TaskId) -> Result<&mut Frame, Halt> {
        if self.tasks.frame_cursor(task) {
            debug_assert!(self.tasks.cursor_is_fresh(task));
            return self
                .tasks
                .cursor_frame()
                .ok_or_else(|| fault("the frame cursor names no frame"));
        }
        self.resolve_frame(task)
    }

    #[inline(never)]
    fn resolve_frame(&mut self, task: TaskId) -> Result<&mut Frame, Halt> {
        self.task(task)?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("a running task has no frame"))
    }

    /// The task table, with the rows of a refinement since the last lookup
    /// reconciled: what a read or write of the rows needs, since the
    /// cursor's lookups leave a refinement unreconciled.
    fn task_table(&mut self) -> Result<&mut tasks::Tasks, HeapFault> {
        self.reconcile_sweep()?;
        Ok(&mut self.tasks)
    }

    #[inline(always)]
    fn reconcile_sweep(&mut self) -> Result<(), HeapFault> {
        if self.bounds.memory_limit == crate::MemoryLimit::Off {
            return Ok(());
        }
        if self.heap.certificate().refinements != self.census_refinements {
            self.reconcile_refinement()?;
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn reconcile_refinement(&mut self) -> Result<(), HeapFault> {
        self.tasks.after_refinement(self.heap.task_root().clone())?;
        self.census_refinements = self.heap.certificate().refinements;
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
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::publish();
        self.identify(coordinates)?;
        self.project_session()?;
        self.flush_arena()?;
        self.reconcile_sweep()?;
        let pages = self.heap.pages().clone();
        let (registers, pinned, borrowed) = if self.bounds.memory_limit == crate::MemoryLimit::Off {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            let mut arena = self.arena.borrow_mut();
            let mut projected = |values: &[Value]| {
                values
                    .iter()
                    .map(|value| project::project(&mut arena, &pages, value))
                    .collect::<Result<Vec<PageValue>, HeapFault>>()
            };
            (
                projected(&self.regs.projection())?,
                projected(&self.pins.values)?,
                projected(&self.pins.borrowed)?,
            )
        };
        let tasks = &mut self.tasks;
        self.heap
            .update_execution_roots(
                self.joins.clone(),
                lash_kernel_state::paged::StatementRoots {
                    registers: &registers,
                    pinned: &pinned,
                    pinned_bytes: 0,
                    borrowed: &borrowed,
                },
                || {
                    if coordinates {
                        tasks.roots()
                    } else {
                        tasks.holdings()
                    }
                },
            )
            .map_err(|error| match error {
                HeapWriteError::Memory => bound(Bound::Memory, self.bounds.memory),
                other => write_halt(other),
            })?;
        Ok(self.reconcile_sweep()?)
    }

    /// Gives the new objects the projection is about to save their
    /// durable ids, in the canonical walk's order (seam C): from the
    /// frames it writes, task by task, then from every changed object
    /// with an id, in id order. Between statements nothing else holds a
    /// value; an exact refinement mid-statement walks the statement's
    /// registers and pins last.
    fn identify(&mut self, coordinates: bool) -> Result<(), Halt> {
        let mut roots = Vec::new();
        self.session_roots(&mut roots);
        self.tasks.walk_roots(coordinates, &mut roots);
        let mut arena = self.arena.borrow_mut();
        let walked = arena.walk(&roots).and_then(|()| arena.walk_dirty());
        let transient: Vec<ArenaHandle> = self
            .regs
            .projection()
            .iter()
            .chain(&self.pins.values)
            .chain(&self.pins.borrowed)
            .filter_map(Value::handle)
            .collect();
        walked
            .and_then(|()| arena.walk(&transient))
            .map_err(|error| fault(&error.to_string()))
    }

    /// Gives what `values` reach their durable ids, as a projection point
    /// would, before a copy-out reads them through the heap.
    pub(super) fn identify_values(&mut self, values: &[Value]) -> Result<(), Halt> {
        let roots: Vec<ArenaHandle> = values.iter().filter_map(Value::handle).collect();
        let mut arena = self.arena.borrow_mut();
        arena
            .walk(&roots)
            .and_then(|()| arena.walk_dirty())
            .map_err(|error| fault(&error.to_string()))
    }

    /// Writes every object the arena changed to the heap: what a
    /// publication, a seal and an exact refinement read.
    pub(super) fn flush_arena(&mut self) -> Result<(), Halt> {
        let exe = Arc::clone(&self.exe);
        let mut arena = self.arena.borrow_mut();
        project::flush(&mut arena, &mut self.heap, &exe).map_err(|error| match error {
            HeapWriteError::Memory => bound(Bound::Memory, self.bounds.memory),
            other => write_halt(other),
        })
    }

    /// The value a refusal raises: its error, built in the arena.
    pub(super) fn thrown(&mut self, refusal: Refusal) -> Box<Value> {
        let mut arena = self.arena.borrow_mut();
        let data = match &refusal.data {
            Some(text) => arena.text(text),
            None => Value::Null,
        };
        Box::new(arena.error(&refusal.kind, &refusal.message, data))
    }

    /// The value an interrupt raises, or the halt it is.
    pub(super) fn thrown_by(&mut self, interrupt: Interrupt) -> Result<Box<Value>, Halt> {
        match interrupt {
            Interrupt::Raise(value) => Ok(value),
            Interrupt::Refuse(refusal) => Ok(self.thrown(*refusal)),
            Interrupt::Halt(halt) => Err(*halt),
        }
    }

    /// The arena value of page form `page`.
    pub(super) fn admit_page(&mut self, page: &PageValue) -> Result<Value, Halt> {
        project::admit(&mut self.arena.borrow_mut(), self.heap.pages(), page).map_err(Halt::from)
    }

    /// The page form of `value`: an object without an id takes one now.
    pub(super) fn page_form(&self, value: &Value) -> Result<PageValue, Halt> {
        project::project(&mut self.arena.borrow_mut(), self.heap.pages(), value).map_err(Halt::from)
    }

    /// Decodes what a read of chunk `chunk` of the cold object `handle`
    /// needs, from the heap or its pages.
    pub(super) fn warm(&mut self, handle: ArenaHandle, chunk: u32) -> Result<(), Halt> {
        let mut arena = self.arena.borrow_mut();
        project::materialize(&mut arena, &self.heap, &self.exe, handle, chunk).map_err(Halt::from)
    }

    /// Opens the head of `handle` when it is a proxy: what a read of its
    /// length needs.
    pub(super) fn warm_head(&mut self, handle: ArenaHandle) -> Result<(), Halt> {
        let cold = self
            .arena
            .borrow()
            .is_cold(handle)
            .map_err(|error| fault(&error.to_string()))?;
        if cold {
            self.warm(handle, 0)?;
        }
        Ok(())
    }

    /// Decodes `handle` whole, chunk by chunk.
    pub(super) fn warm_whole(&mut self, handle: ArenaHandle) -> Result<(), Halt> {
        loop {
            let first = {
                let arena = self.arena.borrow();
                if !arena
                    .needs_decode(handle)
                    .map_err(|error| fault(&error.to_string()))?
                {
                    return Ok(());
                }
                arena
                    .first_cold_chunk(handle)
                    .map_err(|error| fault(&error.to_string()))?
            };
            self.warm(handle, first)?;
        }
    }

    /// Runs every later native call with each object its arguments reach
    /// back on its pages first, so that every chunk it reads answers
    /// `Retry::Cold` before it is warmed: the force-cold laws' run.
    #[cfg(test)]
    pub(crate) fn force_cold(&mut self) {
        self.pins.force_cold = true;
    }

    /// How many times a native call answered `Retry::Cold`.
    #[cfg(test)]
    pub(crate) fn cold_reads(&self) -> u64 {
        self.pins.cold_reads
    }

    /// Writes the arena to the heap and puts every object `args` reach
    /// back on its pages.
    #[cfg(test)]
    pub(super) fn chill_args(&mut self, args: &[Value]) -> Result<(), Halt> {
        self.identify_session()?;
        self.identify_values(args)?;
        self.flush_arena()?;
        let mut pending = Vec::new();
        for value in args {
            // An immutable keeps the page form its projection builds.
            self.page_form(value)?;
            pending.extend(value.handle());
        }
        let mut arena = self.arena.borrow_mut();
        let mut seen = std::collections::HashSet::new();
        while let Some(handle) = pending.pop() {
            if !seen.insert((handle.index(), handle.generation())) {
                continue;
            }
            let body = arena
                .body(handle)
                .map_err(|error| fault(&error.to_string()))?;
            lash_kernel_state::arena::each_member(body, &mut |member| {
                pending.extend(member.handle());
            });
            arena
                .evict(handle)
                .map_err(|error| fault(&error.to_string()))?;
        }
        Ok(())
    }

    /// Decodes every cold object `value` reaches: what a walk of its
    /// whole graph outside a native call reads.
    pub(super) fn warm_reachable(&mut self, value: &Value) -> Result<(), Halt> {
        self.reach(value, true)
    }

    /// Decodes what a measure of `value`'s graph reads: every cold object
    /// it reaches but a long text's or bytes' payload, whose size its
    /// summary gives.
    pub(super) fn warm_measured(&mut self, value: &Value) -> Result<(), Halt> {
        self.reach(value, false)
    }

    fn reach(&mut self, value: &Value, payloads: bool) -> Result<(), Halt> {
        if !self.arena.borrow().any_cold() {
            return Ok(());
        }
        let mut pending: Vec<ArenaHandle> = value.handle().into_iter().collect();
        let mut seen = std::collections::HashSet::new();
        while let Some(handle) = pending.pop() {
            if !seen.insert((handle.index(), handle.generation())) {
                continue;
            }
            let payload = matches!(
                self.arena.borrow().body(handle),
                Ok(lash_kernel_doc::arena::Body::LongText(_)
                    | lash_kernel_doc::arena::Body::Bytes(_))
            );
            if payload && !payloads {
                continue;
            }
            self.warm_whole(handle)?;
            let arena = self.arena.borrow();
            let body = arena
                .body(handle)
                .map_err(|error| fault(&error.to_string()))?;
            lash_kernel_state::arena::each_member(body, &mut |member| {
                pending.extend(member.handle());
            });
            if let lash_kernel_doc::arena::Body::Closure { captures, .. } = body {
                pending.extend(captures.iter().copied());
            }
        }
        Ok(())
    }

    /// Copies `value` out of the run (`K-EFF-002`).
    pub(super) fn copy_datum(&mut self, value: &Value) -> Result<Datum, CopyError> {
        self.warm_reachable(value).map_err(|halt| match halt {
            Halt::Heap(fault) => CopyError::Heap(*fault),
            other => CopyError::Heap(HeapFault::Malformed(format!("{other:?}"))),
        })?;
        crate::data::copy_out(&self.arena.borrow(), self.limits.value_depth(), value)
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
        self.arena.borrow_mut().rebased();
        self.reconcile_sweep().map_err(MachineError::Heap)
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
                    self.memory_pressure(bytes)?;
                    let again = write(&mut self.heap);
                    // The collection point refines over what the run holds
                    // in memory, not over installed roots, so the write
                    // refuses for precision again when the room it left is
                    // not enough.
                    if again.is_err() && self.heap.take_refusal().is_some() {
                        return Err(E::from(bound(Bound::Memory, self.bounds.memory)));
                    }
                    again
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
        // The prepared statement owns the kill set, including on park,
        // raise and halt exits. Clean up before the driver sees the result.
        self.regs.kill_statement();
        if self.bounds.memory_limit != crate::MemoryLimit::Off {
            self.pins.clear();
        }
        self.arena.borrow_mut().statement_ended();
        self.tasks.protect(None);
        result
    }

    fn step_protected(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Result<(), Halt> {
        // What the run holds is published only where something can read
        // it: the slice's yield, park, end or seal, a native call handed
        // the heap, and a reservation or write under pressure. From here
        // the installed roots are stale, and a write that needs a
        // refinement refuses rather than trace them (FIG-6318).
        self.heap.execution_roots_moved();
        if let Some(observer) = self.observer.as_deref() {
            #[cfg(any(test, feature = "perf-witness"))]
            if observer.work_witness {
                crate::perf_witness::step();
            }
            #[cfg(not(any(test, feature = "perf-witness")))]
            let _ = observer;
            self.observe_step(task, exe);
        }
        if self.bounds.memory_limit != crate::MemoryLimit::Off {
            self.pins.clear();
        }
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
                self.refresh_charging(task)?;
                self.resume(task, exe, incoming)
                    .and_then(|value| {
                        self.finish_action(task, host, exe, value, exec::ResultOrigin::Delivered)
                    })
                    .and_then(|ran| self.resumed(task, exe, ran))
            }
            None => self.advance(task, host, exe),
        };
        match self.settle(task, host, exe, outcome) {
            Ok(()) if !self.islands.runs_on() => Ok(()),
            settled => self.island_run_on(task, host, exe, settled),
        }
    }

    /// Inlined into the step: an island's exit is its only other caller.
    #[inline(always)]
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
            Err(interrupt) => Completion::Throw(self.thrown_by(interrupt)?),
        };
        self.complete(task, host, exe, completion)
    }

    /// Marks a task ended, wakes what waits on it and, for `main`, ends
    /// the run (`K-TASK-008`, `K-TASK-018`).
    fn end_task(&mut self, task: TaskId, outcome: Result<Value, Value>) -> Result<(), Halt> {
        self.identify_session()?;
        let pages = self.heap.pages().clone();
        let result = {
            let mut arena = self.arena.borrow_mut();
            match &outcome {
                Ok(value) => Ok(project::project(&mut arena, &pages, value)?),
                Err(value) => Err(project::project(&mut arena, &pages, value)?),
            }
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
            self.end = Some(match outcome {
                Ok(value) => self.conclude(None, Ending::Returned(value))?,
                Err(value) => End::Error(RunError::Uncaught(match self.copy_datum(&value) {
                    Ok(value) => value,
                    Err(CopyError::Raised(error)) => error.into_datum(),
                    Err(CopyError::Heap(error)) => return Err(error.into()),
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
                        let incoming = match decision {
                            Ok(value) => Incoming::Value(self.page_form(&value)?),
                            Err(value) => Incoming::Raise(self.page_form(&value)?),
                        };
                        self.wake(joiner, incoming)?;
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
        self.task_table()?.visit(&mut |id, task| {
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
                    let identity =
                        lash_kernel_doc::kernel_internals::task_label(id.resolve(&access)?);
                    if let Some(site) = tasks::source_body(&frames, &self.exe, &access)? {
                        sources.push((identity.clone(), site.position()));
                    }
                    Ok(identity)
                })
                .collect::<Result<Vec<_>, lash_kernel_doc::heap::HeapFault>>()?;
            let unobserved = unobserved
                .into_iter()
                .map(|id| {
                    id.resolve(&access)
                        .map(lash_kernel_doc::kernel_internals::task_label)
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
                sources,
            }));
        }
        Ok(match ending {
            Ending::Returned(value) => match self.copy_datum(&value) {
                Ok(result) => End::Finished(Box::new(self.finished(result, false)?)),
                Err(CopyError::Raised(raised)) => {
                    End::Error(RunError::Uncaught(raised.into_datum()))
                }
                Err(CopyError::Heap(fault)) => return Err(fault.into()),
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
            self.task_table()
                .and_then(|tasks| {
                    tasks.visit(&mut |_, task| {
                        if task.is_live() {
                            waiting.push(task.identity);
                        }
                        true
                    })
                })
                .map_err(MachineError::Heap)?;
            let waiting = waiting
                .into_iter()
                .map(|id| {
                    id.resolve(self.heap.pages())
                        .map(lash_kernel_doc::kernel_internals::task_label)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(MachineError::Heap)?;
            return Ok(self.finish_with(End::Error(RunError::Deadlock { waiting })));
        }
        let requests = self.report_issued()?;
        let mut withdrawn = Vec::new();
        while let Some(wait) = self.withdrawn.pop_front().map_err(MachineError::Heap)? {
            withdrawn.push(lash_kernel_doc::kernel_internals::wait_id(wait));
        }
        let tasks = self.task_table().map_err(MachineError::Heap)?;
        tasks.project_occurrences().map_err(MachineError::Heap)?;
        let (_, outstanding) = tasks.sealed_root().map_err(MachineError::Heap)?;
        let live = u64::from(self.live_tasks.saturating_sub(1));
        Ok(Step::Parked(Park {
            requests,
            withdrawn,
            outstanding,
            live,
        }))
    }

    fn finish_with(&mut self, end: End) -> Step {
        self.ended = true;
        Step::Ended(end)
    }

    /// Runs tasks until none is ready, the run ends, or `slice` units of
    /// charge have passed (`K-MACH-005`).
    fn run_slice<const METERED: bool>(
        &mut self,
        host: &mut dyn Host,
        slice: u64,
    ) -> Result<Step, MachineError> {
        let before = if METERED { self.charged } else { 0 };
        if METERED {
            self.slice_end = before.saturating_add(slice);
        }
        // With no charge meter the scheduling budget counts machine steps.
        // The entry chooses this specialization once; Exact keeps its charge slices.
        let mut remaining = slice.max(1);
        // A helper's callbacks run as islands only for a host that
        // observes no sites.
        self.islands.quiet = !host.observes_sites();
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
            self.heap
                .release_statement_roots()
                .map_err(MachineError::Heap)?;
            self.collect_arena().map_err(machine_error)?;
            #[cfg(test)]
            self.at_boundary()?;
            if let Some(end) = self.end.take() {
                if let Err(halt) = self.publish_holdings() {
                    return self.ended_by(halt);
                }
                return Ok(self.finish_with(end));
            }
            let more = self.current.is_some() || !self.ready.is_empty();
            let spent = if METERED {
                self.charged.saturating_sub(before) >= slice
            } else {
                remaining -= 1;
                remaining == 0
            };
            if more && spent {
                // A slice return projects only when the host seals: ids
                // never depend on where slices fall.
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

/// The most frames one projection on this thread has visited since the
/// last call.
#[cfg(test)]
pub(crate) fn take_most_visited_frames() -> usize {
    frames::take_most_visited()
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
        struct Source(lash_kernel_state::kernel_internals::tree::Access<'static>);
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

    /// The refusal the coordinate grammar gives the row of `task`'s frame
    /// `index`, restored afresh as a cold open restores it.
    pub(crate) fn restore_refusal(
        &mut self,
        task: TaskId,
        index: u64,
    ) -> Result<Option<lash_kernel_exec::coord::RestoreRefusal>, String> {
        let task = self.task(task).map_err(|halt| format!("{halt:?}"))?;
        task.frames
            .refusal(index)
            .map_err(|fault| format!("{fault:?}"))
    }

    /// Applies `edit` to the saved loop row `at` of `task`'s cold top
    /// frame: a continuation law's tamper with the grammar. The loop's
    /// site as it stood.
    pub(crate) fn tamper_loop(
        &mut self,
        task: TaskId,
        at: u64,
        edit: impl FnOnce(
            &mut lash_kernel_state::paged::LoopRecord,
            &lash_kernel_state::kernel_internals::tree::Access<'_>,
        ),
    ) -> Result<lash_kernel_exec::coord::SitePath, String> {
        let task = self.task(task).map_err(|halt| format!("{halt:?}"))?;
        task.frames
            .loop_mut(at, edit)
            .map_err(|fault| format!("{fault:?}"))
    }

    /// Applies `edit` to the saved continuation of `task`'s frame
    /// `index`: a continuation law's tamper with the grammar.
    pub(crate) fn tamper_pending(
        &mut self,
        task: TaskId,
        index: usize,
        edit: impl FnOnce(
            &mut Option<lash_kernel_state::paged::PendingRecord>,
            &lash_kernel_state::kernel_internals::tree::Access<'_>,
        ),
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
