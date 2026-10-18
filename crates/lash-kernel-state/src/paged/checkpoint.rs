//! Opaque parked state and the small control facts an embedding may inspect.
use super::{CheckpointRoot, PageAnswer, RootStatus};
use lash_kernel_doc::{
    DocumentId,
    graph_tokens::{TokenCodec, TokenError, TokenReader, TokenWriter},
    heap::{HeapFault, PageId},
};

/// An authenticated checkpoint opened from its root page. Execution records
/// remain owned by the kernel; hosts retain and transport the page identity.
#[derive(Clone, Debug)]
pub struct Checkpoint(pub(crate) CheckpointRoot);

impl Checkpoint {
    /// Opens the opaque state and authorizes its references from a page answer.
    pub fn of_answer(page: &PageAnswer) -> Result<Self, HeapFault> {
        CheckpointRoot::of_answer(page).map(Self)
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
    pub release: PageId,
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
        release: root.release,
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
        CheckpointRoot::decode(input).map(Self)
    }
}
