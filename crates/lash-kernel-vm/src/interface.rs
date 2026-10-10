//! The embedder-facing interface of the machine.
//!
//! # Memory a native function allocates
//!
//! [`Bounds::memory`] bounds what the machine accounts, and the machine
//! accounts a library call's result when the call returns (`K-CHG-003`). A
//! native function runs to its end inside one machine step: the machine
//! asks [`Host::cancel_requested`] only between slices, so neither a
//! deadline nor a cancel can stop it, and the charge for the call is taken
//! after it. A function whose result follows a count, a product of two
//! sizes or shared structure (`text.repeat`, `text.replace`,
//! `json.stringify`) can therefore ask the allocator for far more than the
//! bound before the machine sees a byte of it. A guard (`K-LIB-008`) does
//! not help: it counts work, and its limit is in the function's identity,
//! not in the embedder's bounds.
//!
//! The check belongs to the native function, at
//! `lash_kernel_doc::NativeHeap::reserve`: the function computes what it is
//! about to hold, reserves it against the room the bound leaves and only
//! then allocates. The machine answers a refusal as it answers any refused
//! allocation: it collects the heap and makes the call again, and a second
//! refusal ends the run with [`Bound::Memory`]. The accounting after the
//! call still runs for every function, and for one that reserved it
//! confirms room the reservation already took. A function whose result is
//! no larger than a fixed multiple of its arguments reserves nothing: its
//! arguments are live and inside the bound, so it can pass the bound by
//! that multiple at most before the accounting refuses it.
//!
//! The laws: `the_memory_bound_refuses_a_native_reservation_before_the_allocation`
//! here pins the machine's side. The functions that must reserve are pinned
//! where they live: `an_amplifier_reserves_its_result_before_it_builds_it`
//! and `n_pow_reserves_each_product_before_it_multiplies` in
//! `lash-kernel-lib`, and
//! `a_result_the_heap_has_no_room_for_is_refused_at_its_reservation` in
//! `lash-ext-regex-ecma`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash_kernel_doc::{
    Datum, Document, DocumentId, EffectIdentity, EffectName, ErrorDatum, FunctionId, FunctionName,
    FunctionRegistry, Handle, Invalid, Name, Object, ObjectId, TaskIdentity, Timestamp, Type,
    Value,
};

/// What a machine runs: an admitted document and the library functions the
/// embedder registered. The machine compiles it to an executable of its
/// own, which is a cache: derived deterministically, never saved.
#[derive(Clone, Debug)]
pub struct Program {
    pub document: Arc<Document>,
    pub registry: Arc<FunctionRegistry>,
}

/// The execution bounds the embedder states (`K-BND-001`). Passing one ends
/// the run with [`RunError::Bound`]; no guest code catches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// The most the run may be charged, in charge units.
    pub charge: u64,
    /// The most heap the run may hold, in bytes as the machine accounts
    /// them.
    pub memory: u64,
    /// The deepest any task's active calls may nest.
    pub call_depth: u32,
    /// The most tasks that may be live at once, `main` included.
    pub live_tasks: u32,
    /// The most effects and sleeps one park may request.
    pub requests_per_park: u32,
    /// The most members one `join` on a list may have.
    pub join_members: u32,
}

/// Which bound a run passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    Charge,
    Memory,
    CallDepth,
    LiveTasks,
    RequestsPerPark,
    JoinMembers,
    /// A native function's guard (`K-LIB-008`).
    Guard {
        function: FunctionId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the run passed its {bound:?} bound of {limit}")]
pub struct BoundExceeded {
    pub bound: Bound,
    pub limit: u64,
    /// The definition name of the library call that passed the bound, when
    /// it arose in a library call rather than in the program itself.
    pub function: Option<FunctionName>,
}

/// What a run has used so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meters {
    pub charged: u64,
    pub memory: u64,
    pub live_tasks: u32,
}

/// What a new run executes.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// The document's `main`, as a session cell.
    Main,
    /// One of the document's entries, by name.
    Entry(Name),
}

/// How a new run starts.
#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    pub target: Target,
    /// The entry's arguments, decoded by the entry's signature. Empty for
    /// `main`.
    pub args: Vec<Datum>,
    /// The session's bindings, which `main` reads and writes (`K-SES-001`).
    /// Empty for an entry.
    pub bindings: Bindings,
}

