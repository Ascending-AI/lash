use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::PluginError;

static RETRY_JITTER_SEQUENCE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

/// Apply 80-120% multiplicative jitter inside an explicit retry envelope.
///
/// The sequence is process-local and affects pacing only. It is deliberately
/// independent of durable identities, replay, and the store's authority.
#[doc(hidden)]
pub fn bounded_multiplicative_jitter(
    base: Duration,
    floor: Duration,
    ceiling: Duration,
) -> Duration {
    debug_assert!(floor <= ceiling);
    let sequence = RETRY_JITTER_SEQUENCE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    // SplitMix64 finalization gives adjacent calls unrelated low bits without
    // adding a random-number dependency to the runtime kernel.
    let mut mixed = sequence;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    let percent = 80 + mixed % 41;
    let jittered_nanos = base.as_nanos().saturating_mul(u128::from(percent)) / 100;
    let bounded_nanos = jittered_nanos.clamp(floor.as_nanos(), ceiling.as_nanos());
    Duration::from_nanos(bounded_nanos.min(u64::MAX.into()) as u64)
}

#[cfg(test)]
mod jitter_tests {
    use super::*;

    #[test]
    fn retry_jitter_stays_inside_the_production_envelope() {
        let policy = WorkCadencePolicy::DEFAULT;
        for base in [
            Duration::from_millis(25),
            Duration::from_millis(100),
            Duration::from_millis(500),
            Duration::from_secs(1),
        ] {
            for _ in 0..128 {
                let delay =
                    bounded_multiplicative_jitter(base, policy.retry_initial, policy.retry_max);
                assert!(delay >= policy.retry_initial);
                assert!(delay <= policy.retry_max);
                if base < policy.retry_max {
                    assert!(delay >= base.mul_f64(0.8));
                    assert!(delay <= base.mul_f64(1.2));
                }
            }
        }
    }
}
#[cfg(any(test, feature = "testing"))]
use crate::WorkCadencePolicy;

/// Compatibility view of the default policy's transient-attempt budget.
/// Runtime loops read their configured [`WorkCadencePolicy`] instead.
#[cfg(any(test, feature = "testing"))]
pub const QUEUED_WORK_MAX_TRANSIENT_ATTEMPTS: usize =
    WorkCadencePolicy::DEFAULT.max_transient_attempts.get() as usize;

/// Default maximum number of queued-work wake executions admitted at once.
pub const DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY: usize = 64;

#[derive(Clone, Debug)]
pub struct QueuedWorkRunRequest {
    pub session_id: Option<String>,
    pub reason: String,
    pub trace_idle: bool,
}

impl QueuedWorkRunRequest {
    pub(super) fn new(
        session_id: Option<String>,
        reason: impl Into<String>,
        trace_idle: bool,
    ) -> Self {
        Self {
            session_id,
            reason: reason.into(),
            trace_idle,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkRunErrorClass {
    Transient,
    Terminal,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{error}")]
pub struct QueuedWorkRunError {
    pub class: QueuedWorkRunErrorClass,
    pub error: PluginError,
}

impl QueuedWorkRunError {
    pub fn transient(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Transient,
            error,
        }
    }

    pub fn terminal(error: PluginError) -> Self {
        Self {
            class: QueuedWorkRunErrorClass::Terminal,
            error,
        }
    }
}

impl From<PluginError> for QueuedWorkRunError {
    fn from(error: PluginError) -> Self {
        if matches!(&error, PluginError::Runtime(error) if error.is_retryable()) {
            Self::transient(error)
        } else {
            Self::terminal(error)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkWakeOutcome {
    Retrying,
    Terminal,
    Exhausted,
}

/// Operational evidence that a best-effort queued-work wake needs retry.
///
/// A wake failure is never an enqueue failure: the input is already durable,
/// and transient failures re-enter the idempotent pending-work claim path up
/// to the driver's bounded retry limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeFailure {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub retry_after_ms: u64,
    pub disposition: QueuedWorkWakeOutcome,
    pub error: String,
}

/// Operational evidence that an admitted queued-work wake remains unfinished.
///
/// This event is observational only. It never cancels or times out the wake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkSlowWake {
    pub session_id: Option<String>,
    pub reason: String,
    pub attempt: u32,
    pub threshold_ms: u64,
    pub available_permits: Option<usize>,
    pub admission_limit: Option<usize>,
}

/// Repeating operational evidence that queued work is blocked by a live
/// session execution lease.
///
/// This event is observational only. The inline driver must fully hydrate the
/// runtime before it can distinguish a blocked claim from an idle queue, so
/// one hydration per bounded contention poll is the current floor. The cheap
/// pre-hydration peek deliberately remains a conservative queue predicate; it
/// does not expose lease state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedWorkWakeContended {
    pub session_id: Option<String>,
    pub reason: String,
    pub contended_passes: u32,
    pub contended_ms: u64,
    pub threshold_ms: u64,
    pub available_permits: Option<usize>,
    pub admission_limit: Option<usize>,
}

/// Whether one queued-work pass actually claimed durable work.
///
/// The inline reference driver reports this so a positive conservative peek
/// followed by a live session-lease conflict backs off instead of rehydrating
/// eagerly. External engine submitters may retain the default `Unknown` result;
/// Lash never re-drives engine-owned work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedWorkRunProgress {
    Unknown,
    Claimed,
    Blocked,
}

#[async_trait::async_trait]
pub trait QueuedWorkRunHandle: Send + Sync {
    /// Cheap durable check performed before runtime hydration.
    ///
    /// `Some(true)` admits a run and rechecks afterward until the durable queue
    /// is idle; `Some(false)` skips hydration. `None` preserves single-pass
    /// behavior for external handles without this persistence seam. The
    /// embedded Lash adapter returns `Some` from a real
    /// [`SessionStoreFactory`](crate::SessionStoreFactory) read.
    async fn peek_claimable_queued_work(
        &self,
        _session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        Ok(None)
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError>;

    /// Host-driven single pass: claim and submit ready queued work, optionally
    /// narrowed to one session. The symmetric counterpart to
    /// [`ProcessWorkSubstrate::admit_pending_processes`](crate::ProcessWorkSubstrate::admit_pending_processes).
    ///
    /// Idempotency is the store scheduler's job, not a same-process memory
    /// guard. Hosts call this on an event (enqueue, process wake, turn
    /// completion) instead of polling.
    async fn claim_and_run_pending(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<(), QueuedWorkRunError> {
        let request =
            QueuedWorkRunRequest::new(session_id.map(str::to_string), reason.to_string(), false);
        self.run_queued_work(request).await
    }

    /// Run one pass and report whether the pass claimed durable work.
    ///
    /// External handles keep the single-pass default. The inline reference
    /// handle overrides this to distinguish progress from lease contention.
    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.claim_and_run_pending(session_id, reason).await?;
        Ok(QueuedWorkRunProgress::Unknown)
    }
}
