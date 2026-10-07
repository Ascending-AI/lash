/// The authoritative drain status of one Lash deployment at a point in time.
///
/// The host supplies [`accepting_new_work`](Self::accepting_new_work) because
/// admission is host policy. Lash reads the configured process registry and
/// counts every retained non-terminal process row, including waiting or
/// suspended work and retrying work whose persisted status remains `running`,
/// the parked processes among them, and the session store's turns in flight
/// (FIG-3586, FIG-3659). This
/// read does not stop routing, impose a deadline, or retire anything.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct DeploymentDrainStatus {
    /// Whether the host is still admitting new work to this deployment.
    pub accepting_new_work: bool,
    /// Number of retained process rows that are not terminal yet.
    pub remaining_invocations: usize,
    /// Sessions with a turn in flight: an unfinished run, or a turn input
    /// bound to a run that is not settled.
    pub in_flight_turns: usize,
    /// Stalled store→engine delivery obligations per kind (ADR 0109 §1.5):
    /// work the relay stopped retrying — refused, undecodable, or at its
    /// attempt ceiling — until an operator re-arms it through
    /// [`LashCore::rearm_obligation`](crate::LashCore::rearm_obligation).
    /// Every kind is present, zero included.
    pub stalled_obligations: std::collections::BTreeMap<lash_core::store::ObligationKind, u64>,
    /// Host-clock epoch milliseconds at which this read completed.
    pub checked_at: u64,
}

impl DeploymentDrainStatus {
    /// True only when admission is closed and no non-terminal process rows,
    /// no unsettled turns and no stalled obligations remain.
    pub fn drained(&self) -> bool {
        !self.accepting_new_work
            && self.remaining_invocations == 0
            && self.in_flight_turns == 0
            && self.stalled_obligations.values().all(|count| *count == 0)
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
            stalled_obligations:
                &'a std::collections::BTreeMap<lash_core::store::ObligationKind, u64>,
            checked_at: u64,
            drained: bool,
        }
        Wire {
            accepting_new_work: self.accepting_new_work,
            remaining_invocations: self.remaining_invocations,
            in_flight_turns: self.in_flight_turns,
            stalled_obligations: &self.stalled_obligations,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}

/// What a drained node handed to the next build (ADR 0106 §1): each
/// session and process actor it released `ready` at a committed phase, in
/// release order. Nodes that decode their formats claim them and resume
/// each from its committed rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeDrainReport {
    /// The sessions it released.
    pub sessions: Vec<lash_core::SessionId>,
    /// The processes it released.
    pub processes: Vec<lash_core::ProcessId>,
}

impl NodeDrainReport {
    /// The report of the actors a drain released.
    pub(crate) fn of(released: &[lash_core::durable_port::ActorKey]) -> Self {
        let mut report = Self::default();
        // An activation runs only an actor whose id parses, so each one it
        // released does.
        for actor in released {
            match actor.kind() {
                lash_core::durable_port::ActorKind::Session => report
                    .sessions
                    .extend(lash_core::SessionId::try_from(actor.id().to_owned()).ok()),
                lash_core::durable_port::ActorKind::Process => report
                    .processes
                    .extend(lash_core::ProcessId::parse(actor.id()).ok()),
            }
        }
        report
    }
}

/// Why [`LashCore::drain`](crate::LashCore::drain) did not end with its node
/// drained.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NodeDrainError {
    /// The core runs no node: it was built not to serve sessions, or it
    /// shut down before its node started.
    #[error("this core runs no node to drain")]
    NotServing,
    /// The node stopped before it drained: its lease was lost or went
    /// unrenewed, or the core shut down during the drain. Whatever it still
    /// owned is fenced and claimed by other nodes as after a crash.
    #[error("the core's node stopped ({0:?}) before it drained")]
    Stopped(lash_core::durable_port::runner::Stopped),
    /// The store refused the node's registration or its release.
    #[error("the core's node did not drain: {0}")]
    Store(#[from] lash_core::durable_port::DurableError),
}

#[cfg(test)]
mod tests {
    use lash_core::store::ObligationKind;

    use super::DeploymentDrainStatus;

    /// ADR 0109 §1.5, FIG-3586: a deployment is drained only when admission
    /// is closed and it holds no live work and no stalled obligation of any
    /// kind. Each of them alone keeps it undrained, and the report names
    /// every kind, zero included, on the wire.
    #[test]
    fn deployment_drain_aggregates_every_live_and_stalled_kind() {
        let idle = DeploymentDrainStatus {
            accepting_new_work: false,
            remaining_invocations: 0,
            in_flight_turns: 0,
            stalled_obligations: ObligationKind::ALL.iter().map(|kind| (*kind, 0)).collect(),
            checked_at: 1,
        };
        assert!(idle.drained());
        let live: [(&str, DeploymentDrainStatus); 3] = [
            (
                "admission open",
                DeploymentDrainStatus {
                    accepting_new_work: true,
                    ..idle.clone()
                },
            ),
            (
                "a non-terminal process",
                DeploymentDrainStatus {
                    remaining_invocations: 1,
                    ..idle.clone()
                },
            ),
            (
                "a turn in flight",
                DeploymentDrainStatus {
                    in_flight_turns: 1,
                    ..idle.clone()
                },
            ),
        ];
        for (what, status) in live {
            assert!(!status.drained(), "{what} keeps the deployment undrained");
        }
        for kind in ObligationKind::ALL {
            let mut stalled = idle.clone();
            stalled.stalled_obligations.insert(kind, 1);
            assert!(
                !stalled.drained(),
                "a stalled {kind} obligation keeps the deployment undrained"
            );
            let wire = serde_json::to_value(&stalled).expect("serialize the drain status");
            assert_eq!(wire["drained"], false, "{kind}");
            let counts = wire["stalled_obligations"]
                .as_object()
                .expect("stalled obligations by kind");
            assert_eq!(
                counts.len(),
                ObligationKind::ALL.len(),
                "every kind is named"
            );
            assert_eq!(counts[kind.label()], 1, "{kind}");
            assert!(
                ObligationKind::ALL
                    .iter()
                    .filter(|other| **other != kind)
                    .all(|other| counts[other.label()] == 0),
                "{kind}: only its own count moved"
            );
        }
    }
}
