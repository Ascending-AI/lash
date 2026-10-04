//! A build generation's drain (FIG-3799, ADR 0106 §1).
//!
//! An operator marks a generation draining when the build that runs it is
//! being retired. The mark is a store fact, so whichever deployment leads
//! recovery reads it: the leader's hand-over duty wakes every live process
//! whose current segment that generation admitted, each hands its open wait
//! to a successor on the newest build, and the generation's work runs down
//! to zero. The same port answers what is left of a generation's work, the
//! numbers an operator polls until the generation is drained.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::fleet_finalize::DeploymentRegistry;
use super::session_delete::SessionDeleteLedger;
use super::{ObligationKind, ObligationLedger, StoreError};
use crate::build_generation::BuildGeneration;
use crate::{ProcessId, SessionId};

/// A generation an operator marked draining, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainingGeneration {
    pub generation: BuildGeneration,
    /// Host-clock epoch milliseconds of the first mark.
    pub marked_at_ms: u64,
}

/// The durable work one build generation still holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenerationWork {
    /// Live (running or waiting) processes whose current segment the
    /// generation admitted: work pinned to its deployment. A process whose
    /// segment handed over to a successor that has not started yet still
    /// counts here until the successor's admission restamps it.
    pub live_processes: u64,
    /// Parked processes whose parked checkpoint the generation wrote: they
    /// resume only on a build of the generation, or are resolved by an
    /// operator. A successor the newest build refused counts here until the
    /// drain's re-send runs it on the generation's build (FIG-4750).
    pub parked_processes: u64,
    /// Parked turns whose parked checkpoint the generation wrote.
    pub parked_turns: u64,
    /// Sessions whose admitted shift — the queued run its admission stamped
    /// `admitted_generation` on (FIG-3795 S9) — has not settled: the turns
    /// the generation admitted that are still in flight (FIG-3884). A parked
    /// turn holds no pending run of its own here; it counts under
    /// [`parked_turns`](Self::parked_turns).
    pub in_flight_turns: u64,
}

