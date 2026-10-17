//! Fixed, opt-in thread-local implementation work. Never persisted.
//! Counts are calls/rows/work units, not physical instructions. Trace work
//! and scratch peaks cover completed traces only; attempts also include failures.
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Work {
    pub tree_leaf_clones: u64,
    pub tree_index_clones: u64,
    pub tree_rows_cloned: u64,
    pub accounting_certificate_reads: u64,
    pub accounting_additions: u64,
    /// Runtime composite and mutable-object memory pricing passes.
    pub arena_memory_prices: u64,
    pub accounting_changes: u64,
    pub accounting_reservations: u64,
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
