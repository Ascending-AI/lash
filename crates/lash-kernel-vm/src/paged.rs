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

use lash_kernel_doc::heap::PageValue;
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_doc::{Datum, DocumentId, Invalid, Name};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::{
    Checkpoint, CheckpointRoot, FunctionRecord, PageSource, SealBudget, SessionRoot,
};
use std::collections::{BTreeMap, BTreeSet};

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

/// What a session cell's end makes of the bindings its carry would drop
/// (`K-SES-003`): the functions among them its dialect saves
/// (`K-SES-004`). The embedder that knows the cell's dialect supplies it.
pub trait CellSave {
    /// Saves what it can of `cell.dropped` through `saving`.
    ///
    /// # Errors
    ///
    /// The heap's faults.
    fn save(
        &mut self,
        saving: &mut crate::CellSaveContext<'_>,
        cell: SaveCell<'_>,
    ) -> Result<SavedCell, HeapFault>;
}

/// A cell's end as its saving reads it.
pub struct SaveCell<'a> {
    pub document: &'a lash_kernel_doc::Document,
    /// The helper release the cell's code calls into.
    pub release: PageRef,
    /// The bindings the carry drops, and the session's bindings of the
    /// functions it already holds, by name.
    pub bindings: &'a BTreeMap<Name, PageValue>,
    /// The bindings the carry drops, in name order.
    pub dropped: &'a [Name],
    /// The declared names of the functions the session already holds.
    pub held: &'a BTreeSet<Name>,
}

/// What a cell's end saved.
#[derive(Default)]
pub struct SavedCell {
    /// Each saved function's record, by its declared name.
    pub functions: Vec<(Name, FunctionRecord)>,
    /// Each binding that held a saved function, bound to the function's
    /// value now: data, which carries.
    pub rebound: Vec<(Name, PageValue)>,
    /// Each dropped binding not saved, with why.
    pub refused: Vec<(Name, String)>,
}

/// A root a seal produced.
#[derive(Clone, Debug)]
pub struct SealedRoot {
    /// The sealed [`CheckpointRoot`] page.
    pub page: PageRef,
    /// Its decoded header, which the embedder's control record copies
    /// (kernel, document, release, owner, waits summary, meters).
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
    #[error("the registry has no function {function}, which the checkpoint pins")]
    MissingFunction {
        function: lash_kernel_doc::FunctionId,
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
    #[error("the root is not a checkpoint root of this format: {problem}")]
    Malformed { problem: String },
    /// The root's saved memory accounting is inconsistent.
    #[error("the accounting certificate is malformed: {problem}")]
    Accounting { problem: String },
    /// A page the open reads faulted.
    #[error(transparent)]
    Heap(HeapFault),
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
    /// reading the root page and nothing it does not need. `admissions`
    /// holds the request of every wait the root holds handed out. The run
    /// accounts its memory from the root's saved accounting, held to
    /// `bounds.memory`.
    fn open(
        program: Program,
        bounds: Bounds,
        root: &Checkpoint,
        admissions: Vec<AdmittedWait>,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError>;

    /// Selects the embedding policy before the segment runs.
    fn set_options(&mut self, options: crate::MachineOptions);
    /// Takes exact counts since the previous take or live observer delivery.
    fn take_counts(&mut self) -> crate::SiteCounts;
    fn take_path(&mut self) -> Result<crate::ExecutionPath, HeapFault>;

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
