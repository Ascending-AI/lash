//! Derived run observations, addressed by document site ordinal.

use serde::{Deserialize, Serialize};

/// Per-run observation. Off is unobserved, never a zero count.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunObservation {
    #[default]
    Off,
    Counts,
}

impl RunObservation {
    /// Whether this setting records no counts.
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// The common input to the workflow coverage helper. A missing observation
/// means the execution's start was not observed; neither it nor off implies zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RanCounts {
    pub observation: Option<RunObservation>,
    /// Statement/block ordinals, followed by action issues and while tests
    /// (`Document::action_ordinals` and `Document::condition_ordinals`).
    pub ran: Vec<(u32, u64)>,
}

/// A verified segment's statement starts and empty-block entries, in order
/// within each task. Tasks retain the order in which they first appeared.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPath {
    pub tasks: Vec<TaskPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPath {
    pub task: crate::TaskIdentity,
    pub starts: Vec<u32>,
}
