//! Aggregate consumer modes and deferred completion projection.

use super::*;
use crate::runtime::effect::GroupWakePolicy;

/// How a group's consumer decides its aggregate (ADR 0099 §10 L1). A
/// caller-side loop decision, never journaled; [`Self::wake`] is the journaled
/// wake policy it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolAggregateConsumer {
    /// Every settlement: `allSettled` and every all-results batch.
    AllSettled,
    /// Stop at the first consumed rejection: `Promise.all`.
    All,
    /// Stop at the first settlement: `Promise.race`.
    Race,
    /// Stop at the first fulfilment: `Promise.any`.
    Any,
}

impl ToolAggregateConsumer {
    /// The three-way journaled wake policy this four-way consumer mode folds
    /// into every child's envelope (ADR 0099 §10 L1, ADR 0065).
    #[must_use]
    pub fn wake(self) -> GroupWakePolicy {
        match self {
            Self::AllSettled | Self::All => GroupWakePolicy::All,
            Self::Race => GroupWakePolicy::First,
            Self::Any => GroupWakePolicy::FirstSuccess,
        }
    }

    /// Whether a settlement with this fulfilment decides the aggregate.
    #[must_use]
    pub fn decides(self, fulfilled: bool) -> bool {
        match self {
            Self::AllSettled => false,
            Self::All => !fulfilled,
            Self::Race => true,
            Self::Any => fulfilled,
        }
    }
}

/// The tool failure a call refused by the session's `max_tool_calls` settles
/// with: typed by its code, never retried, and worded by the refusal so the
/// limit is named wherever the failure is shown (FIG-4546).
pub(crate) fn tool_call_limit_failure(exceeded: crate::ToolCallLimitExceeded) -> ToolFailure {
    ToolFailure::runtime(
        ToolFailureClass::ResourceLimit,
        crate::ToolCallLimitExceeded::CODE,
        exceeded.to_string(),
    )
}
