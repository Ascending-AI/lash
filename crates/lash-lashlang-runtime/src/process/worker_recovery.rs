//! A process body's worker accounting across its segment boundaries
//! (ADR 0123).

use lash_sansio::ProcessId;

/// Which worker-recovery scope an execution of a process body reserves,
/// and the totals every earlier boundary's executions consumed.
///
/// Each boundary reserves a scope of its own. A segment that handed over is
/// still re-executed by its engine — Restate replays its handler from the
/// start on every resumption, running the body again to reach the steps
/// after its boundary — while its successor runs live. Sharing one scope,
/// that replay's reservation would count the successor's active worker as a
/// lost attempt and fence its settlement, and the body would exhaust its
/// attempts with no worker ever lost (FIG-4422). The totals ride the
/// recorded handover, so a boundary's first reservation starts from what
/// the body already consumed, and consumed attempts and CPU stay monotone
/// across the whole process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct WorkerRecoveryLedger {
    /// The boundaries the body has crossed: 0 for the first segment.
    pub(super) boundary: u64,
    pub(super) totals: lash_core::store::worker_recovery::WorkerRecoveryTotals,
}

impl WorkerRecoveryLedger {
    /// The ledger `handover` carries, read before the handover's full
    /// decode: a handover the decode refuses ends the run there, before
    /// any worker launches, whatever this answered.
    pub(super) fn carried(handover: Option<&lash_core::SegmentHandover>) -> Self {
        #[derive(serde::Deserialize)]
        struct Probe {
            #[serde(default)]
            worker_recovery: WorkerRecoveryLedger,
        }
        handover
            .and_then(|handover| serde_json::from_slice::<Probe>(&handover.engine_state).ok())
            .map(|probe| probe.worker_recovery)
            .unwrap_or_default()
    }

    /// The recovery scope this boundary's executions reserve: the body's
    /// own scope before its first boundary, one per boundary after it.
    pub(super) fn scope(&self, process_id: &ProcessId) -> String {
        let body = lash_vm_broker::CodeCallIdentities::process_body(process_id.clone()).scope();
        match self.boundary {
            0 => body,
            boundary => format!("{body}:boundary:{boundary}"),
        }
    }

    /// The ledger the next boundary carries: `totals` consumed so far.
    pub(super) fn crossed(
        &self,
        totals: lash_core::store::worker_recovery::WorkerRecoveryTotals,
    ) -> Self {
        Self {
            boundary: self.boundary + 1,
            totals: lash_core::store::worker_recovery::WorkerRecoveryTotals {
                replacement: false,
                ..totals
            },
        }
    }
}
