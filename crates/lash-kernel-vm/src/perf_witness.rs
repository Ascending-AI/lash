//! Opt-in, thread-local VM work counts for the counted released-worker
//! harness (FIG-6196), and profiling of one run. Nothing is persisted, and
//! ordinary builds compile every call out. Page loads and seals are counted
//! by the embedder's page source, not here.
//!
//! This is the machine's one diagnostic seam outside the kernel crates
//! (FIG-6688): its counters describe this engine, and an engine that
//! replaces it redefines them.
use std::cell::Cell;

/// The heap's share of [`Work`].
pub use lash_kernel_state::kernel_internals::perf_witness::Work as HeapWork;

/// Counts accumulated since the last [`take_work`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub vm_steps: u64,
    /// Charge regions committed, charge anchors run, completion paths and charge attribution.
    pub charge_work: u64,
    /// Publications of the run's execution roots to its heap (FIG-6318).
    pub root_publishes: u64,
    /// Generated-plan statements that open a memory reservation scope.
    pub memory_scopes: u64,
    /// Slow resolutions for kp2-frame (FIG-6588), excluding active hits.
    pub task_resolutions: u64,
    pub frame_resolutions: u64,
    /// Actual Null writes (growth/reference cleanup) and logical clear extents (FIG-6614).
    pub register_slots_initialized: u64,
    pub register_slots_cleared: u64,
    /// Arena values given their page form (seam C).
    pub page_forms: u64,
    pub heap: HeapWork,
}

thread_local! {
    static WORK: Cell<Option<Work>> = const { Cell::new(None) };
}

/// Enables count-only diagnostics on the calling thread, without VM-loop
/// locks or allocations.
pub fn enable_work() {
    WORK.set(Some(Work::default()));
    lash_kernel_state::kernel_internals::perf_witness::enable_work();
}

/// The counts since the last call, resetting them, or `None` when counting
/// is not enabled on this thread.
pub fn take_work() -> Option<Work> {
    let mut counts = WORK.with(|work| work.replace(work.get().map(|_| Work::default())))?;
    counts.heap =
        lash_kernel_state::kernel_internals::perf_witness::take_work().unwrap_or_default();
    Some(counts)
}

#[inline(always)]
pub(crate) fn enabled() -> bool {
    WORK.with(|work| work.get().is_some())
}

pub(crate) fn step() {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            counts.vm_steps += 1;
            work.set(Some(counts));
        }
    });
}

pub(crate) fn publish() {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            counts.root_publishes += 1;
            work.set(Some(counts));
        }
    });
}

/// Stop diagnostics on this thread, including heap diagnostics.
pub fn disable_work() {
    WORK.set(None);
    lash_kernel_state::kernel_internals::perf_witness::disable_work();
}

pub(crate) fn record(change: impl FnOnce(&mut Work)) {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            change(&mut counts);
            work.set(Some(counts));
        }
    });
}

pub use crate::observe::{
    CodeCounts, CodeKind, CodeSymbol, CodeWork, Completeness, GuestContext, GuestFrame, GuestPhase,
    GuestProfile, GuestSample, GuestSymbols, LibKind, LibSymbol, NO_STATEMENT, NativeCounts,
    NativeWork, READS_PER_TICK, ReadRefused, SymbolTime, WorkReport,
};

/// Profiling one run: its work counted by the guest function that does it,
/// and its guest context published to a sampler. Neither changes a result,
/// a charge, a park or an export.
pub trait Profiling {
    /// Counts the run's work by the function that does it, from here on;
    /// [`Profiling::work`] reads the counts.
    fn count_work(&mut self);
    /// The work counted since [`Profiling::count_work`], or nothing when
    /// the run does not count.
    fn work(&self) -> Option<WorkReport>;
    /// Publishes the running task's frames and the machine's phase to
    /// `context` from here on, for a sampler the embedder runs.
    fn publish_context(&mut self, context: std::sync::Arc<GuestContext>);
    /// What the tokens of this run's counts and context name.
    fn guest_symbols(&self) -> GuestSymbols;
}

impl Profiling for crate::KernelMachine {
    fn count_work(&mut self) {
        crate::KernelMachine::count_work(self);
    }
    fn work(&self) -> Option<WorkReport> {
        crate::KernelMachine::work(self)
    }
    fn publish_context(&mut self, context: std::sync::Arc<GuestContext>) {
        crate::KernelMachine::publish_context(self, context);
    }
    fn guest_symbols(&self) -> GuestSymbols {
        crate::KernelMachine::guest_symbols(self)
    }
}

/// The site of the statement the first ready task of the sealed `root`
/// runs next, read through `source`: where a profiler places a run that
/// parked. None when no task is ready.
///
/// # Errors
///
/// The heap's faults reading the root's tables.
pub fn next_ready_statement(
    checkpoint: &lash_kernel_state::paged::Checkpoint,
    source: Box<dyn lash_kernel_state::paged::PageSource>,
) -> Result<Option<lash_kernel_doc::Site>, lash_kernel_doc::heap::HeapFault> {
    let root = lash_kernel_state::kernel_internals::checkpoint_root(checkpoint);
    use lash_kernel_state::kernel_internals::PagedHeap;
    use lash_kernel_state::kernel_internals::directory::ordinal_key;
    use lash_kernel_state::kernel_internals::tree::Tree;
    use lash_kernel_state::paged::{EditEpoch, FrameRecord, TaskRecord};
    let heap = PagedHeap::open_header(root.heap, source, EditEpoch(1))?;
    let ready = Tree::<lash_kernel_doc::TaskId>::of_sequence(&root.ready);
    let Some((_, next)) = ready.rank_get(0, heap.pages())? else {
        return Ok(None);
    };
    let tasks = Tree::<TaskRecord>::of_keyed(&root.tasks);
    let Some(task) = tasks.get(&ordinal_key(next.0), heap.pages())? else {
        return Ok(None);
    };
    let Some(top) = task.frames.len().checked_sub(1) else {
        return Ok(None);
    };
    let Some((_, frame)) = task.frames.rank_get(top, heap.pages())? else {
        return Ok(None);
    };
    let frame: FrameRecord = frame;
    let site = frame.statement.resolve(heap.pages())?;
    Ok(Some(lash_kernel_doc::Site::clone(&site)))
}

/// A first-use library compilation boundary. A callback body's compilation
/// includes its ordinary fallback code and semantic restore tables.
#[derive(Clone, Copy)]
pub enum Preparation {
    Begin {
        function: lash_kernel_doc::FunctionId,
        callback: bool,
    },
    End,
}

type PreparationObserver = Box<dyn FnMut(Preparation)>;
thread_local! {
    static PREPARATION: std::cell::RefCell<Option<PreparationObserver>> = const { std::cell::RefCell::new(None) };
}

/// Installs a calling-thread probe for first-use helper compilation. The
/// probe runs outside guest execution and changes no guest charge or state.
pub fn observe_preparation(observer: Option<Box<dyn FnMut(Preparation)>>) {
    PREPARATION.with(|slot| *slot.borrow_mut() = observer);
}

#[cfg(feature = "perf-witness")]
pub(crate) fn preparation(event: Preparation) {
    PREPARATION.with(|slot| {
        if let Some(observer) = slot.borrow_mut().as_mut() {
            observer(event);
        }
    });
}
