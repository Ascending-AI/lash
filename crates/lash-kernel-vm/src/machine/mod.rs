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
mod callable_capture;
mod catches;
mod checkpoint;
mod controls;
mod exec;
mod export_abi;
mod foreign;
mod frames;
pub(crate) mod islands;
mod issued;
mod limits;
mod logical_held;
mod loops;
mod observing;
mod operation_entry;
mod ops;
mod origin;
#[cfg(test)]
mod park_laws;
pub(crate) mod project;
mod public_value;
mod queue;
mod raises;
mod refine;
pub(crate) mod registers;
mod session;
pub(crate) mod sites;
mod source_publication;
mod steps;
mod stream;
pub(crate) mod tasks;

use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::{Datum, TaskId, Value};

use lash_kernel_value::names::DocumentId;
use lash_kernel_value::names::Name;

use crate::compile::Layout;
use crate::compile::{BlockId, CodeId, Executable, LibId, StmtId};
use crate::data::{CopyError, Raised};
use crate::interface::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, End, Host, Machine, MachineError,
    Meters, Outcome, Park, Program, RunError, StartError, Step, WaitId,
};
use crate::observe::{GuestPhase, Observer};
use lash_kernel_doc::heap::{HeapFault, PageId};
use lash_kernel_state::kernel_internals::PagedHeap;
use lash_kernel_state::kernel_internals::directory::ordinal_key;
use lash_kernel_state::kernel_internals::tree::Tree;
use lash_kernel_state::paged::{
    BindingRecord, BoundRecord, EndedTask, HeapLifecycle, HeapWriteError,
    IncomingRecord as Incoming, JoinerRecord as Joiner, ListJoinRecord as ListJoin, SemanticHeap,
    TaskStateRecord as TaskState, WaitPhase, WaitRecord,
};
use limits::Limiters;
use loops::Cursor;
use project::SharedArena;
use session::{keyed, open_tree};

/// The bytes the capacity meter counts for a frame before its slots.
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
}

