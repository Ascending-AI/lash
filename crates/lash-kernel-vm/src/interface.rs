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
//! `lash_kernel_doc::Nursery::reserve`: the function computes what it is
//! about to hold, reserves it against the room the bound leaves and only
//! then allocates. A refusal is `Retry::Room`: the machine collects the
//! heap, holds the room asked for and makes the call again, and a refusal
//! of that room ends the run with [`Bound::Memory`]. The accounting after the
//! call still runs for every function, and for one that reserved it
//! confirms room the reservation already took. A function whose result and
//! temporaries are no larger than a small fixed multiple of its arguments
//! (a copy, a concatenation, a case change at most three times its text)
//! reserves nothing: its arguments are live and inside the bound, so it can
//! pass the bound by that multiple at most before the accounting refuses
//! it. A larger multiple is reserved like an amplifier: `json.parse` holds
//! a tree node for every two bytes of `[0,0,...]`, and an integer's digits
//! in radix 2 are eight times its bytes.
//!
//! The laws: `the_memory_bound_refuses_a_native_reservation_before_the_allocation`
//! here pins the machine's side. The functions that must reserve are pinned
//! where they live: `an_amplifier_reserves_its_result_before_it_builds_it`,
//! `an_integer_reserves_its_digits_before_it_writes_them` and
//! `n_pow_reserves_each_product_before_it_multiplies` in
//! `lash-kernel-lib`, and
//! `a_result_the_heap_has_no_room_for_is_refused_at_its_reservation` in
//! `lash-ext-regex-ecma`.

use lash_kernel_doc::{EffectId, TaskLabel};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use crate::PreparedLibrary;
use lash_kernel_doc::{
    Datum, Document, EffectName, ErrorDatum, FunctionId, FunctionName, Handle, Invalid, Name,
    Timestamp, Type,
};
pub use lash_kernel_state::paged::{Charge, MemoryMeter};

/// What a machine runs: an admitted document and the library functions the
/// embedder registered, prepared once for every program that uses them.
/// The prepared library also shares each live document's admission and
/// executable across starts and imports. These are derived caches, never
/// saved. Replacing the document or library requires a fresh admission.
#[derive(Clone, Debug)]
pub struct Program {
    /// The exact helper release selected by composition.
    pub release: lash_kernel_doc::heap::PageId,
    pub document: Arc<Document>,
    pub library: PreparedLibrary,
}

/// The execution bounds and charge policy the embedder states (`K-BND-001`). Passing a bound ends
/// the run with [`RunError::Bound`]; no guest code catches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// Whether memory is metered. On is the default host policy; Off has
    /// no meter, no bound and no [`MemoryLimiter`] call.
    pub memory_meter: MemoryMeter,
    /// Whether charge is metered. Exact is the default host policy; Off has
    /// no meter, no bound and no [`ChargeLimiter`] call.
    pub charge: Charge,
    /// The most the run may be charged under Exact, in charge units, until
    /// its [`ChargeLimiter`] grows it.
    pub charge_limit: u64,
    /// The most heap the run may hold, in bytes as the machine accounts
    /// them, until its [`MemoryLimiter`] grows it.
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

/// What a limiter decides when a run reaches its limit (FIG-6768).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LimitDecision {
    /// Raise the limit by this much, and run on. A limit still reached
    /// after it is asked about again.
    Grow(u64),
    /// End the run with [`RunError::Bound`], as a run without a limiter
    /// ends.
    #[default]
    Refuse,
}

/// The host's decision at a run's memory bound (FIG-6768), shaped as
/// Wasmtime's `ResourceLimiter::memory_growing` (wasmtime 25.0.0,
/// `crates/wasmtime/src/runtime/limits.rs`): the embedder's callback where
/// a store's memory would pass its limit, which allows or refuses the growth.
///
/// The meter is a conservative bound, so the machine first recounts what
/// the run holds exactly and frees what nothing holds; it asks only when
/// the exact count with what the run asks for still does not fit. It asks
/// at an exact, deterministic point, before the statement's effect. A
/// grown limit is saved with the run at its next seal, so a resume
/// continues with it; a run resumed from an earlier seal reaches the same
/// point and asks again, with nothing observable done in between.
pub trait MemoryLimiter {
    /// The run needs `used` bytes and may hold `limit`.
    fn limit_reached(&mut self, used: u64, limit: u64) -> LimitDecision {
        let _ = (used, limit);
        LimitDecision::Refuse
    }
}