/// A session's bindings as data: each variable's value, and the heap
/// objects those values reach, with their identities, so that two bindings
/// that share an object still share it in the next cell.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bindings {
    pub variables: BTreeMap<Name, Value>,
    pub objects: BTreeMap<ObjectId, Object>,
}

/// The synchronous interface a machine reads its host through while it
/// runs. Every call is answered at once: none is a wait, none yields the
/// running task's turn, and none is saved, so a read in a stretch that was
/// not saved is drawn again after a crash (`K-HOST-001`).
pub trait Host {
    /// The embedder's clock.
    fn clock(&mut self) -> Timestamp;
    /// Sixty-four uniformly random bits (`K-HOST-003`).
    fn random(&mut self) -> u64;
    /// Reads through a projection handle. The answer is kernel data; an
    /// error is raised in the guest at the read.
    fn read(&mut self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum>;
    /// Takes the output of one `print`.
    fn print(&mut self, value: &Datum);
    /// Whether the embedder wants the run cancelled. The machine asks at
    /// every fuel-slice boundary and at every park (`K-MACH-006`).
    fn cancel_requested(&mut self) -> bool;
}

pub use lash_kernel_state::WaitId;

/// Something a task asked for and waits on.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Effect(EffectRequest),
    Sleep(SleepRequest),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectRequest {
    pub wait: WaitId,
    pub identity: EffectIdentity,
    pub effect: EffectName,
    /// The arguments, copied out of the run (`K-EFF-002`).
    pub args: Vec<Datum>,
    /// The type the `perform` decodes its result by.
    pub result: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SleepRequest {
    pub wait: WaitId,
    pub identity: EffectIdentity,
    pub duration: Duration,
}

/// How a wait ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The effect returned this. Its numbers may be undecoded tokens; the
    /// machine decodes them by the `perform`'s result type (`K-EFF-005`).
    Completed(Datum),
    /// The effect failed. The error is raised at the `perform`.
    Failed(ErrorDatum),
    /// The sleep is over.
    Elapsed,
}

/// What a delivery did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivered {
    /// The waiting task is ready, at the back of the queue.
    Accepted,
    /// The wait was withdrawn (its task was cancelled or the run moved past
    /// it); the outcome is dropped.
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeliverError {
    #[error("the machine handed out no wait {}", .wait.0)]
    UnknownWait { wait: WaitId },
    #[error("wait {} already has its outcome", .wait.0)]
    AlreadyDelivered { wait: WaitId },
    /// `Elapsed` for an effect, or an effect outcome for a sleep.
    #[error("the outcome is not one wait {} can have", .wait.0)]
    WrongOutcome { wait: WaitId },
    #[error("the run has ended")]
    Ended,
}

/// Where [`Machine::run`] stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// No task is ready. The embedder admits the requests with the saved
    /// state, in one transaction, and delivers outcomes as they commit.
    Parked(Park),
    /// The fuel slice is spent and a task is still ready. The embedder may
    /// save here; `run` again continues.
    Slice,
    /// The run is over.
    Ended(End),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Park {
    /// Every effect and sleep requested since the last park, in request
    /// order.
    pub requests: Vec<Request>,
    /// Every wait withdrawn since the last park. The embedder releases what
    /// it holds for each; a later outcome for one is dropped.
    pub withdrawn: Vec<WaitId>,
    /// The tasks other than `main` that would make the run's end a
    /// `TasksOutstanding` error if it ended now (`K-TASK-018`): one that has
    /// not ended and was never a member of a list `join` that returned or
    /// raised, and one that ended in an error nothing observed. An embedder
    /// reads it to refuse an effect that would end the run.
    pub outstanding: Vec<TaskIdentity>,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    /// `main` or the entry returned, or a `finish` ran.
    Finished(Finished),
    /// A `fail` ran, with this reason.
    Failed(Datum),
    Error(RunError),
    /// The embedder's cancel was observed. No cleanup block ran.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub result: Datum,
    /// Whether a `finish` ended the run. Otherwise `main` or the entry
    /// returned, and `result` is what it returned.
    pub finish: bool,
    /// The session's bindings after `main`. Empty for an entry.
    pub bindings: Bindings,
    /// The session bindings that were not carried because they reach a
    /// closure or a task handle (`K-SES-003`).
    pub not_carried: Vec<Name>,
    /// Those of `not_carried` that reach no task handle, as the run left
    /// them: each one's value and the objects it reaches, with every
    /// closure as the expression it was made from and the variables it
    /// shares. No later run starts from these; an embedder reads them to
    /// keep a function as a value of its own.
    pub closures: Bindings,
}

/// A run that ended in an error (`K-TASK-018`, `K-BND-001`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RunError {
    /// A value no `catch` took ended `main`, copied out unchanged.
    #[error("uncaught {0:?}")]
    Uncaught(Datum),
    /// The run reached its end with tasks that were unfinished, or that
    /// ended in an error no `join` observed.
    #[error("the run ended with {} unfinished task(s) and {} unobserved task error(s)", .unfinished.len(), .unobserved.len())]
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    /// No task is ready and none waits on anything the embedder can
    /// answer.
    #[error("{} task(s) wait on each other and nothing can wake them", .waiting.len())]
    Deadlock { waiting: Vec<TaskIdentity> },
    #[error(transparent)]
    Bound(#[from] BoundExceeded),
}

/// A program or a start the machine refuses.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    #[error("the document is not admitted: {0}")]
    NotAdmitted(#[from] Invalid),
    #[error("the registry has no function {function}, which the manifest lists")]
    MissingFunction { function: FunctionId },
    #[error("the document has no entry `{entry}`")]
    UnknownEntry { entry: Name },
    #[error("the arguments do not fit the entry's signature: {problem}")]
    Arguments { problem: String },
    #[error("the bindings are not a session's: {problem}")]
    Bindings { problem: String },
}

/// A call the machine cannot serve in its present state.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MachineError {
    #[error("the run has ended")]
    Ended,
    /// The machine found its own state inconsistent: a defect in the
    /// machine, never in the document. The run is over.
    #[error("the machine is in a state it cannot run from: {problem}")]
    Fault { problem: String },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    #[error("the run has ended; there is nothing to park")]
    Ended,
    /// The machine found its own state inconsistent: a defect in the
    /// machine, never in the document.
    #[error("the machine is in a state it cannot write: {problem}")]
    Fault { problem: String },
}

