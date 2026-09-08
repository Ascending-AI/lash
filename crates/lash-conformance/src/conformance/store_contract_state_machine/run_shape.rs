use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RunShape {
    pub(super) enqueues_committed: u64,
    pub(super) consumes_committed: u64,
    pub(super) out_of_order_states: u64,
    pub(super) spawns: u64,
    pub(super) terminal_transitions: u64,
    pub(super) tail_terminal_transitions: u64,
    pub(super) tail_prune_ops: u64,
    pub(super) prune_ops_with_effect: u64,
    pub(super) tail_prune_ops_with_effect: u64,
}

#[derive(Debug, Default)]
pub(super) struct RunShapeTotals {
    pub(super) enqueues_committed: AtomicU64,
    pub(super) consumes_committed: AtomicU64,
    pub(super) out_of_order_states: AtomicU64,
    pub(super) spawns: AtomicU64,
    pub(super) terminal_transitions: AtomicU64,
    pub(super) tail_terminal_transitions: AtomicU64,
    pub(super) tail_prune_ops: AtomicU64,
    pub(super) prune_ops_with_effect: AtomicU64,
    pub(super) tail_prune_ops_with_effect: AtomicU64,
}

impl RunShapeTotals {
    pub(super) fn add(&self, shape: RunShape) {
        self.enqueues_committed
            .fetch_add(shape.enqueues_committed, Ordering::Relaxed);
        self.consumes_committed
            .fetch_add(shape.consumes_committed, Ordering::Relaxed);
        self.out_of_order_states
            .fetch_add(shape.out_of_order_states, Ordering::Relaxed);
        self.spawns.fetch_add(shape.spawns, Ordering::Relaxed);
        self.terminal_transitions
            .fetch_add(shape.terminal_transitions, Ordering::Relaxed);
        self.tail_terminal_transitions
            .fetch_add(shape.tail_terminal_transitions, Ordering::Relaxed);
        self.tail_prune_ops
            .fetch_add(shape.tail_prune_ops, Ordering::Relaxed);
        self.prune_ops_with_effect
            .fetch_add(shape.prune_ops_with_effect, Ordering::Relaxed);
        self.tail_prune_ops_with_effect
            .fetch_add(shape.tail_prune_ops_with_effect, Ordering::Relaxed);
    }
}
