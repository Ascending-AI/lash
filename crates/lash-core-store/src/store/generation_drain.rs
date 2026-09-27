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

use super::session_delete::SessionDeleteLedger;
use super::{ObligationKind, ObligationLedger, StoreError};
use crate::ProcessId;
use crate::build_generation::BuildGeneration;

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
    /// operator.
    pub parked_processes: u64,
    /// Parked turns whose parked checkpoint the generation wrote.
    pub parked_turns: u64,
    /// Sessions whose admitted drive — the queued run its admission stamped
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
}

/// What one build generation still holds while it drains (FIG-3799): the
/// read an operator polls after marking it draining until the generation's
/// deployment can be retired.
///
/// Lash counts the durable work pinned to the generation: live processes
/// whose current segment it admitted — including one whose segment handed
/// over to a successor that has not started yet — the parked processes and
/// turns whose checkpoint it wrote, and the turns its drives admitted that
/// have not settled (FIG-3884). The recovery leader wakes the live
/// processes, each hands its open wait to a successor on the newest build,
/// and the count runs down. Parked work does not move by itself: an
/// operator redrives it on a build of the generation, cancels it, or forks
/// it. Stalled obligations are counted as in the deployment drain status:
/// they are not per generation, but a drain is not finished while one waits
/// for an operator. Nor is it while a session is closing: its close ended its
/// roots, but each root's turn-control waits stay registered with the
/// engine, on whichever build ran the root, until the session's physical
/// delete revokes them (ADR 0109 §4).
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
    /// Turns the generation's drives admitted that have not settled: the
    /// pending queued runs stamped `admitted_generation` (FIG-3795 S9,
    /// FIG-3884).
    pub in_flight_turns: u64,
    /// Sessions closing: their close committed and their physical delete,
    /// which revokes the waits their roots registered with the engine, has
    /// not run. Not per generation, as stalled obligations are not.
    pub closing_sessions: u64,
    /// Stalled store→engine delivery obligations per kind (ADR 0109 §1.5),
    /// every kind present, zero included.
    pub stalled_obligations: BTreeMap<ObligationKind, u64>,
    /// Host-clock epoch milliseconds at which this read completed.
    pub checked_at: u64,
}

impl GenerationDrainStatus {
    /// Compose the status over `drain`'s reads, `session_delete`'s closing
    /// sessions and the stalled counts of the ledgers `obligation_ledger`
    /// hands out, stamped `now_ms`.
    ///
    /// Reporting is part of the read: the per-generation work gauges and
    /// each kind's stalled count record here, so polling this status is also
    /// the metrics refresh (FIG-3884).
    pub async fn collect(
        drain: &dyn GenerationDrainStore,
        session_delete: &dyn SessionDeleteLedger,
        obligation_ledger: impl Fn(ObligationKind) -> Arc<dyn ObligationLedger>,
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
        let label = generation.as_str();
        crate::operational_metrics::record_generation_drain_work(
            label,
            "live_processes",
            work.live_processes,
        );
        crate::operational_metrics::record_generation_drain_work(
            label,
            "parked_processes",
            work.parked_processes,
        );
        crate::operational_metrics::record_generation_drain_work(
            label,
            "parked_turns",
            work.parked_turns,
        );
        crate::operational_metrics::record_generation_drain_work(
            label,
            "in_flight_turns",
            work.in_flight_turns,
        );
        let closing_sessions = session_delete.count_closing().await?;
        let mut stalled_obligations = BTreeMap::new();
        for kind in ObligationKind::ALL {
            let count = obligation_ledger(kind).count_stalled().await?;
            crate::operational_metrics::record_obligations_stalled(kind.label(), count);
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
            stalled_obligations,
            checked_at: now_ms,
        })
    }

    /// True only when the generation is marked draining and it holds no live
    /// process, no parked process or turn, no in-flight turn, no session is
    /// closing, and no obligation is stalled.
    pub fn drained(&self) -> bool {
        self.draining_since_ms.is_some()
            && self.live_processes == 0
            && self.parked_processes == 0
            && self.parked_turns == 0
            && self.in_flight_turns == 0
            && self.closing_sessions == 0
            && self.stalled_obligations.values().all(|count| *count == 0)
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
            stalled_obligations: &self.stalled_obligations,
            checked_at: self.checked_at,
            drained: self.drained(),
        }
        .serialize(serializer)
    }
}