/// The drain marks and the per-generation work reads (FIG-3799).
#[async_trait::async_trait]
pub trait GenerationDrainStore: Send + Sync {
    /// Mark `generation` draining at `now_ms`. `true` when this call marked
    /// it; a generation already marked keeps its first mark.
    async fn mark_draining(
        &self,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> Result<bool, StoreError>;

    /// Remove `generation`'s drain mark: the recovery leader stops waking its
    /// processes. `true` when a mark was removed.
    async fn clear_draining(&self, generation: &BuildGeneration) -> Result<bool, StoreError>;

    /// Every generation marked draining, in generation order.
    async fn draining_generations(&self) -> Result<Vec<DrainingGeneration>, StoreError>;

    /// The work `generation` still holds. Each count is an indexed read of its
    /// own table, not one snapshot across them.
    async fn generation_work(
        &self,
        generation: &BuildGeneration,
    ) -> Result<GenerationWork, StoreError>;

    /// The live processes whose current segment `generation` admitted,
    /// strictly after `after` in process-id order, at most `limit`.
    async fn live_processes(
        &self,
        generation: &BuildGeneration,
        after: Option<&ProcessId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<ProcessId>, StoreError>;

    /// The sessions holding a turn in flight that `generation` counts — an
    /// unfinished run stamped with it — strictly after `after` in
    /// session-id order, at most `limit` (FIG-4739). The drain wakes each
    /// one's parked turn waits so the turn hands over.
    async fn sessions_in_flight(
        &self,
        generation: &BuildGeneration,
        after: Option<&SessionId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<SessionId>, StoreError>;
}

/// What one build generation still holds while it drains (FIG-3799): the
/// read an operator polls after marking it draining until the generation's
/// deployment can be retired.
///
/// Lash counts the durable work pinned to the generation: live processes
/// whose current segment it admitted — including one whose segment handed
/// over to a successor that has not started yet — the parked processes and
/// turns whose checkpoint it wrote, and the turns its shifts admitted that
/// have not settled (FIG-3884). The recovery leader wakes the live
/// processes, each hands its open wait to a successor on the newest build,
/// and the count runs down. A process parked because the newest build
/// refused its successor is sent back to the generation's build by the same
/// pass and runs there until it ends (FIG-4750). Other parked work does not
/// move by itself: an operator redrives it on a build of the generation,
/// cancels it, or forks it. Nor is a drain finished while a session is closing: its close ended
/// its runs, but each run's turn-control waits stay registered with the
/// engine, on whichever build ran the run, until the session's physical
/// delete revokes them (ADR 0109 §4).
///
/// Stalled obligations are counted, but they do not hold the drain (ADR 0115
/// §3.5, amended by FIG-4076). No obligation is pinned to a generation: any
/// build of the window delivers one an operator re-arms, and one that no
/// build of the window can decode stays stalled whichever deployments are
/// registered. Keeping the generation's deployment would settle none of
/// them, so the operator is shown them to settle, not made to wait on them.
///
/// The type lives beside the store port that answers it so the facade's
/// `LashCore::generation_drain_status` and the operator binary compose the
/// same read through [`collect`](Self::collect).
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct GenerationDrainStatus {
    /// The generation read.
    pub generation: BuildGeneration,
    /// Host-clock epoch milliseconds at which an operator marked the
    /// generation draining; `None` while it is not marked.
    pub draining_since_ms: Option<u64>,
    /// Live processes whose current segment the generation admitted.
    pub live_processes: u64,
    /// Parked processes whose parked checkpoint the generation wrote.
    pub parked_processes: u64,
    /// Parked turns whose parked checkpoint the generation wrote.
    pub parked_turns: u64,
    /// Turns the generation's executes admitted that have not settled: the
    /// pending queued runs stamped `admitted_generation` (FIG-3795 S9,
    /// FIG-3884).
    pub in_flight_turns: u64,
    /// Sessions closing: their close committed and their physical delete,
    /// which revokes the waits their runs registered with the engine, has
    /// not run. Not per generation, as stalled obligations are not.
    pub closing_sessions: u64,
    /// Unfinished engine invocations pinned to a deployment serving this
    /// generation, including the predecessor's wait, attach, terminal read
    /// and session shift until they return. Independent of the SQL counts.
    pub unfinished_invocations: u64,
    /// Stalled store→engine delivery obligations per kind (ADR 0109 §1.5),
    /// every kind present, zero included. Not per generation, and they do
    /// not hold the drain: see [`drained`](Self::drained).
    pub stalled_obligations: BTreeMap<ObligationKind, u64>,
    /// Host-clock epoch milliseconds at which this read completed.
    pub checked_at: u64,
}

impl GenerationDrainStatus {
    /// Compose the status over `drain`'s reads, `session_delete`'s closing
    /// sessions, the stalled counts of the ledgers `obligation_ledger` hands
    /// out and the engine's unfinished invocations `registry` reports,
    /// stamped `now_ms`.
    pub async fn collect(
        drain: &dyn GenerationDrainStore,
        session_delete: &dyn SessionDeleteLedger,
        obligation_ledger: impl Fn(ObligationKind) -> Arc<dyn ObligationLedger>,
        registry: &dyn DeploymentRegistry,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> Result<Self, StoreError> {
        let draining_since_ms = drain
            .draining_generations()
            .await?
            .into_iter()
            .find(|marked| &marked.generation == generation)
            .map(|marked| marked.marked_at_ms);
        let work = drain.generation_work(generation).await?;
        let closing_sessions = session_delete.count_closing().await?;
        let unfinished_invocations =
            registry
                .unfinished_invocations(generation)
                .await
                .map_err(|error| StoreError::StorageFailure {
                    backend: "engine deployment registry",
                    message: error.to_string(),
                })?;
        let mut stalled_obligations = BTreeMap::new();
        for kind in ObligationKind::ALL {
            let count = obligation_ledger(kind).count_stalled().await?;
            stalled_obligations.insert(kind, count);
        }
        Ok(Self {
            generation: generation.clone(),
            draining_since_ms,
            live_processes: work.live_processes,
            parked_processes: work.parked_processes,
            parked_turns: work.parked_turns,
            in_flight_turns: work.in_flight_turns,
            closing_sessions,
            unfinished_invocations,
            stalled_obligations,
            checked_at: now_ms,
        })
    }

    /// True only when the generation is marked draining, it holds no live
    /// process, no parked process or turn and no in-flight turn, no session
    /// is closing, and
    /// no unfinished engine invocation is pinned to a deployment serving it.
    ///
    /// Stalled obligations do not hold it (ADR 0115 §3.5, FIG-4076). They
    /// are not pinned to any generation and none of them moves until an
    /// operator acts, so a drain that waited on them would never finish. An
    /// operator reads [`stalled_obligations`](Self::stalled_obligations) and
    /// settles each one before retiring the deployment.
    pub fn drained(&self) -> bool {
        self.draining_since_ms.is_some()
            && self.live_processes == 0
            && self.parked_processes == 0
            && self.parked_turns == 0
            && self.in_flight_turns == 0
            && self.closing_sessions == 0
            && self.unfinished_invocations == 0
    }
}

/// The serialized form carries the computed `drained` key, as the
/// deployment drain status's does.
impl serde::Serialize for GenerationDrainStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Wire<'a> {
            generation: &'a BuildGeneration,
            draining_since_ms: Option<u64>,
            live_processes: u64,
            parked_processes: u64,
            parked_turns: u64,
            in_flight_turns: u64,
            closing_sessions: u64,
            unfinished_invocations: u64,
            stalled_obligations: &'a BTreeMap<ObligationKind, u64>,
            checked_at: u64,
            drained: bool,
        }
        Wire {
            generation: &self.generation,
            draining_since_ms: self.draining_since_ms,
            live_processes: self.live_processes,
            parked_processes: self.parked_processes,
            parked_turns: self.parked_turns,
            in_flight_turns: self.in_flight_turns,
            closing_sessions: self.closing_sessions,
            unfinished_invocations: self.unfinished_invocations,
            stalled_obligations: &self.stalled_obligations,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked_and_empty() -> GenerationDrainStatus {
        GenerationDrainStatus {
            generation: BuildGeneration::parse("0123456789ab").expect("a generation"),
            draining_since_ms: Some(1),
            live_processes: 0,
            parked_processes: 0,
            parked_turns: 0,
            in_flight_turns: 0,
            closing_sessions: 0,
            unfinished_invocations: 0,
            stalled_obligations: ObligationKind::ALL
                .into_iter()
                .map(|kind| (kind, 0))
                .collect(),
            checked_at: 2,
        }
    }

    #[test]
    fn a_stalled_obligation_does_not_hold_the_drain() {
        let mut status = marked_and_empty();
        status
            .stalled_obligations
            .insert(ObligationKind::ArtifactCleanup, 1);
        assert!(status.drained(), "{status:?}");
        assert_eq!(
            serde_json::to_value(&status).expect("serialize")["drained"],
            true
        );
    }

    #[test]
    fn work_pinned_to_the_generation_holds_the_drain() {
        let holds: [fn(&mut GenerationDrainStatus); 7] = [
            |status| status.draining_since_ms = None,
            |status| status.live_processes = 1,
            |status| status.parked_processes = 1,
            |status| status.parked_turns = 1,
            |status| status.in_flight_turns = 1,
            |status| status.closing_sessions = 1,
            |status| status.unfinished_invocations = 1,
        ];
        for hold in holds {
            let mut status = marked_and_empty();
            hold(&mut status);
            assert!(!status.drained(), "{status:?}");
        }
    }
}
