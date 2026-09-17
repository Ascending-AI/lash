use std::sync::atomic::{AtomicU64, Ordering};

/// The run-shape counter alphabet. `RunShape`, `RunShapeTotals`, and the
/// report all derive from this one enum, so a new counter cannot be counted
/// without being reported.
#[derive(Clone, Copy, Debug)]
pub(super) enum RunShapeCounter {
    EnqueuesCommitted,
    ConsumesCommitted,
    OutOfOrderStates,
    Spawns,
    TerminalTransitions,
    TailTerminalTransitions,
    TailPruneOps,
    PruneOpsWithEffect,
    TailPruneOpsWithEffect,
}

impl RunShapeCounter {
    pub(super) const ALL: &[Self] = &[
        Self::EnqueuesCommitted,
        Self::ConsumesCommitted,
        Self::OutOfOrderStates,
        Self::Spawns,
        Self::TerminalTransitions,
        Self::TailTerminalTransitions,
        Self::TailPruneOps,
        Self::PruneOpsWithEffect,
        Self::TailPruneOpsWithEffect,
    ];
    const COUNT: usize = Self::ALL.len();

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::EnqueuesCommitted => "enqueues_committed",
            Self::ConsumesCommitted => "consumes_committed",
            Self::OutOfOrderStates => "out_of_order_states",
            Self::Spawns => "spawns",
            Self::TerminalTransitions => "terminal_transitions",
            Self::TailTerminalTransitions => "tail_terminal_transitions",
            Self::TailPruneOps => "tail_prune_ops",
            Self::PruneOpsWithEffect => "prune_ops_with_effect",
            Self::TailPruneOpsWithEffect => "tail_prune_ops_with_effect",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RunShape {
    counts: [u64; RunShapeCounter::COUNT],
}

impl std::ops::Index<RunShapeCounter> for RunShape {
    type Output = u64;
    fn index(&self, counter: RunShapeCounter) -> &u64 {
        &self.counts[counter as usize]
    }
}

impl std::ops::IndexMut<RunShapeCounter> for RunShape {
    fn index_mut(&mut self, counter: RunShapeCounter) -> &mut u64 {
        &mut self.counts[counter as usize]
    }
}

#[derive(Debug)]
pub(super) struct RunShapeTotals {
    counts: [AtomicU64; RunShapeCounter::COUNT],
}

impl Default for RunShapeTotals {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl RunShapeTotals {
    pub(super) fn add(&self, shape: RunShape) {
        for counter in RunShapeCounter::ALL {
            self.counts[*counter as usize].fetch_add(shape[*counter], Ordering::Relaxed);
        }
    }

    pub(super) fn report(&self) -> String {
        RunShapeCounter::ALL
            .iter()
            .map(|counter| {
                format!(
                    "{}={}",
                    counter.name(),
                    self.counts[*counter as usize].load(Ordering::Relaxed)
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}
