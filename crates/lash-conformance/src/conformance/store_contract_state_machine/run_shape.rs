use crate::conformance::run_shape;

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

impl run_shape::Counter for RunShapeCounter {
    const ALL: &'static [Self] = &[
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

    fn name(self) -> &'static str {
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

    fn index(self) -> usize {
        self as usize
    }
}

pub(super) type RunShape = run_shape::RunShape<RunShapeCounter>;
pub(super) type RunShapeTotals = run_shape::RunShapeTotals<RunShapeCounter>;