/// A parked state the machine refuses to resume.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    #[error("the document is not admitted: {0}")]
    NotAdmitted(#[from] Invalid),
    #[error("the state was parked under document {parked}, not {given}")]
    DocumentMismatch {
        parked: DocumentId,
        given: DocumentId,
    },
    #[error(
        "the state was parked under kernel version {parked}; the document is written for {supported}"
    )]
    KernelVersion { parked: u32, supported: u32 },
    #[error("the registry has no function {function}, which the parked run pins")]
    MissingFunction { function: FunctionId },
    #[error("the parked state is malformed: {problem}")]
    Malformed { problem: String },
}

/// A kernel machine: one run of one document.
///
/// An embedder drives it in a loop: [`run`](Self::run) until it parks,
/// commit the park, [`deliver`](Self::deliver) outcomes as they commit, and
/// `run` again. Between calls the run is at a safe point: every task is
/// between statements, and [`export`](Self::export) may be called.
///
/// The conformance harness runs its corpus against any implementation of
/// this trait.
pub trait Machine: Sized {
    /// The parked state this machine writes and reads: for
    /// [`KernelMachine`](crate::KernelMachine), the `ParkedRun` of
    /// `lash-kernel-state`.
    type Parked;

    /// Compiles `program` and creates a run that has executed nothing.
    fn start(program: Program, bounds: Bounds, start: Start) -> Result<Self, StartError>;

    /// Runs ready tasks, first in first out, until no task is ready, the
    /// run ends, or `slice` charge units are spent in this call
    /// (`K-MACH-002`). A slice of `u64::MAX` never ends early.
    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError>;

    /// Hands the machine one committed outcome. Outcomes may be delivered
    /// in any order, and several before the next `run` (`K-MACH-004`).
    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError>;

    /// Writes the run's state as it stands at this safe point
    /// (`K-MACH-007`).
    fn export(&mut self) -> Result<Self::Parked, ExportError>;

    /// Rebuilds a run from a parked state. The executable is compiled
    /// afresh; resuming continues the computation and its effect ownership
    /// exactly (`K-MACH-008`).
    fn import(program: Program, bounds: Bounds, parked: Self::Parked) -> Result<Self, ImportError>;

    /// What the run has used so far.
    fn meters(&self) -> Meters;
}
