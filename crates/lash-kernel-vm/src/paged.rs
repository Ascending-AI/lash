//! The machine's face after the persistent-heap cutover (FIG-6195, seams S1
//! and S4 of `docs/design/persistent-heap-seams.md`).
//!
//! The paged KernelMachine implements this interface. Its checkpoint root
//! names persistent task, frame, queue, wait, binding and function tables.
//!
//! The machine executes directly on a [`SemanticHeap`]: a run opens a
//! sealed [`CheckpointRoot`] and faults in only the pages its next steps
//! touch; a park seals the changed pages and execution records into a new
//! root. Neither direction converts the whole state.

use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_doc::{Datum, Invalid};

use lash_kernel_state::paged::{Checkpoint, CheckpointRoot, PageSource, SealBudget, SessionRoot};
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::names::Name;

use crate::interface::{
    Bounds, DeliverError, Delivered, EffectModes, Host, Meters, Outcome, Program, StartError, Step,
    Target, WaitId,
};

/// How a new run starts.
#[derive(Clone, Debug)]
pub struct CellStart {
    /// The exact activation owner binding supplied by composition.
    pub owner: lash_kernel_doc::heap::PageId,
    pub target: Target,
    /// The entry's arguments. Empty for `main`.
    pub args: Vec<Datum>,
    /// The carried session a cell's `main` reads and writes (`K-SES-001`),
    /// opened in place: the same identities, nothing renamed or loaded.
    pub session: Option<SessionRoot>,
    pub modes: EffectModes,
    /// The host's read-only bindings a cell's `main` starts with over the
    /// session's: bound at the start, and left out of the cell's carry.
    /// Its name list must fit [`CheckpointRoot::HOST_METADATA_LIMIT`],
    /// including token framing; start refuses it before opening the heap.
    pub host: Vec<(Name, Datum)>,
}

impl CellStart {
    /// Admit the host's names before opening a heap or starting a worker.
    ///
    /// # Errors
    ///
    /// [`StartError::HostMetadataTooLarge`] when the encoded name list
    /// exceeds the space reserved in every checkpoint root.
    pub fn admit_host_metadata<'a>(
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), StartError> {
        let actual = CheckpointRoot::host_metadata_bytes(names);
        let limit = CheckpointRoot::HOST_METADATA_LIMIT;
        if actual > limit {
            return Err(StartError::HostMetadataTooLarge { limit, actual });
        }
        Ok(())
    }
}

/// A root a seal produced.
#[derive(Clone, Debug)]
pub struct SealedRoot {
    /// The sealed [`CheckpointRoot`] page.
    pub page: PageRef,
    /// Its decoded header, which the embedder's control record copies
    /// (kernel, document, closure, owner, waits summary, meters).
    pub(crate) root: CheckpointRoot,
}

impl SealedRoot {
    /// Opaque state for reopening this checkpoint.
    pub fn checkpoint(&self) -> Checkpoint {
        lash_kernel_state::kernel_internals::checkpoint(self.root.clone())
    }
}

/// A successful run's end after the cutover.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub result: Datum,
    pub finish: bool,
    /// The session carry the cell hands on, sealed (S8). `None` for an
    /// entry.
    pub carry: Option<SessionRoot>,
    /// The bindings not carried, each with why (`K-SES-003`): one that
    /// reaches a task, or a function its dialect does not save.
    pub not_carried: Vec<(Name, String)>,
}

/// A call the machine cannot serve.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MachineError {
    #[error("the run has ended")]
    Ended,
    /// A seal ran out of budget mid-projection: the run runs again once a
    /// seal completes the projection (seam G).
    #[error("a projection is incomplete; seal again before running")]
    Projecting,
    #[error("the machine is in a state it cannot run from: {problem}")]
    Fault { problem: String },
    #[error("the host failed a read: {0}")]
    Host(crate::interface::HostFault),
    /// Heap access failed (S3): the activation ends as infrastructure and
    /// resumes from its last committed root. Never a guest error.
    #[error(transparent)]
    Heap(HeapFault),
}

