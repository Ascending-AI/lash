use crate::{AwaitEventKey, RuntimeError};

/// Guard on the acquired lane. Opaque newtype over `SessionExecutionLeaseGuard`.
pub struct QueuedLaneGuard(crate::session_execution_lease::SessionExecutionLeaseGuard);

impl QueuedLaneGuard {
    pub fn new(guard: crate::session_execution_lease::SessionExecutionLeaseGuard) -> Self {
        Self(guard)
    }

    pub fn into_inner(self) -> crate::session_execution_lease::SessionExecutionLeaseGuard {
        self.0
    }
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
pub struct QueuedLaneHolder(crate::store::SessionExecutionLease);

/// The inner store row carries the lease token, which must never reach logs or panic messages.
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
    async fn pause(&self, slice: std::time::Duration);
}
