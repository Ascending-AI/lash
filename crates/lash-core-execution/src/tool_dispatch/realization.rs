//! A final's realized intents. Its declarations realize inside the admitted
//! execution that runs the call, before the call is presented; what they
//! write to the lash store is staged, and commits with the call's outcome
//! (ADR 0132 §5).

use serde::{Deserialize, Serialize};

/// The ordered outcome of every intent a final declared.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealizationReceipt {
    pub outcomes: Vec<crate::ToolIntentExecutionOutcome>,
}

impl RealizationReceipt {
    /// Whether an intent's execution refused, which presents the final as
    /// a failure and so settles it as a rejection.
    #[must_use]
    pub fn rejects(&self) -> bool {
        super::attempt_coordinator::superseding_refusal(&self.outcomes).is_some()
    }
}

/// What realizing a final's intents answers: the ordered outcome of every
/// intent, and the store-local effects those outcomes stand on, which
/// commit with the call's outcome under its owner's epoch fence.
#[derive(Clone, Debug, Default)]
pub struct Realization {
    pub receipt: RealizationReceipt,
    pub store_local: Vec<crate::runtime::actor::round::StoreLocalEffect>,
}