/// A root the machine refuses to open.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    #[error("build helper {function} is unavailable: {reason}")]
    HelperUnavailable {
        reason: lash_kernel_value::HelperUnavailableReason,
        function: lash_kernel_value::FunctionId,
    },
    #[error("the registry has no function {function}, which the checkpoint pins")]
    MissingFunction {
        function: lash_kernel_value::names::FunctionId,
    },
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
    /// The root's envelope names a parked-state format this build does not
    /// read (contract §9): refused before any page is read, never
    /// reinterpreted.
    #[error("the state was parked under {surface} {observed}; this build reads {expected}")]
    Format {
        surface: &'static str,
        expected: lash_kernel_state::paged::FormatValue,
        observed: lash_kernel_state::paged::FormatValue,
    },
    #[error("the root is not a checkpoint root of this format: {problem}")]
    Malformed { problem: String },
    /// The root's saved memory accounting is inconsistent.
    #[error("the accounting certificate is malformed: {problem}")]
    Accounting { problem: String },
    /// The sealed run stands outside the admission relation (contract §5).
    #[error("the parked run is not a state the machine reaches: {0}")]
    Inadmissible(#[from] Inadmissible),
    /// A page the open reads faulted.
    #[error(transparent)]
    Heap(HeapFault),
}

/// A sealed run outside the admission relation (contract §5): each names
/// the one rule the run breaks. Open checks the whole relation before it
/// restores anything.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Inadmissible {
    /// A wait performs an operation the run binds as a read (`K-EFF-012`).
    #[error("wait {wait} performs an operation the run binds as a read")]
    ReadBoundWait { wait: u64 },
    /// One wait number is held twice: an outcome delivered twice.
    #[error("wait {wait} is held twice")]
    DuplicateWait { wait: u64 },
    /// A wait numbered at or past the next one, or one withdrawn.
    #[error("wait {wait} was never handed out, or was withdrawn")]
    WaitNumber { wait: u64 },
    /// A wait no task waits on or resumes with: an orphan, a second wait
    /// of one task, or a consumed receipt held as outstanding.
    #[error("no task waits on or resumes with wait {wait}")]
    OrphanWait { wait: u64 },
    /// A task waits on or resumes with a wait the run does not hold in
    /// the phase its state needs: requested or handed over while it
    /// waits, delivered while it resumes.
    #[error("task {task} needs wait {wait}, which the run does not hold in that phase")]
    MissingWait { task: u32, wait: u64 },
    /// A wait's task does not stand issued at the `perform` it names.
    #[error("wait {wait}'s task does not stand issued at its perform")]
    WaitPoint { wait: u64 },
    /// A wait requested since the last park is not listed as issued, or
    /// one listed is not requested.
    #[error("wait {wait} is listed as issued without its request, or the reverse")]
    Issued { wait: u64 },
    /// A delivered failure that holds no error.
    #[error("delivered wait {wait} failed with no error")]
    Outcome { wait: u64 },
    /// The ready queue holds a task twice.
    #[error("task {task} is in the ready queue twice")]
    ReadyDuplicate { task: u32 },
    /// A queued task that is not ready, or a ready task not queued.
    #[error("task {task} is queued and not ready, or ready and not queued")]
    Queue { task: u32 },
    /// A frame under another stands somewhere other than its call.
    #[error("frame {frame} of task {task} is suspended but not at a call")]
    SuspendedCall { task: u32, frame: u32 },
    /// A frame's bill reaches outside its task's billed values, or below
    /// the bill of a frame it called.
    #[error("frame {frame} of task {task} owes a bill outside its task's billed values")]
    Bill { task: u32, frame: u32 },
    /// A closure body's frame runs no closure, or another frame runs one.
    #[error(
        "frame {frame} of task {task} lacks the closure its unit runs, or names one it does not"
    )]
    Captures { task: u32, frame: u32 },
    /// A task's outermost frame is not the unit it starts in: the run's
    /// start unit for the first task, a spawn stub for the others.
    #[error("task {task}'s outermost frame is not where it starts")]
    Root { task: u32 },
    /// A spawned task before its parent, or `main` other than first.
    #[error("task {task} stands before its parent in spawn order")]
    SpawnOrder { task: u32 },
    /// A task's count at a site is at or below an identity it minted there.
    #[error("task {task}'s count at an action is below an identity it minted")]
    Counter { task: u32 },
    /// A task's join or joiners name a task or list join that does not
    /// name it back, or an ended task.
    #[error("task {task}'s joins do not agree with the tasks they name")]
    Join { task: u32 },
    /// A list join whose joiner or members disagree with it.
    #[error("list join {join} does not agree with its joiner and members")]
    ListJoin { join: u64 },
    /// An ended task that still holds frames.
    #[error("task {task} has ended but holds frames")]
    Ended { task: u32 },
    /// The installed task is not the one running, or another runs.
    #[error("task {task} runs but is not the installed task")]
    Running { task: u32 },
    /// The live count disagrees with the tasks that have not ended.
    #[error("the run counts {saved} live tasks; {counted} have not ended")]
    Live { saved: u32, counted: u32 },
}

