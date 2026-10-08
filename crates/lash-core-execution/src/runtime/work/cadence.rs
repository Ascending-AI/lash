use std::num::NonZeroUsize;
use std::time::Duration;

/// Pacing of registry waits.
#[derive(Clone, Debug)]
pub struct WorkCadencePolicy {
    pub poll_initial: Duration,
    pub poll_max: Duration,
}

impl WorkCadencePolicy {
    /// Standard work preset: poll from 25ms to 1s. These values are historical
    /// selections without supporting workload measurements.
    pub const fn standard() -> Self {
        Self::DEFAULT
    }

    /// Values of the standard work preset.
    pub const DEFAULT: Self = Self {
        poll_initial: Duration::from_millis(25),
        poll_max: Duration::from_secs(1),
    };

    /// Reject durations that would busy-spin a wait loop or
    /// violate its advertised maximum delay.
    pub fn validate(&self) -> Result<(), WorkCadenceError> {
        validate_non_zero_duration("work_cadence.poll_initial", self.poll_initial)?;
        validate_non_zero_duration("work_cadence.poll_max", self.poll_max)?;
        validate_initial_not_greater_than_max(
            "work_cadence.poll_initial",
            self.poll_initial,
            "work_cadence.poll_max",
            self.poll_max,
        )?;
        Ok(())
    }
}

impl Default for WorkCadencePolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// Invalid work pacing supplied by a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct WorkCadenceError(WorkCadenceErrorKind);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
enum WorkCadenceErrorKind {
    #[error("work pacing duration `{field}` must be greater than zero")]
    ZeroDuration { field: &'static str },
    #[error(
        "work pacing duration `{initial_field}` ({initial:?}) must not exceed `{max_field}` ({max:?})"
    )]
    InitialExceedsMax {
        initial_field: &'static str,
        initial: Duration,
        max_field: &'static str,
        max: Duration,
    },
}

fn validate_non_zero_duration(
    field: &'static str,
    duration: Duration,
) -> Result<(), WorkCadenceError> {
    if duration.is_zero() {
        return Err(WorkCadenceError(WorkCadenceErrorKind::ZeroDuration {
            field,
        }));
    }
    Ok(())
}

fn validate_initial_not_greater_than_max(
    initial_field: &'static str,
    initial: Duration,
    max_field: &'static str,
    max: Duration,
) -> Result<(), WorkCadenceError> {
    if initial > max {
        return Err(WorkCadenceError(WorkCadenceErrorKind::InitialExceedsMax {
            initial_field,
            initial,
            max_field,
            max,
        }));
    }
    Ok(())
}

/// Bounds of a process-local same-session commit admission queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitAdmissionPolicy {
    /// Maximum queued attempts for one session. The active attempt is separate.
    pub max_waiters: NonZeroUsize,
    /// How long an attempt may wait for admission.
    pub wait_ttl: Duration,
}

impl CommitAdmissionPolicy {
    /// Standard admission preset: 64 waiters per session, each waiting at
    /// most 30 seconds. These historical values have no workload measurement.
    pub const fn standard() -> Self {
        Self {
            max_waiters: NonZeroUsize::MIN.saturating_add(63),
            wait_ttl: Duration::from_secs(30),
        }
    }

    /// Reject a zero wait TTL before starting the runtime.
    pub fn validate(&self) -> Result<(), CommitAdmissionPolicyError> {
        if self.wait_ttl.is_zero() {
            return Err(CommitAdmissionPolicyError);
        }
        Ok(())
    }
}

impl Default for CommitAdmissionPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// A commit admission policy whose wait TTL is zero.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("commit admission wait TTL must be greater than zero")]
pub struct CommitAdmissionPolicyError;

/// A validated exponential polling or retry schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollPacing {
    initial: Duration,
    maximum: Duration,
}

impl PollPacing {
    /// Require positive delays with the initial delay at most the maximum.
    pub fn new(initial: Duration, maximum: Duration) -> Result<Self, WorkCadenceError> {
        validate_non_zero_duration("poll.initial", initial)?;
        validate_initial_not_greater_than_max("poll.initial", initial, "poll.maximum", maximum)?;
        Ok(Self { initial, maximum })
    }

    /// Standard observer preset: 25ms initial delay, doubling up to 1s.
    /// These historical values have no supporting workload measurement.
    pub const fn standard() -> Self {
        Self {
            initial: Duration::from_millis(25),
            maximum: Duration::from_secs(1),
        }
    }

    /// Standard terminal preset: 20ms initial delay, doubling up to 1s.
    /// These historical values have no supporting workload measurement.
    pub const fn terminal_standard() -> Self {
        Self {
            initial: Duration::from_millis(20),
            maximum: Duration::from_secs(1),
        }
    }

    /// Standard tool-fault retry preset: 10ms initial delay, doubling up to 1s.
    /// These historical values have no supporting workload measurement.
    pub const fn fault_standard() -> Self {
        Self {
            initial: Duration::from_millis(10),
            maximum: Duration::from_secs(1),
        }
    }

    pub const fn initial(self) -> Duration {
        self.initial
    }
    pub const fn maximum(self) -> Duration {
        self.maximum
    }
    pub fn next(self, current: Duration) -> Duration {
        current.saturating_mul(2).min(self.maximum)
    }
    /// Retry delay after `faults` previous failed attempts.
    pub fn after_faults(self, faults: u32) -> Duration {
        let mut delay = self.initial;
        // Even the full Duration representation saturates within 128 doublings.
        for _ in 0..faults.min(128) {
            delay = self.next(delay);
            if delay == self.maximum {
                break;
            }
        }
        delay
    }
}

impl Default for PollPacing {
    fn default() -> Self {
        Self::standard()
    }
}

/// Work chunk and tool-fault retry controls for a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimePacingPolicy {
    pub tool_fault_retry: PollPacing,
    pub checkpoint_inputs: NonZeroUsize,
}

impl RuntimePacingPolicy {
    /// Standard runtime preset: tool faults retry from 10ms to 1s and each
    /// checkpoint admits at most 64 inputs. No workload measurements back these values.
    pub const fn standard() -> Self {
        Self {
            tool_fault_retry: PollPacing::fault_standard(),
            checkpoint_inputs: NonZeroUsize::MIN.saturating_add(63),
        }
    }
}

impl Default for RuntimePacingPolicy {
    fn default() -> Self {
        Self::standard()
    }
}
