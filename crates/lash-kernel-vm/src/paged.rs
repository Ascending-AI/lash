//! The machine's face after the persistent-heap cutover (FIG-6195, seams S1
//! and S4 of `docs/design/persistent-heap-seams.md`).
//!
//! Declarations only, called by nothing yet. At the cutover (layer hp-heap,
//! FIG-6188, with the integrator, FIG-6193) [`Machine`], [`MachineError`],
//! [`OpenError`], [`SealError`], [`CellStart`] and [`Finished`] replace
//! `interface::Machine` and its whole-state `export`/`import`, `Parked`,
//! `Bindings` and `ExportError`/`ImportError`, which are deleted.
//!
//! The machine executes directly on a [`SemanticHeap`]: a run opens a
//! sealed [`CheckpointRoot`] and faults in only the pages its next steps
//! touch; a park seals the changed pages and execution records into a new
//! root. Neither direction converts the whole state.

use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_doc::{Datum, DocumentId, Invalid, Name};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::{
    AllocationCensus, CheckpointRoot, PageSource, SealBudget, SessionRoot,
};

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
}

/// A root a seal produced.
#[derive(Clone, Debug)]
pub struct SealedRoot {
    /// The sealed [`CheckpointRoot`] page.
    pub page: PageRef,
    /// Its decoded header, which the embedder's control record copies
    /// (kernel, document, release, owner, waits summary, meters).
    pub root: CheckpointRoot,
}

/// A successful run's end after the cutover.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub result: Datum,
    pub finish: bool,
    /// The session carry the cell hands on, sealed (S8). `None` for an
    /// entry.
    pub carry: Option<SessionRoot>,
    /// The bindings not carried because they reach a closure or a task
    /// (`K-SES-003`).
    pub not_carried: Vec<Name>,
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
    /// executed nothing, its heap read and sealed through `source`.
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
    ) -> Result<Self, StartError>;

    /// Opens a sealed root against its document's prepared executable,
    /// reading the root page and nothing it does not need. `admissions`
    /// holds the request of every wait the root holds handed out.
    fn open(
        program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        admissions: Vec<AdmittedWait>,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
    ) -> Result<Self, OpenError>;

    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError>;

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError>;

    /// Seals the run as it stands at this safe point (`K-MACH-007`): the
    /// changed pages and execution records only, within `budget`; `None`
    /// when the budget ran out first (call again).
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError>;

    fn meters(&self) -> Meters;
}
