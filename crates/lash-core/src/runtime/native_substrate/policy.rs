use std::time::Duration;

/// Pacing for the native substrate's own scheduler loops. Restate deployments
/// have no equivalent: on an engine tier the engine owns retry pacing, and
/// neither port implementation in `lash-restate` reads this.
#[derive(Clone, Debug, Default)]
pub struct NativeSubstrateConfig {
    pub worker_sweep: WorkerSweepPolicy,
    pub work_cadence: WorkCadencePolicy,
}

/// Pacing for native process-worklist intake and retry.
#[derive(Clone, Debug)]
pub struct WorkerSweepPolicy {
    pub intake_page: usize,
    pub fetch_attempts: usize,
    pub fetch_retry_base: Duration,
}

impl WorkerSweepPolicy {
    pub(crate) const DEFAULT: Self = Self {
        intake_page: 256,
        fetch_attempts: 3,
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
    pub max_transient_attempts: u32,
    pub slow_wake_threshold: Duration,
    pub poll_initial: Duration,
    pub poll_max: Duration,
    pub delivery_batch: usize,
    pub delivery_retry_initial: Duration,
    pub delivery_retry_max: Duration,
}

impl WorkCadencePolicy {
    pub(crate) const DEFAULT: Self = Self {
        retry_initial: Duration::from_millis(25),
        retry_max: Duration::from_secs(1),
        max_transient_attempts: 8,
        slow_wake_threshold: Duration::from_secs(30),
        poll_initial: Duration::from_millis(25),
        poll_max: Duration::from_secs(1),
        delivery_batch: 32,
        delivery_retry_initial: Duration::from_millis(50),
        delivery_retry_max: Duration::from_secs(5 * 60),
    };
}

impl Default for WorkCadencePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_substrate_defaults_match_the_existing_scheduler_constants() {
        let config = NativeSubstrateConfig::default();

        assert_eq!(config.worker_sweep.intake_page, 256);
        assert_eq!(config.worker_sweep.fetch_attempts, 3);
        assert_eq!(
            config.worker_sweep.fetch_retry_base,
            Duration::from_millis(10)
        );
        assert_eq!(config.work_cadence.retry_initial, Duration::from_millis(25));
        assert_eq!(config.work_cadence.retry_max, Duration::from_secs(1));
        assert_eq!(config.work_cadence.max_transient_attempts, 8);
        assert_eq!(
            config.work_cadence.slow_wake_threshold,
            Duration::from_secs(30)
        );
        assert_eq!(config.work_cadence.poll_initial, Duration::from_millis(25));
        assert_eq!(config.work_cadence.poll_max, Duration::from_secs(1));
        assert_eq!(config.work_cadence.delivery_batch, 32);
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