impl Halt {
    /// Preserve the innermost library definition that caused a bound.
    fn in_function(mut self, function: &lash_kernel_value::names::FunctionName) -> Self {
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
    fn in_function(self, function: &lash_kernel_value::names::FunctionName) -> Self {
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
        self.in_function(&lib.header.name)
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
            NativeError::Host(error) => Self::from(error),
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
        site: None,
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

#[derive(Clone, Copy, Debug)]
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
    entry_sites: std::collections::BTreeMap<CodeId, lash_kernel_state::paged::SiteRef>,
    /// Each closure code's site, which every closure of it shares.
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
    source_frontier: Option<crate::compile::room::publication::Frontier>,
    invocation: u64,
    logical: Option<logical_held::FrameRoots>,
    convention: Option<lash_kernel_doc::StartConvention>,
    public_result: Option<lash_kernel_value::types::Type>,
    link: lash_kernel_value::content_ids::LinkId,
    code: CodeId,
    // Rebuilt from the admitted code; never written to a frame row.
    charging: bool,
    /// Its code counts the run's pace: charged code under charge Off.
    paced: bool,
    slots: frames::Slots,
    /// Where the frame stands when no block of its says: its body's
    /// entry, a return policy's site, or its body while a run-root cleanup
    /// runs.
    statement: lash_kernel_state::paged::SiteRef,
    /// The continuation it saved where it stands at an action nested in
    /// its statement (`K-EVAL-008`).
    pending: frames::Continuation,
    control: controls::Controls,
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
    logical: lash_kernel_state::kernel_internals::logical::Lifecycle,
    /// Last checked publication of the shared logical ledger. Meters remain
    /// infallible; ledger faults are returned at the publication boundary.
    logical_memory: std::cell::Cell<u64>,
    program: Program,
    active_link: lash_kernel_value::content_ids::LinkId,
    /// The document's identity, which a parked run pins, once it has been
    /// taken.
    document: Option<DocumentId>,
    exe: Arc<Executable>,
    bounds: Bounds,
    /// What the run's heap was sealed holding, as its certificate records
    /// it: the capacity meter counts the run holding at least this until
    /// its first sweep recounts (FIG-6791).
    capacity_floor: u64,
    heap: PagedHeap,
    /// The objects the run executes on; the heap is their projection.
    arena: SharedArena,
    limits: lash_kernel_value::data_limits::KernelDataLimits,
    closure: PageId,
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
    /// Unreported cancellation intent, and every wait withdrawn forever.
    cancellations: queue::Queue<lash_kernel_value::sites::WaitCancellation>,
    withdrawn_ever: Tree<u64>,
    /// The mode each effect is performed in, bound at the start and saved
    /// with the run (`K-EFF-012`).
    modes: Tree<lash_kernel_value::names::OperationUse>,
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
    captions: Vec<lash_kernel_doc::CaptionEntry>,
    function_origins: crate::interface::FunctionOrigins,
    /// What decides at the run's bounds (FIG-6768); none refuses.
    limiters: Limiters,
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
    /// The running code counts the run's pace (`Frame::paced`).
    paced: bool,
    /// The charge region the running statement stands in, admitted whole
    /// (`stream::Admitted`): what it executed is charged when it closes or
    /// is left.
    region: Option<stream::Admitted>,
    /// The charge, or with charge Off the pace, at which the running
    /// slice returns, at its next step's end: a region admitted whole ends
    /// strictly below it.
    slice_end: u64,
    /// The property read sites' caches: never saved, so a restore
    /// discards them.
    sites: sites::SiteCaches,
    /// The call islands' reused buffers: never saved.
    islands: islands::Scratch,
    /// The run's charge; with charge Off, its pace: one unit for each
    /// statement, loop test and action atom of charged code. Slices count
    /// it, and nothing reads the pace out.
    charged: u64,
    /// The cost table of the kernel version the document states.
    live_tasks: u32,
    census_refinements: u64,
    /// The registers: the values of the expressions of the statement being
    /// run and the arguments of the calls and actions it makes. No step
    /// reads one an earlier step wrote. Initialized storage is reused,
    /// while each step's end releases its logical values.
    regs: registers::Registers,
    storage: Storage,
    end: Option<End>,
    ended: bool,
    /// What observes the run, when anything does: never part of its state.
    observer: Option<Box<Observer>>,
    /// Statement starts in this run segment, never captured in parked state.
    ran: Vec<u64>,
    /// The task whose raise the running step carries, and where it
    /// stands (`raises`): never saved.
    raised: Option<(TaskId, raises::Coordinate)>,
    /// The failed waits of this run segment and the catches that took
    /// them (`catches`): never saved.
    catches: catches::Catches,
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
}

impl KernelMachine {
    /// Checks the ops of every statement of the document the run executes
    /// (`lash_kernel_exec::verify_stream`): each range is well formed and
    /// spans no Boundary. Gives how many statements it checked; the
    /// library's are [`crate::PreparedLibrary::verify_stream`]'s.
    pub(crate) fn verify_stream(&self) -> Result<usize, lash_kernel_exec::KirError> {
        self.exe.verify_stream()
    }

    fn current_link(&self) -> lash_kernel_value::content_ids::LinkId {
        self.active_link
    }

    /// Read bindings are pinned by use; unresolved uses always wait.
    fn bind_modes(&mut self, modes: crate::interface::EffectModes) -> Result<(), StartError> {
        for (use_, mode) in modes {
            let declared = self
                .program
                .instances
                .get(&use_.link)
                .and_then(|instance| instance.operation(use_.slot));
            if !matches!(declared, Some(found) if found == use_) {
                return Err(StartError::UndeclaredRead { use_ });
            }
            if mode == lash_kernel_value::package::OperationMode::Read {
                self.modes
                    .write(operation_key(&use_), use_, self.heap.pages())?;
            }
        }
        Ok(())
    }

    fn effect_mode(
        &self,
        use_: &lash_kernel_value::names::OperationUse,
    ) -> Result<lash_kernel_value::package::OperationMode, HeapFault> {
        Ok(
            if self
                .modes
                .get(&operation_key(use_), self.heap.pages())?
                .is_some()
            {
                lash_kernel_value::package::OperationMode::Read
            } else {
                lash_kernel_value::package::OperationMode::Waited
            },
        )
    }

    fn operation(
        &mut self,
        task: TaskId,
        slot: lash_kernel_value::ImportSlot,
        unit: &lash_kernel_value::sites::Unit,
    ) -> Result<lash_kernel_value::OperationUse, Halt> {
        let link = self.frame(task)?.link;
        let instance = self
            .program
            .instances
            .get(&link)
            .ok_or_else(|| fault("operation context is not loaded"))?;
        let key = match unit {
            lash_kernel_value::sites::Unit::Library(function)
                if !self.program.documents[&instance.document]
                    .functions
                    .contains_key(function) =>
            {
                let registered = self
                    .program
                    .helpers()
                    .registered(function)
                    .or_else(|| self.program.library.registry().get(function))
                    .ok_or_else(|| fault("helper operation body is not loaded"))?;
                let import = registered
                    .imports()
                    .get(&slot)
                    .ok_or_else(|| fault("helper does not declare this operation"))?;
                lash_kernel_value::OperationSlot::Helper(
                    lash_kernel_value::HelperOperationId::of_import(import),
                )
            }
            _ => slot.into(),
        };
        instance
            .operation(key)
            .filter(|use_| use_.link == link && use_.slot == key)
            .ok_or_else(|| fault("perform has no explicit operation fill in its defining Link"))
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
        let lifecycle = self.logical.clone();
        let exe = Arc::clone(&self.exe);
        let access = self.heap.pages().clone();
        let frame = self
            .task(task)?
            .frames
            .last_mut()?
            .ok_or_else(|| fault("a running task has no frame"))?;
        if frame.logical.is_none() && frame.invocation != 0 {
            frame.logical = Some(logical_held::FrameRoots::restore(
                lifecycle, task, frame, &exe, &access,
            )?);
        }
        Ok(frame)
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
        if self.bounds.memory_meter == crate::MemoryMeter::Off {
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
        self.logical_flush()?;
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::publish();
        self.identify(coordinates)?;
        self.project_session()?;
        self.flush_arena()?;
        self.reconcile_sweep()?;
        let pages = self.heap.pages().clone();
        let (registers, pinned, borrowed) = if self.bounds.memory_meter == crate::MemoryMeter::Off {
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
            .map_err(write_halt)?;
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
        project::flush(&mut arena, &mut self.heap, &exe).map_err(write_halt)
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

    /// The value an interrupt `task` meets raises, or the halt it is.
    pub(super) fn thrown_by(
        &mut self,
        task: TaskId,
        interrupt: Interrupt,
    ) -> Result<Box<Value>, Halt> {
        let value = match interrupt {
            Interrupt::Raise(value) => value,
            Interrupt::Refuse(refusal) => self.thrown(*refusal),
            Interrupt::Halt(halt) => return Err(*halt),
        };
        self.raise_born(task)?;
        Ok(value)
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
        self.logical.handoff(&mut arena, &self.heap)?;
        let ledger = self.heap.logical_ledger();
        let fact = if arena
            .facts(handle)
            .map_err(|error| fault(&error.to_string()))?
            .entity
            .is_some()
        {
            Some(
                ledger
                    .borrow()
                    .authenticate_warm(&arena, handle, self.heap.pages())?,
            )
        } else {
            None
        };
        project::materialize(&mut arena, &self.heap, &self.exe, handle, chunk)?;
        if let Some(fact) = fact {
            fact.finish(&ledger.borrow(), &arena)?;
        }
        Ok(())
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
        self.islands.chilled = true;
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
        self.reconcile_sweep().map_err(MachineError::Heap)
    }

    /// The end a halt outside a step gives the run: a bound ends it as the
    /// guest's own error, anything else is the machine's.
    fn ended_by(&mut self, halt: Halt) -> Result<Step, MachineError> {
        match halt {
            Halt::Bound(exceeded) => Ok(self.finish_with(End::Error(RunError::Bound(*exceeded)))),
            other => Err(machine_error(other)),
        }
    }

    /// Runs one statement of `task`, or resumes the one it waits in.
    fn step(&mut self, task: TaskId, host: &mut dyn Host, exe: &Executable) -> Result<(), Halt> {
        let (link, program) = {
            let frame = self.frame(task)?;
            let program = exe
                .island_of(frame.code)
                .ok_or_else(|| fault("a frame's code has no register program"))?;
            (frame.link, program)
        };
        self.active_link = link;
        self.tasks.protect(Some(task));
        let result = self.step_protected(task, host, exe, program);
        // The prepared statement owns the kill set, including on park,
        // raise and halt exits. Clean up before the driver sees the result.
        self.regs.kill_statement();
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            self.pins.clear();
        }
        if result.is_ok() {
            self.logical_flush_controls(task, exe)?;
            self.logical_flush()?;
        }
        self.tasks.protect(None);
        result
    }

    fn step_protected(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        program: u32,
    ) -> Result<(), Halt> {
        // What the run holds is published only where something can read
        // it: the slice's yield, park, end or seal, a native call handed
        // the heap, and a reservation or write under pressure. From here
        // the installed roots are stale, and a write that needs a
        // refinement refuses rather than trace them (FIG-6318).
        self.heap.execution_roots_moved();
        self.raised = None;
        if let Some(observer) = self.observer.as_deref() {
            #[cfg(any(test, feature = "perf-witness"))]
            if observer.work_witness {
                crate::perf_witness::step();
            }
            #[cfg(not(any(test, feature = "perf-witness")))]
            let _ = observer;
            self.observe_step(task, exe);
        }
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
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
                self.delivered(task, host, exe, incoming)
            }
            None => self.advance_active(task, host, exe, program),
        };
        self.settle(task, host, exe, outcome)
    }

    /// The statement the top frame waits in continues with its delivered
    /// value at the same semantic coordinate.
    fn delivered(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        incoming: Incoming,
    ) -> Eval<Completion> {
        let value = self.resume(task, exe, incoming);
        self.logical.replace(
            lash_kernel_state::kernel_internals::logical::Owner::Incoming(task),
            None,
        );
        let value = value?;
        let ran = self.finish_action(task, host, exe, value, exec::ResultOrigin::Delivered)?;
        self.resumed(task, exe, ran)
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
            Err(interrupt) => Completion::Throw(self.thrown_by(task, interrupt)?),
        };
        self.complete(task, host, exe, completion)
    }

    fn park(&mut self, host: &mut dyn Host) -> Result<Step, MachineError> {
        if host.cancel_requested() {
            return Ok(self.finish_with(End::Cancelled));
        }
        if self.waits.is_empty() && self.cancellations.is_empty() {
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
                        .map(lash_kernel_value::kernel_internals::task_label)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(MachineError::Heap)?;
            return Ok(self.finish_with(End::Error(RunError::Deadlock { waiting })));
        }
        let requests = self.report_issued()?;
        let mut withdrawn = Vec::new();
        while let Some(wait) = self.cancellations.pop_front().map_err(MachineError::Heap)? {
            withdrawn.push(wait);
        }
        let tasks = self.task_table().map_err(MachineError::Heap)?;
        tasks.project_occurrences().map_err(MachineError::Heap)?;
        let (_, outstanding) = tasks.sealed_root().map_err(MachineError::Heap)?;
        let live = u64::from(self.live_tasks.saturating_sub(1));
        Ok(Step::Parked(Park {
            requests,
            cancel_requests: withdrawn,
            outstanding,
            live,
        }))
    }

    fn finish_with(&mut self, end: End) -> Step {
        self.ended = true;
        Step::Ended(end)
    }

    /// Runs tasks until none is ready, the run ends, or `slice` units of
    /// charge, or with charge Off of pace, have passed (`K-MACH-005`).
    /// Pace, like charge, does not depend on the plan: a region or an
    /// island counts it as the reference does, so a slice falls where it
    /// would without them.
    fn run_slice(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        let before = self.charged;
        self.slice_end = before.saturating_add(slice);
        // A helper's callbacks run as islands only for a host that
        // observes no sites.
        // The executable is the run's for its whole life.
        let exe = Arc::clone(&self.exe);
        loop {
            // Cancelling an admitted call is a host-visible operation. Stop
            // before cleanup or any other task can perform a later operation.
            if !self.cancellations.is_empty() {
                self.publish_holdings().map_err(machine_error)?;
                return self.park(host);
            }
            if let Some(end) = self.end.take() {
                if let Err(halt) = self.publish_holdings() {
                    return self.ended_by(halt);
                }
                return Ok(self.finish_with(end));
            }
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
                    if exceeded.bound == Bound::CallDepth {
                        // Depth belongs to guest recursion, even when its next
                        // comparison or arithmetic helper needs the final unit.
                        exceeded.function = None;
                        return Ok(self.finish_with(End::Error(RunError::Bound(*exceeded))));
                    }
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
                        exceeded.function = Some(self.exe.lib(lib).header.name.clone());
                    } else if exceeded.function.is_none()
                        && let Some(lib) = self
                            .frame(task)
                            .ok()
                            .and_then(|frame| frame.library.as_ref().map(|call| call.lib))
                    {
                        exceeded.function = Some(self.exe.lib(lib).header.name.clone());
                    }
                    return Ok(self.finish_with(End::Error(RunError::Bound(*exceeded))));
                }
                Err(Halt::Heap(error)) => return Err(MachineError::Heap(*error)),
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
            self.heap.release_statement_roots();
            self.collect_arena().map_err(machine_error)?;
            // The capacity meter's verdict point at a statement's end
            // (FIG-6791): what the run holds is in its tasks and session,
            // so a crossing the statement made is decided before the next
            // statement's effect.
            if let Err(halt) = self.capacity_check() {
                return self.ended_by(halt);
            }
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
        HeapWriteError::Fault(error) => Halt::from(error),
        HeapWriteError::Raised { kind, message } => {
            Halt::Fault(format!("an allocation returned {kind}: {message}"))
        }
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

#[cfg(any(test, feature = "testing"))]
impl KernelMachine {
    /// The shared kernel equality domain at a projection point.
    pub(crate) fn try_semantic_checkpoint(
        &mut self,
    ) -> Result<lash_kernel_state::semantic::Checkpoint, crate::SealError> {
        let sealed = self
            .seal(lash_kernel_state::paged::SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })?
            .ok_or_else(|| crate::SealError::Fault {
                problem: "an unbounded semantic seal left work".into(),
            })?;
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
        lash_kernel_state::semantic::checkpoint(&sealed.root, Box::new(Source(access)))
            .map_err(crate::SealError::Heap)
    }
}

#[cfg(test)]
impl KernelMachine {
    /// The shared kernel equality domain at a test's projection point.
    pub(crate) fn semantic_checkpoint(&mut self) -> lash_kernel_state::semantic::Checkpoint {
        self.try_semantic_checkpoint().expect("semantic checkpoint")
    }

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

fn operation_key(use_: &lash_kernel_value::names::OperationUse) -> Vec<u8> {
    let mut key = use_.link.as_bytes().to_vec();
    key.extend_from_slice(&lash_kernel_value::tokens::encode_body(&use_.slot));
    key
}

#[cfg(any(test, feature = "perf-witness"))]
impl KernelMachine {
    /// Begin a fresh physical execution census for this machine without
    /// selecting semantic observation or a different executor.
    pub fn count_execution(&mut self) {
        self.islands.census.enable();
        self.islands.counting = true;
    }

    /// Finish this machine's census and disable its producer, including
    /// when the run refused. Other machines' collection is independent.
    pub fn take_execution_census(&mut self) -> crate::perf_witness::execution::Census {
        self.islands.counting = false;
        self.islands.census.finish()
    }
}
