//! Opaque parked state and the small control facts an embedding may inspect.
use super::{CheckpointRoot, PageAnswer, RootStatus};
use lash_kernel_doc::heap::{HeapFault, PageId};
use {
    lash_kernel_value::names::DocumentId, lash_kernel_value::tokens::TokenCodec,
    lash_kernel_value::tokens::TokenError, lash_kernel_value::tokens::TokenReader,
    lash_kernel_value::tokens::TokenWriter,
};

/// An authenticated checkpoint opened from its root page. Execution records
/// remain owned by the kernel; hosts retain and transport the page identity.
///
/// A host that has not matched the root's envelope against its own format
/// states the format the envelope names ([`Checkpoint::stated`]); an open
/// refuses a root stated under another format before it reads a page
/// (contract §9).
#[derive(Clone, Debug)]
pub struct Checkpoint(pub(crate) CheckpointRoot, pub(crate) Option<StatedFormat>);

/// The parked-state format a root's envelope names (`observed`) against the
/// format the opening build reads (`expected`), on the witness `surface`.
/// The witnesses live with the embedding, outside the kernel, so the host
/// states both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatedFormat {
    pub surface: &'static str,
    pub expected: FormatValue,
    pub observed: FormatValue,
}

/// A format witness's value: a version number, or a domain string such as
/// a page digest domain, which changes every page's identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum FormatValue {
    Number(u32),
    Text(std::sync::Arc<str>),
}

impl std::fmt::Display for FormatValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::Text(text) => write!(f, "{text:?}"),
        }
    }
}

impl Checkpoint {
    /// Opens the opaque state and authorizes its references from a page answer.
    pub fn of_answer(page: &PageAnswer) -> Result<Self, HeapFault> {
        CheckpointRoot::of_answer(page).map(|root| Self(root, None))
    }
    /// This checkpoint, its envelope stating `format`.
    pub fn stated(self, format: StatedFormat) -> Self {
        Self(self.0, Some(format))
    }
    /// The stated format, when it is not the one the opening build reads.
    pub fn foreign_format(&self) -> Option<&StatedFormat> {
        self.1
            .as_ref()
            .filter(|format| format.observed != format.expected)
    }
    /// Whether both checkpoints retain the same authenticated pending-wait set.
    /// This compares the saved set without exposing its storage coordinates.
    pub fn same_pending_waits(&self, other: &Self) -> bool {
        self.0.waits == other.0.waits
    }
    pub fn header(&self) -> CheckpointHeader {
        header(&self.0)
    }
}

/// Control facts only: no running representation, queues or table roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointHeader {
    /// The limits the run continues with.
    pub limits: super::SavedLimits,
    pub kernel: u32,
    pub document: DocumentId,
    /// The authenticated linked program whose main frames may continue.
    pub link: lash_kernel_value::LinkId,
    pub closure: PageId,
    pub helper_library: lash_kernel_value::HelperLibraryId,
    pub owner: PageId,
    pub parked: bool,
    pub charged: Option<u64>,
    pub live_tasks: u32,
    pub pending_waits: u64,
}

fn header(root: &CheckpointRoot) -> CheckpointHeader {
    CheckpointHeader {
        limits: root.limits,
        kernel: root.kernel,
        document: root.document,
        link: root.link,
        closure: root.closure,
        helper_library: root.helper_library,
        owner: root.owner,
        parked: matches!(root.status, RootStatus::Parked),
        charged: root.meters.charged,
        live_tasks: root.meters.live_tasks,
        pending_waits: root.waits.len(),
    }
}

/// Inspects one authenticated root page without opening an execution heap.
pub fn inspect_checkpoint(page: &PageAnswer) -> Result<CheckpointHeader, HeapFault> {
    CheckpointRoot::of_answer(page).map(|root| header(&root))
}

/// The opaque handle retains the exact baseline encoding.
impl TokenCodec for Checkpoint {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        CheckpointRoot::decode(input).map(|root| Self(root, None))
    }
}
