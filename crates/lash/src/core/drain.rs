/// The authoritative drain status of one Lash deployment at a point in time.
///
/// The host supplies [`accepting_new_work`](Self::accepting_new_work) because
/// admission is host policy. Lash reads the configured process registry and
/// counts every retained non-terminal process row, including waiting or
/// suspended work and retrying work whose persisted status remains `running`,
/// and the session store's turns in flight and parked turns (FIG-3586). This
/// read does not stop routing, impose a deadline, or retire anything.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct DeploymentDrainStatus {
    /// Whether the host is still admitting new work to this deployment.
    pub accepting_new_work: bool,
    /// Number of retained process rows that are not terminal yet.
    pub remaining_invocations: usize,
    /// Sessions with a turn in flight: a pending queued run, a claimed turn
    /// input that is not settled, or a parked turn. Parked turns are included.
    pub in_flight_turns: usize,
    /// Sessions whose turn is parked: aborted on a replay refusal, holding
    /// its claims until a redrive under the build that wrote its journal, a
    /// cancel, or a fork resolves it.
    pub parked_turns: usize,
    /// Host-clock epoch milliseconds of the oldest live park's first refusal:
    /// the minimum `since_ms` over parked turns (NOW-B folds in parked
    /// processes). `None` when nothing is parked.
    pub oldest_parked_since_ms: Option<u64>,
    /// Host-clock epoch milliseconds at which this read completed.
    pub checked_at: u64,
}

impl DeploymentDrainStatus {
    /// True only when admission is closed and no non-terminal process rows
    /// and no unsettled turns remain.
    pub fn drained(&self) -> bool {
        !self.accepting_new_work
            && self.remaining_invocations == 0
            && self.in_flight_turns == 0
            && self.parked_turns == 0
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
            in_flight_turns: usize,
            parked_turns: usize,
            oldest_parked_since_ms: Option<u64>,
            checked_at: u64,
            drained: bool,
        }
        Wire {
            accepting_new_work: self.accepting_new_work,
            remaining_invocations: self.remaining_invocations,
            in_flight_turns: self.in_flight_turns,
            parked_turns: self.parked_turns,
            oldest_parked_since_ms: self.oldest_parked_since_ms,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}
