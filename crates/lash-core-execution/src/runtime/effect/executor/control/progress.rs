#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentProgress {
    pub effects_executed: u64,
    pub journaled_bytes_estimate: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryReason {
    JournalBudget,
    /// The drain woke a segment waiting on a generation it retires, and the
    /// segment handed its open signal wait to a successor on the newest
    /// build (FIG-3799). The successor waits again on the same wait.
    HandOver,
}
