//! A final's realized intents. Its declarations realize in place, inside the
//! admitted execution that runs the call, before the call is presented
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
