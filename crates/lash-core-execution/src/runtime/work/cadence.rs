use std::num::NonZeroUsize;
use std::time::Duration;

/// Pacing of the wake-delivery driver and of registry waits.
#[derive(Clone, Debug)]
pub struct WorkCadencePolicy {
    pub poll_initial: Duration,
    pub poll_max: Duration,
    pub delivery_batch: NonZeroUsize,
    pub delivery_retry_initial: Duration,
    pub delivery_retry_max: Duration,
}

impl WorkCadencePolicy {
    pub const DEFAULT: Self = Self {
        poll_initial: Duration::from_millis(25),
        poll_max: Duration::from_secs(1),
        delivery_batch: NonZeroUsize::new(32).unwrap(),
        delivery_retry_initial: Duration::from_millis(50),
        delivery_retry_max: Duration::from_secs(5 * 60),
    };

    /// Reject durations that would busy-spin a wake-delivery or wait loop or
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
        validate_millisecond_duration(
            "work_cadence.delivery_retry_initial",
            self.delivery_retry_initial,
        )?;
        validate_millisecond_duration("work_cadence.delivery_retry_max", self.delivery_retry_max)?;
        Ok(())
    }
}

impl Default for WorkCadencePolicy {
    fn default() -> Self {
        Self::DEFAULT
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
        "work pacing duration `{field}` must be at least 1ms because wake retry timestamps have millisecond resolution, got {duration:?}"
    )]
    SubMillisecondDuration {
        field: &'static str,
        duration: Duration,
    },
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

fn validate_millisecond_duration(
    field: &'static str,
    duration: Duration,
) -> Result<(), WorkCadenceError> {
    if duration.as_millis() == 0 {
        return Err(WorkCadenceError(
            WorkCadenceErrorKind::SubMillisecondDuration { field, duration },
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_cadence_defaults_match_the_wait_and_delivery_constants() {
        let cadence = WorkCadencePolicy::default();

        assert_eq!(cadence.poll_initial, Duration::from_millis(25));
        assert_eq!(cadence.poll_max, Duration::from_secs(1));
        assert_eq!(cadence.delivery_batch, NonZeroUsize::new(32).unwrap());
        assert_eq!(cadence.delivery_retry_initial, Duration::from_millis(50));
        assert_eq!(cadence.delivery_retry_max, Duration::from_secs(5 * 60));
    }
}
