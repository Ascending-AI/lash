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
    pub flat_size_visits: u64,
    /// Charge regions committed, charge anchors run, completion paths and charge attribution.
    pub charge_work: u64,
    /// Publications of the run's execution roots to its heap (FIG-6318).
    pub root_publishes: u64,
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
    /// `context` from here on, for a sampler the embedder runs. Publication
    /// preserves quiet execution. Register frames publish at frame transitions;
    /// their statement is the last published site, not a per-instruction cursor.
    fn publish_context(&mut self, context: std::sync::Arc<GuestContext>);
    /// What the tokens of this run's counts and context name.
    fn guest_symbols(&self) -> GuestSymbols;
}

/// Where the first ready task of the sealed `checkpoint` stands, read
/// through `source`: a statement or an action nested in one (`K-SITE-001`),
/// where a profiler places a run that parked. None when no task is ready.
///
/// # Errors
///
/// The heap's faults reading the run's record.
pub fn next_ready_statement(
    checkpoint: &lash_kernel_state::paged::Checkpoint,
    source: Box<dyn lash_kernel_state::paged::PageSource>,
) -> Result<Option<lash_kernel_value::sites::Site>, lash_kernel_doc::heap::HeapFault> {
    let root = lash_kernel_state::kernel_internals::checkpoint_root(checkpoint);
    let (run, _) = lash_kernel_state::paged::run::continuation::open(source, root.heap)?;
    Ok(run
        .ready
        .first()
        .and_then(|task| run.tasks.get(*task as usize))
        .and_then(|task| task.frames.last())
        .map(|frame| frame.site.clone()))
}

/// A first-use library compilation boundary. A callback body's compilation
/// includes its ordinary fallback code and semantic restore tables.
#[derive(Clone, Copy)]
pub enum Preparation {
    Begin {
        function: lash_kernel_value::names::FunctionId,
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

/// The run's logical balance, observed at machine verdict points.
pub type MemoryObserver = fn(u64);
thread_local! {
    static MEMORY_OBSERVER: Cell<Option<MemoryObserver>> = const { Cell::new(None) };
}
/// Installs or removes the calling thread's diagnostic memory observer.
pub fn observe_memory(observer: Option<MemoryObserver>) {
    MEMORY_OBSERVER.set(observer);
}
