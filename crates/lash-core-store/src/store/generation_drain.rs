//! A build generation's drain (FIG-3799, ADR 0106 §1).
//!
//! An operator marks a generation draining when the build that runs it is
//! being retired. The mark is a store fact, so whichever deployment leads
//! recovery reads it: the leader's hand-over duty wakes every live process
//! whose current segment that generation admitted, each hands its open wait
//! to a successor on the newest build, and the generation's work runs down
//! to zero. The same port answers what is left of a generation's work, the
//! numbers an operator polls until the generation is drained.

use std::num::NonZeroUsize;

use super::StoreError;
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
