//! The seams a worker pool implements for the broker: one checked-out
//! worker's byte transport, and the slots workers are checked out of.
//!
//! The broker never spawns, reaps or resets a worker itself. It checks one
//! out for a run, speaks the protocol over its transport, and hands it back
//! either clean ([`WorkerSlots::release`]: the pool resets and reuses it) or
//! failed ([`WorkerSlots::discard`]: the pool kills, reaps and replaces it).
//!
//! # Slot release
//!
//! A slot is an *active* checkout: a worker running model code for one owner.
//! A run that awaits a parent effect needing a worker of its own (nested
//! compilation, a process body started from a cell) parks: the worker
//! serializes its VM on the broker's [`Park`](lash_vm_protocol::ParentMessage::Park)
//! and the broker releases its slot before performing the effect, then checks
//! a worker out again to resume the run from the parked state. A run never
//! holds a slot while it waits on work that needs one, so a pool of one
//! worker cannot deadlock on itself.

use std::time::Duration;

use lash_vm_protocol::{ExecutionLease, InfrastructureOutcome, SupervisorEvidence, VmOwner};

/// What a transport read yields.
#[derive(Debug, PartialEq, Eq)]
pub enum WorkerRead {
    /// The next bytes the worker wrote, in order. Frame boundaries are the
    /// broker's to find.
    Bytes(Vec<u8>),
    /// A classified failure from the supervising pool.
    Failed(InfrastructureOutcome),
    /// The stream ended, with the supervisor's evidence of how. A worker
    /// never testifies to its own end.
    Ended(SupervisorEvidence),
    /// The worker sent nothing for longer than the pool's no-response
    /// watchdog allows while it owed the parent a frame. The watchdog pauses
    /// while the parent performs an effect, and it is not a guest execution
    /// limit.
    Unresponsive { silent_ms: u64 },
}

/// One checked-out worker's transport.
#[async_trait::async_trait]
pub trait WorkerTransport: Send {
    /// Writes one encoded parent frame. A worker that can no longer be
    /// written to answers the supervisor's evidence of its end.
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), SupervisorEvidence>;

    /// The worker's next bytes, or the end of its stream. Cancel-safe: the
    /// broker races it against the parent's own work, and a read dropped
    /// before it completes loses no bytes.
    async fn recv(&mut self) -> WorkerRead;

    /// Kills the worker and reaps it, answering how it ended. Idempotent.
    async fn kill(&mut self) -> SupervisorEvidence;
}

/// One worker checked out for one run, under its own lease.
pub struct WorkerCheckout {
    pub lease: ExecutionLease,
    pub transport: Box<dyn WorkerTransport>,
}

impl std::fmt::Debug for WorkerCheckout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerCheckout")
            .field("lease", &self.lease)
            .finish_non_exhaustive()
    }
}

/// Why no worker could be checked out.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckoutRefusal {
    #[error("the pool's queue is full")]
    QueueFull,
    #[error(transparent)]
    Infrastructure(InfrastructureOutcome),
    #[error("no worker was free within {waited:?}")]
    TimedOut { waited: Duration },
    #[error("the pool is failing queued work after repeated worker failures")]
    RestartStorm,
    #[error("the pool is shut down")]
    Closed,
}

impl CheckoutRefusal {
    /// The typed infrastructure outcome a refused checkout surfaces as: the
    /// run never started, so the owning invocation is re-driven.
    pub fn outcome(&self) -> InfrastructureOutcome {
        if let Self::Infrastructure(outcome) = self {
            return outcome.clone();
        }
        InfrastructureOutcome::WorkerUnresponsive {
            silent_ms: match self {
                Self::TimedOut { waited } => u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                Self::QueueFull | Self::RestartStorm | Self::Closed | Self::Infrastructure(_) => 0,
            },
        }
    }
}

/// The slots workers are checked out of: the scheduling seam a pool
/// implements (see the module docs).
#[async_trait::async_trait]
pub trait WorkerSlots: Send + Sync {
    /// Checks out a worker for `owner`, waiting no longer than the pool's
    /// checkout bound.
    async fn checkout(
        &self,
        owner: &VmOwner,
        start: &lash_vm_protocol::Start,
    ) -> Result<WorkerCheckout, CheckoutRefusal>;

    /// Returns a worker whose run ended cleanly (completed, failed as a
    /// guest, or parked): the pool resets it and may reuse it.
    async fn release(&self, checkout: WorkerCheckout) -> Result<(), CheckoutRefusal>;

    /// Returns a worker after any failure: the pool kills and reaps it and
    /// replaces it with a fresh one. A failed worker is never reset.
    async fn discard(&self, checkout: WorkerCheckout) -> Result<(), CheckoutRefusal>;
}
