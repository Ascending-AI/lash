//! Opt-in, thread-local VM work counts for the counted released-worker
//! harness (FIG-6196). Nothing is persisted, and ordinary builds compile
//! every call out. Page loads and seals are counted by the embedder's page
//! source, not here.
use std::cell::Cell;

/// Counts accumulated since the last [`take_work`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub vm_steps: u64,
    /// Publications of the run's execution roots to its heap (FIG-6318).
    pub root_publishes: u64,
    /// Slow resolutions for kp2-frame (FIG-6588), excluding active hits.
    pub task_resolutions: u64,
    pub frame_resolutions: u64,
    /// Actual Null writes (growth/reference cleanup) and logical clear extents (FIG-6614).
    pub register_slots_initialized: u64,
    pub register_slots_cleared: u64,
    pub heap: lash_kernel_state::perf_witness::Work,
}

thread_local! {
    static WORK: Cell<Option<Work>> = const { Cell::new(None) };
}

/// Enables count-only diagnostics on the calling thread, without VM-loop
/// locks or allocations.
pub fn enable_work() {
    WORK.set(Some(Work::default()));
    lash_kernel_state::perf_witness::enable_work();
}

/// The counts since the last call, resetting them, or `None` when counting
/// is not enabled on this thread.
pub fn take_work() -> Option<Work> {
    let mut counts = WORK.with(|work| work.replace(work.get().map(|_| Work::default())))?;
    counts.heap = lash_kernel_state::perf_witness::take_work().unwrap_or_default();
    Some(counts)
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
    lash_kernel_state::perf_witness::disable_work();
}

pub(crate) fn record(change: impl FnOnce(&mut Work)) {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            change(&mut counts);
            work.set(Some(counts));
        }
    });
}
