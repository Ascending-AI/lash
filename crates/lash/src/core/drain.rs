/// The authoritative drain status of one Lash deployment at a point in time.
///
/// The host supplies [`accepting_new_work`](Self::accepting_new_work) because
/// admission is host policy. Lash reads the configured process registry and
/// counts every retained non-terminal process row, including waiting or
/// suspended work and retrying work whose persisted status remains `running`.
/// This read does not stop routing, impose a deadline, or retire anything.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct DeploymentDrainStatus {
    /// Whether the host is still admitting new work to this deployment.
    pub accepting_new_work: bool,
    /// Number of retained process rows that are not terminal yet.
    pub remaining_invocations: usize,
    /// Host-clock epoch milliseconds at which this read completed.
    pub checked_at: u64,
}

impl DeploymentDrainStatus {
    /// True only when admission is closed and no non-terminal rows remain.
    pub fn drained(&self) -> bool {
        !self.accepting_new_work && self.remaining_invocations == 0
    }
}

/// The serialized form keeps the `drained` key for hosts that consume the
/// JSON report; the value is computed, not stored.
impl serde::Serialize for DeploymentDrainStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Wire {
            accepting_new_work: bool,
            remaining_invocations: usize,
            checked_at: u64,
            drained: bool,
        }
        Wire {
            accepting_new_work: self.accepting_new_work,
            remaining_invocations: self.remaining_invocations,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}
