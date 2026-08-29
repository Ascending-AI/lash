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

pub(crate) fn record_provider_retry(provider: &str, kind: &'static str) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.provider.retries");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_provider_retry(provider, kind));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (provider, kind);
}

pub(crate) fn record_provider_throttle_wait(provider: &str, wait: Duration) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.provider.throttle_wait.duration");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_provider_throttle_wait(provider, wait));
    #[cfg(not(feature = "otel-trace"))]
    let _ = (provider, wait);
}

#[cfg(feature = "otel-trace")]
pub(crate) fn record_session_lane_contention_wait(wait: Duration, outcome: &'static str) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.session_execution_lane.contention_wait.duration");
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_session_lane_contention_wait(wait, outcome);
    });
}

pub(crate) fn record_session_lane_give_up(reason: &'static str) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.session_execution_lane.give_ups");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| metrics.record_session_lane_give_up(reason));
    #[cfg(not(feature = "otel-trace"))]
    let _ = reason;
}

pub(crate) fn record_queued_work_wake_retry() {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.queued_work.wake_retries");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(
        lash_trace::otel::RuntimeTuningMetrics::record_queued_work_wake_retry,
    );
}

pub(crate) fn record_postgres_pool_acquire_wait(wait: Duration, outcome: &'static str) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.postgres.pool.acquire_wait.duration");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_postgres_pool_acquire_wait(wait, outcome);
    });
    #[cfg(not(feature = "otel-trace"))]
    let _ = (wait, outcome);
}

pub(crate) fn record_runtime_commit_budgeted_size(bytes: usize, outcome: &'static str) {
    #[cfg(all(test, feature = "otel-trace"))]
    observe_test_metric("lash.runtime_commit.budgeted_size");
    #[cfg(feature = "otel-trace")]
    with_runtime_tuning_metrics(|metrics| {
        metrics.record_runtime_commit_budgeted_size(bytes, outcome);
    });
    #[cfg(not(feature = "otel-trace"))]
    let _ = (bytes, outcome);
}

#[cfg(all(test, feature = "otel-trace"))]
fn observe_test_metric(name: &'static str) {
    TEST_OBSERVATIONS.with(|slot| {
        if let Some(observations) = slot.borrow_mut().as_mut() {
            observations.push(name);
        }
    });
}

#[cfg(all(test, feature = "otel-trace"))]
thread_local! {
    static TEST_OBSERVATIONS: std::cell::RefCell<Option<Vec<&'static str>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, feature = "otel-trace"))]
pub(crate) struct TestMetrics;

#[cfg(all(test, feature = "otel-trace"))]
impl TestMetrics {
    pub(crate) fn install() -> Self {
        TEST_OBSERVATIONS.with(|slot| {
            assert!(
                slot.borrow_mut().replace(Vec::new()).is_none(),
                "test metrics already installed on this thread"
            );
        });
        Self
    }

    pub(crate) fn counter_value(&self, name: &str) -> u64 {
        self.observation_count(name)
    }

    pub(crate) fn histogram_count(&self, name: &str) -> u64 {
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

#[cfg(all(test, feature = "otel-trace"))]
impl Drop for TestMetrics {
    fn drop(&mut self) {
        TEST_OBSERVATIONS.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}
