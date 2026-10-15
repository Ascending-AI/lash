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
}

thread_local! {
    static WORK: Cell<Option<Work>> = const { Cell::new(None) };
}

/// Enables count-only diagnostics on the calling thread, without VM-loop
/// locks or allocations.
pub fn enable_work() {
    WORK.set(Some(Work::default()));
}

/// The counts since the last call, resetting them, or `None` when counting
/// is not enabled on this thread.
pub fn take_work() -> Option<Work> {
    WORK.with(|work| work.replace(work.get().map(|_| Work::default())))
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
