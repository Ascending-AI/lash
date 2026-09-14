//! The durable session-execution lane a queued drain must own, and the
//! tool-intent submission gate that rides beside it.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

/// Guard on the acquired lane. Opaque newtype over `SessionExecutionLeaseGuard`.
pub struct QueuedLaneGuard(
    pub(super) crate::runtime::session_execution_lease::SessionExecutionLeaseGuard,
);

impl QueuedLaneGuard {
    pub(crate) fn new(
        guard: crate::runtime::session_execution_lease::SessionExecutionLeaseGuard,
    ) -> Self {
        Self(guard)
    }

    pub(crate) fn into_inner(
        self,
    ) -> crate::runtime::session_execution_lease::SessionExecutionLeaseGuard {
        self.0
    }
}

/// Opaque guard holding one facade tool-intent submission gate.
#[allow(dead_code)]
pub struct ToolIntentSubmissionGuard(tokio::sync::OwnedMutexGuard<()>);

impl ToolIntentSubmissionGuard {
    /// Wrap the owned mutex guard supplied by the facade's submission-gate
    /// collaborator.
    pub fn from_owned_mutex_guard(guard: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self(guard)
    }
}

/// Core-provided collaborator for durable tool-intent admission and outcome
/// recording. The effect-host seam never learns which registry or lock table
/// backs these operations.
#[async_trait::async_trait]
pub trait ToolIntentOutcomeSink: Send + Sync {
    async fn lock_submission_gate(&self, replay_key: &str) -> ToolIntentSubmissionGuard;

    async fn admit(
        &self,
        record: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, RuntimeError>;

    async fn complete_submission(
        &self,
        identity: &crate::ToolIntentIdentity,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;

    async fn retain_in_journal(
        &self,
        identity: &crate::ToolIntentIdentity,
        submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError>;
}

/// Preparation chosen by the effect host before one tool intent is realized.
pub enum ToolIntentPreparation {
    /// The controller journal owns the submission.
    ControllerOwned,
    /// The runtime registry owns the submission row and the gate remains held
    /// until realization and outcome recording finish.
    RuntimeOwned {
        admission: crate::ToolIntentSubmissionAdmission,
        _guard: ToolIntentSubmissionGuard,
    },
}

/// Whether a runtime-effect failure should end the controller invocation or
/// be recorded as an ordinary failed turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeEffectFailureDisposition {
    AbortInvocation,
    RecordTurnFailure,
}

/// Result of preparing an externally routable tool completion key.
pub enum CompletionKeyPreparation {
    NotNeeded,
    Unsupported,
    Issued(AwaitEventKey),
}

/// One attempt at the durable lane a queued drain must own, plus the facts a
/// bounded wait needs. Opaque: no store type, no lease timings, no guard
/// internals cross the seam.
#[derive(Clone)]
pub struct QueuedLaneHolder(pub(super) crate::store::SessionExecutionLease);

/// Prints only the [`QueuedLaneHolder::describe`] facts. The inner store row
/// carries the lease token, which must never reach logs or panic messages.
impl std::fmt::Debug for QueuedLaneHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueuedLaneHolder")
            .field(&self.describe())
            .finish()
    }
}

impl QueuedLaneHolder {
    pub fn new(holder: crate::store::SessionExecutionLease) -> Self {
        Self(holder)
    }

    pub fn lease(&self) -> &crate::store::SessionExecutionLease {
        &self.0
    }

    /// The holder's own persisted lease term. The only honest budget unit for
    /// waiting one out — see `native_substrate/lane_wait.rs`.
    pub fn lease_term_ms(&self) -> u64 {
        self.0.lease_term_ms
    }

    /// True iff this observation proves the holder renewed under an unchanged
    /// identity triple since `previous`: alive, not crashed.
    pub fn renewed_since(&self, previous: &QueuedLaneHolder) -> bool {
        self.0.owner == previous.0.owner
            && self.0.executor_id == previous.0.executor_id
            && self.0.expires_at_epoch_ms > previous.0.expires_at_epoch_ms
    }

    /// Describes the persisted holder identity and lease facts for diagnostics.
    pub fn describe(&self) -> String {
        format!(
            "owner `{}` incarnation `{}` executor `{}` (fencing generation {}, expires at {})",
            self.0.owner.owner_id,
            self.0.owner.incarnation_id,
            self.0.executor_id,
            self.0.fencing_token,
            self.0.expires_at_epoch_ms,
        )
    }
}

/// Result of one attempt to acquire the queued-work execution lane.
pub enum QueuedLaneAttempt {
    Acquired(QueuedLaneGuard),
    Busy(QueuedLaneHolder),
}

/// Result of applying a boundary's queued-lane acquisition policy.
pub enum QueuedLaneAcquisition {
    Acquired(QueuedLaneGuard),
    NotAcquired,
}

/// Core-provided probe. Owns everything it needs — `Arc<dyn RuntimePersistence>`,
/// copied owner/executor identity, copied `LeaseTimings`, `Arc<dyn Clock>` — so
/// it can cross an owned channel. The substrate decides how many times and how
/// long to try; it never learns what it is trying.
#[async_trait::async_trait]
pub trait QueuedLaneProbe: Send + Sync {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, RuntimeError>;
    /// Sleep `slice` through the runtime's injected clock.
    async fn pause(&self, slice: std::time::Duration);
}
