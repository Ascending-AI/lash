//! Kernel primitives used by external backend certification fixtures.

pub use crate::attachments::PersistenceManifestAdapter;
pub use crate::runtime::default_queued_drain_policy;
pub use crate::runtime::effect::group_drain::DrainedChild;
pub use crate::runtime::effect::group_drain::{
    ChildDrainOutcome, GroupDrainReport, GroupExecutors, StoreEffectGroupDrain,
};
pub use crate::runtime::native_substrate::lane_wait::{
    QueuedLaneGiveUp, QueuedLaneWait, QueuedLaneWaitStep,
};
pub use crate::runtime::state::RuntimeCheckpointComponents;
pub use crate::runtime::state::{append_session_nodes_to_state_with_clock, boundary_operation};
pub use crate::runtime::turn_control::{ActiveTurnControl, TurnCancelPeekIdentity};
pub use crate::runtime::{
    PendingTokenLedgerEntry, StagedTokenLedger, append_receipt_mixed_usage_envelope_conformance,
    append_usage_cancellation_exactly_once_conformance,
    reconcile_pruned_trigger_deliveries_interleaved, record_token_usage_shared,
    stage_token_ledger_shared,
};

/// Raw graph mutation for corruption fixtures, available only with test support.
pub trait SessionGraphConformanceAccess {
    fn data_mut(&mut self) -> &mut crate::session_graph::SessionGraphData;
}

impl SessionGraphConformanceAccess for crate::SessionGraph {
    fn data_mut(&mut self) -> &mut crate::session_graph::SessionGraphData {
        crate::SessionGraph::data_mut(self)
    }
}

/// Generation injection for backend checkpoint round-trip fixtures.
pub trait ToolStateConformanceAccess {
    fn with_generation_for_conformance(self, generation: u64) -> Self;
}

impl ToolStateConformanceAccess for crate::ToolState {
    fn with_generation_for_conformance(self, generation: u64) -> Self {
        crate::ToolState::with_generation(self, generation)
    }
}
