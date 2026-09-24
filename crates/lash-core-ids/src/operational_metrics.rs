use std::time::Duration;

#[cfg(feature = "otel-trace")]
fn runtime_tuning_metrics() -> &'static lash_trace::otel::RuntimeTuningMetrics {
    static METRICS: std::sync::LazyLock<lash_trace::otel::RuntimeTuningMetrics> =
        std::sync::LazyLock::new(lash_trace::otel::RuntimeTuningMetrics::from_global_provider);
    &METRICS
}

#[cfg(feature = "otel-trace")]
fn with_runtime_tuning_metrics(record: impl Fn(&lash_trace::otel::RuntimeTuningMetrics)) {
    record(runtime_tuning_metrics());
}

#[cfg(feature = "otel-trace")]
fn parked_work_metrics() -> &'static lash_trace::otel::ParkedWorkMetrics {
    static METRICS: std::sync::LazyLock<lash_trace::otel::ParkedWorkMetrics> =
        std::sync::LazyLock::new(lash_trace::otel::ParkedWorkMetrics::from_global_provider);
    &METRICS
}

#[cfg(feature = "otel-trace")]
fn with_parked_work_metrics(record: impl Fn(&lash_trace::otel::ParkedWorkMetrics)) {
    record(parked_work_metrics());
}

pub fn record_provider_retry(provider: &str, kind: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.provider.retries");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_provider_retry(provider, kind));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (provider, kind);
}

pub fn record_provider_throttle_wait(provider: &str, wait: Duration) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.provider.throttle_wait.duration");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_provider_throttle_wait(provider, wait));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (provider, wait);
}

#[cfg(feature = "otel-trace")]
pub fn record_session_lane_contention_wait(wait: Duration, outcome: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.session_execution_lane.contention_wait.duration");
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_session_lane_contention_wait(wait, outcome);
    });
}

pub fn record_session_lane_give_up(reason: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.session_execution_lane.give_ups");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_session_lane_give_up(reason));
    #[cfg(not(feature = "otel-trace"))]
    let _ = reason;
}

pub fn record_queued_work_wake_retry() {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.queued_work.wake_retries");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(
        lash_trace::otel::RuntimeTuningMetrics::record_queued_work_wake_retry,
    );
}

pub fn record_postgres_pool_acquire_wait(wait: Duration, outcome: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.postgres.pool.acquire_wait.duration");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_postgres_pool_acquire_wait(wait, outcome);
    });
    #[cfg(not(feature = "otel-trace"))]
    let _ = (wait, outcome);
}

/// Count a successful park write — first park or same-turn re-park alike
/// (FIG-3659). Emitted only after the store durably records the park.
pub fn record_work_parked(kind: &'static str, reason: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.parked_work.parks");
    #[cfg(feature = "otel-trace")]
    with_parked_work_metrics(|metrics| metrics.record_park(kind, reason));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (kind, reason);
}

/// Report the live parked count for one (kind, reason) cell, including zero
/// so a cleared reason does not go stale (FIG-3659).
pub fn record_parked_work_count(kind: &'static str, reason: &'static str, count: u64) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.parked_work.count");
    #[cfg(feature = "otel-trace")]
    with_parked_work_metrics(|metrics| metrics.record_count(kind, reason, count));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (kind, reason, count);
}

/// Report the oldest live park's age in milliseconds; zero when nothing is
/// parked (FIG-3659).
pub fn record_parked_work_oldest_age(kind: &'static str, age_ms: u64) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.parked_work.oldest_age");
    #[cfg(feature = "otel-trace")]
    with_parked_work_metrics(|metrics| metrics.record_oldest_age(kind, age_ms));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (kind, age_ms);
}

pub fn record_runtime_commit_budgeted_size(bytes: usize, outcome: &'static str) {
    #[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
    observe_test_metric("lash.runtime_commit.budgeted_size");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_runtime_commit_budgeted_size(bytes, outcome);
    });
    #[cfg(not(feature = "otel-trace"))]
    let _ = (bytes, outcome);
}

#[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
fn observe_test_metric(name: &'static str) {
    TEST_OBSERVATIONS.with(|slot| {
        if let Some(observations) = slot.borrow_mut().as_mut() {
            observations.push(name);
        }
    });
}

#[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
thread_local! {
    static TEST_OBSERVATIONS: std::cell::RefCell<Option<Vec<&'static str>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
pub struct TestMetrics;

#[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
impl TestMetrics {
    pub fn install() -> Self {
        TEST_OBSERVATIONS.with(|slot| {
            assert!(
                slot.borrow_mut().replace(Vec::new()).is_none(),
                "test metrics already installed on this thread"
            );
        });
        Self
    }

    pub fn counter_value(&self, name: &str) -> u64 {
        self.observation_count(name)
    }

    pub fn histogram_count(&self, name: &str) -> u64 {
        self.observation_count(name)
    }

    fn observation_count(&self, name: &str) -> u64 {
        TEST_OBSERVATIONS.with(|slot| {
            slot.borrow()
                .as_ref()
                .expect("test metrics are installed")
                .iter()
                .filter(|observed| **observed == name)
                .count() as u64
        })
    }
}

#[cfg(all(any(test, feature = "testing"), feature = "otel-trace"))]
impl Drop for TestMetrics {
    fn drop(&mut self) {
        TEST_OBSERVATIONS.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(all(test, feature = "otel-trace"))]
mod tests {
    use super::*;

    #[test]
    fn parked_work_shims_emit_the_fig_3659_metric_names() {
        let metrics = TestMetrics::install();

        record_work_parked("turn", "replay_divergence");
        record_parked_work_count("turn", "replay_divergence", 3);
        record_parked_work_oldest_age("turn", 42);

        assert_eq!(metrics.counter_value("lash.parked_work.parks"), 1);
        assert_eq!(metrics.counter_value("lash.parked_work.count"), 1);
        assert_eq!(metrics.counter_value("lash.parked_work.oldest_age"), 1);
    }
}
