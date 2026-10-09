//! Host-selected operational pacing for a VM run.
use std::num::{NonZeroU64, NonZeroUsize};

/// Working cadence, separate from instruction/memory spend bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmPacing {
    pub heap_gc_allocation_interval: NonZeroU64,
    pub cooperative_yield_instructions: NonZeroUsize,
    /// Initial adaptive cancellation gap. The immutable 2^28 backstop still applies.
    pub cancel_checkpoint_instructions: NonZeroU64,
}

impl VmPacing {
    /// Preserve the existing cadence: GC every 1,024 allocations, a yield every
    /// 1,024 dispatched instructions, and an initial cancel gap of 2^20.
    /// These are provisional working presets; no workload measurement backs them.
    pub const fn standard() -> Self {
        Self {
            heap_gc_allocation_interval: NonZeroU64::MIN
                .saturating_add(super::HEAP_GC_ALLOCATION_INTERVAL - 1),
            cooperative_yield_instructions: NonZeroUsize::MIN
                .saturating_add(super::COOPERATIVE_YIELD_INSTRUCTION_BUDGET - 1),
            cancel_checkpoint_instructions: NonZeroU64::MIN
                .saturating_add(super::CANCEL_CHECKPOINT_INSTRUCTIONS - 1),
        }
    }

    /// Adaptive checkpoints double their gap up to the structural backstop.
    pub fn checkpoints_reached(self, mut instructions: u64) -> u64 {
        let cap = super::CANCEL_CHECKPOINT_INTERVAL_CAP;
        let mut gap = self.cancel_checkpoint_instructions.get().min(cap);
        let mut checkpoints = 0;
        while gap < cap && instructions >= gap {
            instructions -= gap;
            checkpoints += 1;
            gap = gap.saturating_mul(2).min(cap);
        }
        if gap == cap {
            checkpoints += instructions / cap;
        }
        checkpoints
    }
}

impl Default for VmPacing {
    fn default() -> Self {
        Self::standard()
    }
}
