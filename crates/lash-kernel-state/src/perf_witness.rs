//! Fixed, opt-in thread-local implementation work. Never persisted.
//! Counts are calls/rows/work units, not physical instructions. Trace work
//! and scratch peaks cover completed traces only; attempts also include failures.
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Work {
    /// Inline leaves parsed from their stored token grammar.
    pub inline_decodes: u64,
    /// Point lookups of an object's stored head, excluding directory scans.
    pub object_head_lookups: u64,
    /// Checkpoint publication work, including projection and budget retries.
    pub seal_attempts: u64,
    pub seal_nodes_hashed: u64,
    pub seal_nodes_visited: u64,
    pub seal_rows_examined: u64,
    pub seal_boundary_hashes: u64,
    pub seal_pages_read: u64,
    pub tree_leaf_clones: u64,
    pub flat_size_members: u64,
    pub tree_index_clones: u64,
    pub tree_rows_cloned: u64,
    pub accounting_additions: u64,
    pub refinement_attempts: u64,
    pub completed_refinements: u64,
    pub completed_trace_work: u64,
    pub trace_bitmap_peak_bytes: u64,
    pub trace_work_list_peak_bytes: u64,
    pub trace_budget_peak_bytes: u64,
    /// Arena objects a projection examined: walked to assign ids, or
    /// written back (seam D, invariant 4).
    pub projection_visits: u64,
    /// Arena objects a collection marked.
    pub arena_marks: u64,
    /// Mutable objects given a durable id (seam C).
    pub identified: u64,
}

thread_local! {
    static WORK: Cell<Option<Work>> = const { Cell::new(None) };
}

pub fn enable_work() {
    WORK.set(Some(Work::default()));
}

pub fn take_work() -> Option<Work> {
    WORK.with(|work| work.replace(work.get().map(|_| Work::default())))
}

pub fn disable_work() {
    WORK.set(None);
}

pub(crate) fn record(change: impl FnOnce(&mut Work)) {
    WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            change(&mut counts);
            work.set(Some(counts));
        }
    });
}

/// Nestable checkpoint window. Drop restores attribution on every exit.
pub struct SealScope(std::marker::PhantomData<std::rc::Rc<()>>);
thread_local! {
    static SEAL_DEPTH: Cell<u64> = const { Cell::new(0) };
}
impl SealScope {
    pub fn enter() -> Self {
        SEAL_DEPTH.set(SEAL_DEPTH.get() + 1);
        record(|w| w.seal_attempts += 1);
        Self(std::marker::PhantomData)
    }
}
impl Drop for SealScope {
    fn drop(&mut self) {
        SEAL_DEPTH.set(SEAL_DEPTH.get() - 1);
    }
}
pub(crate) fn sealing(change: impl FnOnce(&mut Work)) {
    if SEAL_DEPTH.get() > 0 {
        record(change);
    }
}
