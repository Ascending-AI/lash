//! The durable record of one queued logical run: its admission, its progress
//! and its settlement. The run record outlives the queue that fed it; its
//! members are ingress items (ADR 0101).

use super::{
    BeginQueuedRun, QueuedRunAdmission, QueuedRunCommit, SessionExecutionLeaseAuthority, StoreError,
};
use crate::SessionId;

/// Durable queued-run capability: one unfinished run per session, admitted,
/// resumed and settled under the caller's fence.
#[async_trait::async_trait]
pub trait QueuedRunStore: Send + Sync {
    /// Acquire or resume the session's sole unfinished queued run under the
    /// current lane fence. Retry preserves identity and physical position.
    async fn begin_or_resume_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        request: BeginQueuedRun,
    ) -> Result<QueuedRunAdmission, StoreError>;

    /// Pending-run discovery remains available after original members settle.
    async fn pending_queued_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<QueuedRunAdmission>, StoreError>;

    /// The admission recorded for the drain `scope`, pending or settled, or
    /// `None` when that drain never admitted a run (or forgot an unworked
    /// one).
    ///
    /// A read, never a claim: it takes no lane and admits nothing. A settled
    /// run is a drain end (ADR 0094, FIG-3419/3559), so this is how the
    /// parent-end recovery sweep tells a drain whose end is owed — `terminal`
    /// is recorded but the end receipt is not — from one that is merely
    /// interrupted and ends through its own retry (FIG-3563).
    async fn queued_run(
        &self,
        scope: &crate::ExecutionScope,
    ) -> Result<Option<QueuedRunAdmission>, StoreError>;

    /// Fenced disposition for an empty run or a failure before physical commit.
    async fn settle_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        settlement: QueuedRunCommit,
    ) -> Result<QueuedRunAdmission, StoreError>;
}