/// Refuses `root` when its envelope states a format this build does not
/// read; the first check of every open, ahead of any page read.
pub(crate) fn admit_format(root: &Checkpoint) -> Result<(), OpenError> {
    match root.foreign_format() {
        None => Ok(()),
        Some(format) => Err(OpenError::Format {
            surface: format.surface,
            expected: format.expected.clone(),
            observed: format.observed.clone(),
        }),
    }
}

/// Why a seal did not produce a root.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealError {
    #[error("the run has ended; there is nothing to park")]
    Ended,
    #[error("the machine is in a state it cannot write: {problem}")]
    Fault { problem: String },
    #[error(transparent)]
    Heap(HeapFault),
}

/// A kernel machine over a paged heap: one run of one document.
///
/// Implemented by `KernelMachine` (layer hp-heap, FIG-6188).
pub trait Machine: Sized {
    /// Admits and prepares `program`'s document and creates a run that has
    /// executed nothing, its heap read and sealed through `source`. The run
    /// accounts its memory from the session's saved accounting (a fresh
    /// heap's without a session), held to `bounds.memory`.
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError>;

    /// Opens a sealed root against its document's prepared executable,
    /// reading the root page and nothing it does not need. The root's waits
    /// carry every request it handed out. The run accounts its memory from
    /// the root's saved accounting, held to `bounds.memory`.
    fn open(
        program: Program,
        bounds: Bounds,
        root: &Checkpoint,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError>;

    /// Selects the embedding policy before the segment runs.
    fn set_options(&mut self, options: crate::MachineOptions);
    /// Takes exact counts since the previous take or live observer delivery.
    fn take_counts(&mut self) -> crate::SiteCounts;
    fn take_path(&mut self) -> Result<crate::ExecutionPath, HeapFault>;
    /// Takes the waits whose failed outcome a `catch` recovered from since
    /// the previous take, in order (FIG-6817). Taken at a park or an end:
    /// a catch block still running there has recovered.
    fn take_caught(&mut self) -> Vec<WaitId>;

    /// Exact slices count charge units; Off slices count machine steps.
    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError>;

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError>;

    /// Seals the run as it stands at this safe point (`K-MACH-007`): the
    /// changed pages and execution records only, within `budget`; `None`
    /// when the budget ran out first (call again).
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError>;

    /// Seals the run whole at this safe point: [`Machine::seal`] with no
    /// budget, until it produces the root.
    ///
    /// # Errors
    ///
    /// [`Machine::seal`]'s.
    fn checkpoint(&mut self) -> Result<SealedRoot, SealError> {
        loop {
            if let Some(sealed) = self.seal(SealBudget {
                pages: u64::MAX,
                bytes: u64::MAX,
            })? {
                return Ok(sealed);
            }
        }
    }

    fn meters(&self) -> Meters;
}

/// Validates every continuation in a foreign image before its owner publishes it.
/// No guest step or host effect runs during this admission.
pub fn validate_foreign_checkpoint(
    program: Program,
    bounds: crate::Bounds,
    checkpoint: &lash_kernel_state::paged::Checkpoint,
    source: Box<dyn PageSource>,
) -> Result<(), OpenError> {
    use crate::Machine as _;
    // The core's open checks the admission relation and the live resume
    // closure before it returns (contract §5, §7).
    crate::KernelMachine::open(program, bounds, checkpoint, source).map(drop)
}
