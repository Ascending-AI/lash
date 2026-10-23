//! Opaque parked state and the small control facts an embedding may inspect.
use super::{CheckpointRoot, PageAnswer, RootStatus};
use lash_kernel_doc::heap::{HeapFault, PageId};
use lash_vocabulary::{
    names::DocumentId,
    tokens::{TokenCodec, TokenError, TokenReader, TokenWriter},
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
    pub closure: PageId,
    pub helper_library: lash_vocabulary::HelperLibraryId,
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
        CheckpointRoot::decode(input).map(Self)
    }
}

/// Exact helper identities owed by the retained continuation and callable values.
/// Produced only on a library change; never stored as executable definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelperPins(pub std::collections::BTreeSet<lash_vocabulary::FunctionId>);

/// A helper refusal stays typed until the embedding maps its protocol surface.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelperCompatibilityError {
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error("build helper {function} is missing")]
    MissingHelper {
        function: lash_vocabulary::FunctionId,
    },
}

/// Compare before asking for any live-code facts. Equal images require no
/// heap reads, helper preparation, dependency discovery or membership probes.
pub fn check_helper_compatibility(
    parked_id: lash_vocabulary::HelperLibraryId,
    pins: impl FnOnce() -> Result<HelperPins, HeapFault>,
    build: &lash_kernel_doc::BuildHelpers,
) -> Result<(), HelperCompatibilityError> {
    #[cfg(any(test, feature = "perf-witness"))]
    helper_compatibility_work::update(|work| work.comparisons += 1);
    if parked_id == build.identity() {
        return Ok(());
    }
    check_pins(pins()?.0, build)
}

fn check_pins(
    pins: impl IntoIterator<Item = lash_vocabulary::FunctionId>,
    build: &lash_kernel_doc::BuildHelpers,
) -> Result<(), HelperCompatibilityError> {
    for function in pins {
        #[cfg(any(test, feature = "perf-witness"))]
        helper_compatibility_work::update(|work| work.membership_probes += 1);
        if !build.contains(&function) {
            return Err(HelperCompatibilityError::MissingHelper { function });
        }
    }
    Ok(())
}

impl Checkpoint {
    /// Establish compatibility before assembling an executable or rewriting
    /// carried state. On a library change only, walk authenticated live roots
    /// once and expand their static reach using the immutable installed index.
    /// The original page port is returned for subsequent execution.
    pub fn check_helper_compatibility(
        &self,
        source: Box<dyn super::PageSource>,
        documents: &std::collections::BTreeMap<lash_vocabulary::LinkId, lash_kernel_doc::Document>,
        code_imports: &std::collections::BTreeMap<
            (lash_vocabulary::LinkId, lash_vocabulary::ImportSlot),
            lash_vocabulary::CallableId,
        >,
        build: &lash_kernel_doc::BuildHelpers,
    ) -> Result<(Box<dyn super::PageSource>, lash_kernel_doc::Selection), HelperCompatibilityError>
    {
        use std::{cell::RefCell, rc::Rc};
        #[cfg(any(test, feature = "perf-witness"))]
        helper_compatibility_work::update(|work| work.comparisons += 1);
        if self.0.helper_library == build.identity() {
            return Ok((source, lash_kernel_doc::Selection::Fresh));
        }
        // The shared owner exists before the walk starts and is dropped with
        // it on every refusal. No page-port clone or fabricated grant is needed.
        let source = Rc::new(RefCell::new(source));
        #[cfg(any(test, feature = "perf-witness"))]
        helper_compatibility_work::update(|work| work.live_code_walks += 1);
        let live = super::checkpoint_code(self, Box::new(SharedSource(Rc::clone(&source))))?;
        let restored = super::code::restore_selection(live, documents, code_imports, build)?;
        check_pins(restored.pins.iter().copied(), build)?;
        Rc::try_unwrap(source)
            .map(|source| {
                (
                    source.into_inner(),
                    lash_kernel_doc::Selection::Restored(restored),
                )
            })
            .map_err(|_| {
                HeapFault::Malformed("helper compatibility retained its page port".into()).into()
            })
    }
}

struct SharedSource(std::rc::Rc<std::cell::RefCell<Box<dyn super::PageSource>>>);
impl super::PageSource for SharedSource {
    fn read(
        &mut self,
        pages: &[lash_kernel_doc::heap::PageRef],
    ) -> Result<Vec<Result<super::PageAnswer, HeapFault>>, HeapFault> {
        self.0.borrow_mut().read(pages)
    }
    fn seal(&mut self, page: super::PageBody) -> Result<lash_kernel_doc::heap::PageRef, HeapFault> {
        self.0.borrow_mut().seal(page)
    }
}

/// Calling-thread counters for compatibility only; execution preparation has
/// its own probes and is never counted as a compatibility decode or hash.
#[cfg(any(test, feature = "perf-witness"))]
pub mod helper_compatibility_work {
    use std::cell::Cell;
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Work {
        pub comparisons: u64,
        pub live_code_walks: u64,
        pub membership_probes: u64,
        pub dependency_steps: u64,
    }
    thread_local! {
        static WORK: Cell<Option<Work>> = const { Cell::new(None) };
    }
    pub fn enable() {
        WORK.set(Some(Work::default()));
    }
    pub fn take() -> Option<Work> {
        WORK.replace(WORK.get().map(|_| Work::default()))
    }
    pub(crate) fn update(change: impl FnOnce(&mut Work)) {
        if let Some(mut work) = WORK.get() {
            change(&mut work);
            WORK.set(Some(work));
        }
    }
}
