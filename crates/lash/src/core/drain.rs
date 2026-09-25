/// The authoritative drain status of one Lash deployment at a point in time.
///
/// The host supplies [`accepting_new_work`](Self::accepting_new_work) because
/// admission is host policy. Lash reads the configured process registry and
/// counts every retained non-terminal process row, including waiting or
/// suspended work and retrying work whose persisted status remains `running`,
/// the parked processes among them, and the session store's turns in flight
/// and parked turns (FIG-3586, FIG-3659). This
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
    /// Processes that are parked: their body refused to replay its journal,
    /// and they hold what they hold until an operator acts. Parked processes
    /// are non-terminal, so they are included in
    /// [`remaining_invocations`](Self::remaining_invocations).
    pub parked_processes: usize,
    /// Host-clock epoch milliseconds of the oldest live park's first refusal:
    /// the minimum `since_ms` over parked turns and parked processes. `None`
    /// when nothing is parked.
    pub oldest_parked_since_ms: Option<u64>,
    /// Parked turns admitted, and parked processes started, under an
    /// executable generation this build retired, per that generation
    /// (FIG-3571): what an old-build drain of each generation still has to
    /// redrive.
    pub retired_by_executable_generation:
        std::collections::BTreeMap<lash_core::ExecutableGeneration, usize>,
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
        struct Wire<'a> {
            accepting_new_work: bool,
            remaining_invocations: usize,
            in_flight_turns: usize,
            parked_turns: usize,
            parked_processes: usize,
            oldest_parked_since_ms: Option<u64>,
            retired_by_executable_generation:
                &'a std::collections::BTreeMap<lash_core::ExecutableGeneration, usize>,
            checked_at: u64,
            drained: bool,
        }
        Wire {
            accepting_new_work: self.accepting_new_work,
            remaining_invocations: self.remaining_invocations,
            in_flight_turns: self.in_flight_turns,
            parked_turns: self.parked_turns,
            parked_processes: self.parked_processes,
            oldest_parked_since_ms: self.oldest_parked_since_ms,
            retired_by_executable_generation: &self.retired_by_executable_generation,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}
