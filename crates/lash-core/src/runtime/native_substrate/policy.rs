use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

/// Pacing for the native substrate's own scheduler loops. Restate deployments
/// have no equivalent: on an engine tier the engine owns retry pacing, and
/// neither port implementation in `lash-restate` reads this.
#[derive(Clone, Debug, Default)]
pub struct NativeSubstrateConfig {
    pub worker_sweep: WorkerSweepPolicy,
    pub work_cadence: WorkCadencePolicy,
}

impl NativeSubstrateConfig {
    /// Reject pacing durations that would busy-spin a native scheduler loop or
    /// violate a loop's advertised maximum delay.
    pub fn validate(&self) -> Result<(), NativeSubstrateConfigError> {
        validate_non_zero_duration(
            "worker_sweep.fetch_retry_base",
            self.worker_sweep.fetch_retry_base,
        )?;
        validate_non_zero_duration(
            "work_cadence.retry_initial",
            self.work_cadence.retry_initial,
        )?;
        validate_non_zero_duration("work_cadence.retry_max", self.work_cadence.retry_max)?;
        validate_initial_not_greater_than_max(
            "work_cadence.retry_initial",
            self.work_cadence.retry_initial,
            "work_cadence.retry_max",
            self.work_cadence.retry_max,
        )?;
        validate_non_zero_duration("work_cadence.poll_initial", self.work_cadence.poll_initial)?;
        validate_non_zero_duration("work_cadence.poll_max", self.work_cadence.poll_max)?;
        validate_initial_not_greater_than_max(
            "work_cadence.poll_initial",
            self.work_cadence.poll_initial,
            "work_cadence.poll_max",
            self.work_cadence.poll_max,
        )?;
        validate_millisecond_duration(
            "work_cadence.delivery_retry_initial",
            self.work_cadence.delivery_retry_initial,
        )?;
        validate_millisecond_duration(
            "work_cadence.delivery_retry_max",
            self.work_cadence.delivery_retry_max,
        )?;
        Ok(())
    }
}

/// Pacing for native process-worklist intake and retry.
#[derive(Clone, Debug)]
pub struct WorkerSweepPolicy {
    pub intake_page: NonZeroUsize,
    pub fetch_attempts: NonZeroUsize,
    pub fetch_retry_base: Duration,
}

impl WorkerSweepPolicy {
    pub(crate) const DEFAULT: Self = Self {
        intake_page: NonZeroUsize::new(256).unwrap(),
        fetch_attempts: NonZeroUsize::new(3).unwrap(),
        fetch_retry_base: Duration::from_millis(10),
    };
}

impl Default for WorkerSweepPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Pacing shared by native queued-work and wake-delivery scheduler loops.
#[derive(Clone, Debug)]
pub struct WorkCadencePolicy {
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub max_transient_attempts: NonZeroU32,
    pub slow_wake_threshold: Duration,
    pub poll_initial: Duration,
    pub poll_max: Duration,
    pub delivery_batch: NonZeroUsize,
    pub delivery_retry_initial: Duration,
    pub delivery_retry_max: Duration,
}

impl WorkCadencePolicy {
    pub(crate) const DEFAULT: Self = Self {
        retry_initial: Duration::from_millis(25),
        retry_max: Duration::from_secs(1),
        max_transient_attempts: NonZeroU32::new(8).unwrap(),
        slow_wake_threshold: Duration::from_secs(30),
        poll_initial: Duration::from_millis(25),
        poll_max: Duration::from_secs(1),
        delivery_batch: NonZeroUsize::new(32).unwrap(),
        delivery_retry_initial: Duration::from_millis(50),
        delivery_retry_max: Duration::from_secs(5 * 60),
    };
}

impl Default for WorkCadencePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Invalid native scheduler pacing supplied by a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct NativeSubstrateConfigError(NativeSubstrateConfigErrorKind);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
enum NativeSubstrateConfigErrorKind {
    #[error("native substrate pacing duration `{field}` must be greater than zero")]
    ZeroDuration { field: &'static str },
    #[error(
        "native substrate pacing duration `{field}` must be at least 1ms because wake retry timestamps have millisecond resolution, got {duration:?}"
    )]
    SubMillisecondDuration {
        field: &'static str,
        duration: Duration,
    },
    #[error(
        "native substrate pacing duration `{initial_field}` ({initial:?}) must not exceed `{max_field}` ({max:?})"
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
) -> Result<(), NativeSubstrateConfigError> {
    if duration.is_zero() {
        return Err(NativeSubstrateConfigError(
            NativeSubstrateConfigErrorKind::ZeroDuration { field },
        ));
    }
    Ok(())
}

fn validate_millisecond_duration(
    field: &'static str,
    duration: Duration,
) -> Result<(), NativeSubstrateConfigError> {
    if duration.as_millis() == 0 {
        return Err(NativeSubstrateConfigError(
            NativeSubstrateConfigErrorKind::SubMillisecondDuration { field, duration },
        ));
    }
    Ok(())
}

fn validate_initial_not_greater_than_max(
    initial_field: &'static str,
    initial: Duration,
    max_field: &'static str,
    max: Duration,
) -> Result<(), NativeSubstrateConfigError> {
    if initial > max {
        return Err(NativeSubstrateConfigError(
            NativeSubstrateConfigErrorKind::InitialExceedsMax {
                initial_field,
                initial,
                max_field,
                max,
            },
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_substrate_defaults_match_the_existing_scheduler_constants() {
        let config = NativeSubstrateConfig::default();

        assert_eq!(
            config.worker_sweep.intake_page,
            NonZeroUsize::new(256).unwrap()
        );
        assert_eq!(
            config.worker_sweep.fetch_attempts,
            NonZeroUsize::new(3).unwrap()
        );
        assert_eq!(
            config.worker_sweep.fetch_retry_base,
            Duration::from_millis(10)
        );
        assert_eq!(config.work_cadence.retry_initial, Duration::from_millis(25));
        assert_eq!(config.work_cadence.retry_max, Duration::from_secs(1));
        assert_eq!(
            config.work_cadence.max_transient_attempts,
            NonZeroU32::new(8).unwrap()
        );
        assert_eq!(
            config.work_cadence.slow_wake_threshold,
            Duration::from_secs(30)
        );
        assert_eq!(config.work_cadence.poll_initial, Duration::from_millis(25));
        assert_eq!(config.work_cadence.poll_max, Duration::from_secs(1));
        assert_eq!(
            config.work_cadence.delivery_batch,
            NonZeroUsize::new(32).unwrap()
        );
        assert_eq!(
            config.work_cadence.delivery_retry_initial,
            Duration::from_millis(50)
        );
        assert_eq!(
            config.work_cadence.delivery_retry_max,
            Duration::from_secs(5 * 60)
        );
    }
}