/// The host's decision at a run's charge bound (FIG-6768), shaped as
/// Wasmtime's `Store::epoch_deadline_callback` (wasmtime 25.0.0,
/// `crates/wasmtime/src/runtime/store.rs`), whose `UpdateDeadline::Continue`
/// extends the deadline by a number of ticks: [`LimitDecision::Grow`]
/// extends the bound by a number of charge units.
///
/// The machine asks where the charge passes the bound, at the unit the
/// reference charger crosses it, before the statement's effect: the
/// charge `used` includes the unit that crossed, as the refused run's
/// charge does. A grown limit is saved as [`MemoryLimiter`]'s is.
pub trait ChargeLimiter {
    /// The run is charged `used` and may be charged `limit`.
    fn limit_reached(&mut self, used: u64, limit: u64) -> LimitDecision {
        let _ = (used, limit);
        LimitDecision::Refuse
    }
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
    /// The effects and sleeps a run's parks admitted over its whole life,
    /// which its host's broker counts: the machine never passes it itself.
    Effects,
    /// The page bytes a run sealed into its checkpoints over its whole
    /// life, which its host's broker counts: the machine never passes it
    /// itself.
    CheckpointBytes,
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
    /// None when charge metering is Off.
    pub charged: Option<u64>,
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

/// How a run's `perform` of one effect runs (`K-EFF-012`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectMode {
    /// The `perform` waits: the embedder admits its request at the next
    /// park and delivers its committed outcome (`K-EFF-001`).
    Recorded,
    /// The `perform` is answered by the host at once, inside its statement,
    /// through [`Host::perform`]: no wait, no request, no turn given up.
    Read,
}

/// The mode each effect of a run is performed in, bound once when the run
/// starts and saved with its state: no later resume can change it
/// (`K-EFF-012`). Every effect it does not name as a read is recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectModes {
    reads: BTreeSet<EffectName>,
}

impl EffectModes {
    /// Every effect recorded.
    #[must_use]
    pub fn recorded() -> Self {
        Self::default()
    }

    /// `reads` performed as reads, every other effect recorded.
    #[must_use]
    pub fn reading(reads: impl IntoIterator<Item = EffectName>) -> Self {
        Self {
            reads: reads.into_iter().collect(),
        }
    }

    /// The mode `effect` is performed in.
    #[must_use]
    pub fn mode(&self, effect: &EffectName) -> EffectMode {
        if self.reads.contains(effect) {
            EffectMode::Read
        } else {
            EffectMode::Recorded
        }
    }

    /// The effects performed as reads, in name order.
    #[must_use]
    pub fn reads(&self) -> &BTreeSet<EffectName> {
        &self.reads
    }
}

/// The host's own failure to answer a read-bound `perform`: nothing about
/// the guest, which raises nothing. The run stops with
/// [`MachineError::Host`], and its last save stands.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{problem}")]
pub struct HostFault {
    pub problem: String,
}

impl HostFault {
    #[must_use]
    pub fn new(problem: impl Into<String>) -> Self {
        Self {
            problem: problem.into(),
        }
    }
}

/// Passive semantic observations of a running segment. Counts are additive;
/// a host read sees every start before that read, never the same start twice.
pub trait RunObserver {
    fn counts(&mut self, counts: &crate::SiteCounts);
}

/// Per-segment embedding policy. These options are derived, never checkpointed.
#[derive(Default)]
pub struct MachineOptions {
    pub observation: crate::ObservationMode,
    pub trace_path: bool,
    pub cell_save: Option<Box<dyn CellSave>>,
    /// Source captions available when a function crosses to a tool.
    pub captions: Vec<lash_kernel_doc::CaptionEntry>,
    /// What decides at the memory bound; none refuses.
    pub memory_limiter: Option<Box<dyn MemoryLimiter>>,
    /// What decides at the charge bound; none refuses.
    pub charge_limiter: Option<Box<dyn ChargeLimiter>>,
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
    /// Performs `effect`, which the run binds as a read, with `args` copied
    /// out of the run, for the `perform` at `site` under the authored
    /// statement `origin` (`K-EFF-012`, ORIGIN). The answer is the
    /// effect's result, decoded by the `perform`'s type, or the error
    /// raised at the `perform`, which the guest may catch. `Err` is the
    /// host's own fault: the run stops and nothing is raised. A host that
    /// binds no read is never asked.
    fn perform(
        &mut self,
        effect: &EffectName,
        args: &[Datum],
        site: &lash_kernel_doc::PositionLabel,
        origin: &lash_kernel_doc::Site,
    ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
        let _ = (args, site, origin);
        Err(HostFault::new(format!(
            "this host performs no read, and the run binds `{effect}` as one"
        )))
    }
    /// Whether the host takes [`Host::enter_site`]. A host that does not
    /// is never handed an entry, so the machine builds no identity for it.
    fn observes_sites(&self) -> bool {
        false
    }
    /// Passive entry into a document action. Library implementation sites
    /// are private; the document's call remains visible while they compute.
    fn enter_site(&mut self, _at: &EffectId) {}
    /// The passive semantic observer, when the host accepts live deltas.
    /// Reading this port never consumes counts unless an observer is present.
    fn observer(&mut self) -> Option<&mut dyn RunObserver> {
        None
    }
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
    pub identity: EffectId,
    pub effect: EffectName,
    /// The arguments, copied out of the run (`K-EFF-002`).
    pub args: Vec<Datum>,
    /// The type the `perform` decodes its result by.
    pub result: Type,
    /// The nearest authored statement the `perform` stands under
    /// (ORIGIN): its own statement in document code, the document
    /// statement that entered a library body. Derived when the request is
    /// reported, and no part of its identity.
    pub origin: lash_kernel_doc::Site,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SleepRequest {
    pub wait: WaitId,
    pub identity: EffectId,
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
    #[error("the machine handed out no wait {}", .wait)]
    UnknownWait { wait: WaitId },
    #[error("wait {} already has its outcome", .wait)]
    AlreadyDelivered { wait: WaitId },
    /// `Elapsed` for an effect, or an effect outcome for a sleep.
    #[error("the outcome is not one wait {} can have", .wait)]
    WrongOutcome { wait: WaitId },
    #[error("the run has ended")]
    Ended,
    #[error(transparent)]
    Heap(#[from] lash_kernel_doc::heap::HeapFault),
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
    /// can inspect the kernel's end condition independently of live tasks.
    pub outstanding: u64,
    /// Every task other than `main` that has not ended, including passed
    /// list-join members (`K-TASK-018` exempts those from `outstanding`).
    /// A turn control must wait for these tasks to settle too.
    pub live: u64,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    Finished(Box<Finished>),
    Failed(Datum),
    Error(RunError),
    Cancelled,
}

/// A run that ended in an error (`K-TASK-018`, `K-BND-001`).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RunError {
    /// A value no `catch` took ended `main`, copied out unchanged.
    #[error("uncaught {}", uncaught_text(.error))]
    Uncaught {
        error: Datum,
        /// Where `main` raised it, in document code: the wait action
        /// (`perform`, `sleep`, `join`) whose outcome raised, else the
        /// statement whose evaluation raised, a library call's included.
        /// A raise out of library code names the first document statement
        /// it unwinds into, and one a `finally` held names its `try`
        /// statement. `None` when a run-root cleanup held it, or no
        /// document statement was left to name it.
        site: Option<lash_kernel_doc::Site>,
    },
    /// The run reached its end with tasks that were unfinished, or that
    /// ended in an error no `join` observed.
    #[error("the run ended with {} unfinished task(s) and {} unobserved task error(s)", .unfinished.len(), .unobserved.len())]
    TasksOutstanding {
        unfinished: Vec<TaskLabel>,
        unobserved: Vec<TaskLabel>,
        /// The outermost document body still running in each unfinished task.
        /// Derived from semantic frames, separate from the actual spawn identity.
        /// An embedder resolves these sites through its validated source map.
        sources: Vec<(TaskLabel, lash_kernel_doc::PositionLabel)>,
    },
    /// No task is ready and none waits on anything the embedder can
    /// answer.
    #[error("{} task(s) wait on each other and nothing can wake them", .waiting.len())]
    Deadlock { waiting: Vec<TaskLabel> },
    #[error(transparent)]
    Bound(#[from] BoundExceeded),
}

/// An uncaught value as plain text: an error's kind and message, never a
/// debug dump of the value.
fn uncaught_text(error: &Datum) -> String {
    match error {
        Datum::Error(error) => format!("{}: {}", error.kind, error.message),
        _ => "a value that is not an error".to_owned(),
    }
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
    /// The host's name list cannot fit every checkpoint root's page.
    #[error("host-binding metadata of {actual} encoded bytes exceeds the {limit}-byte limit")]
    HostMetadataTooLarge { limit: usize, actual: usize },
    #[error("the bindings are not a session's: {problem}")]
    Bindings { problem: String },
    /// The run binds as a read an effect its document's manifest does not
    /// declare.
    #[error("the run binds `{effect}` as a read, and the manifest declares no such effect")]
    UndeclaredRead { effect: EffectName },
    /// The session's saved memory accounting is inconsistent.
    #[error("the accounting certificate is malformed: {problem}")]
    Accounting { problem: String },
    #[error(transparent)]
    Heap(#[from] lash_kernel_doc::heap::HeapFault),
}

pub use crate::paged::{
    CellSave, CellStart, Finished, Machine, MachineError, OpenError, SaveCell, SavedCell, SealError,
};
