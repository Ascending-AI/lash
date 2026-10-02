//! Operational observations use the runtime's injected instruments.
//! Live work requires a body permit, transition counters require a committed
//! transition permit, gauges report current state, and physical resource
//! observations use a construction-time store observer without a permit.

use lash_trace::telemetry::metrics::TelemetryMetrics;
use lash_trace::{EmissionPermit, EmissionSource};
use std::time::Duration;

/// Instruments for physical store-resource observations, injected at construction.
#[derive(Clone, Default)]
pub struct StoreObserver {
    metrics: Option<TelemetryMetrics>,
}

impl std::fmt::Debug for StoreObserver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreObserver")
            .field("observed", &self.is_observed())
            .finish()
    }
}

impl StoreObserver {
    pub fn new(metrics: TelemetryMetrics) -> Self {
        Self {
            metrics: Some(metrics),
        }
    }

    pub fn is_observed(&self) -> bool {
        self.metrics.is_some()
    }

    pub fn pool_acquire_wait(&self, wait: Duration, outcome: &'static str) {
        if let Some(metrics) = &self.metrics {
            record_pool_acquire_wait(metrics, wait, outcome);
        }
    }

    pub fn recovery_leadership(&self, name: &str, leading: bool, term: u64) {
        if let Some(metrics) = &self.metrics {
            record_recovery_leadership(metrics, name, leading, term);
        }
    }
}

/// Live observation for `lash.provider.retries`.
pub fn record_provider_retry(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    provider: &str,
    kind: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.provider.retries");
    metrics.runtime_tuning.record_provider_retry(provider, kind);
}

/// Live observation for `lash.provider.throttle_wait.duration`.
pub fn record_provider_throttle_wait(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    provider: &str,
    wait: Duration,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.provider.throttle_wait.duration");
    metrics
        .runtime_tuning
        .record_provider_throttle_wait(provider, wait);
}

/// Live observation for `lash.session_execution_lane.contention_wait.duration`.
pub fn record_session_lane_contention_wait(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    wait: Duration,
    outcome: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.session_execution_lane.contention_wait.duration");
    metrics
        .runtime_tuning
        .record_session_lane_contention_wait(wait, outcome);
}

/// Live observation for `lash.session_execution_lane.give_ups`.
pub fn record_session_lane_give_up(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    reason: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.session_execution_lane.give_ups");
    metrics.runtime_tuning.record_session_lane_give_up(reason);
}

/// Live observation for `lash.queued_work.wake_retries`.
pub fn record_queued_work_wake_retry(metrics: &TelemetryMetrics, permit: Option<&EmissionPermit>) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.queued_work.wake_retries");
    metrics.runtime_tuning.record_queued_work_wake_retry();
}

/// Physical resource observation for `lash.store.pool.acquire_wait.duration`.
pub fn record_pool_acquire_wait(metrics: &TelemetryMetrics, wait: Duration, outcome: &'static str) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.store.pool.acquire_wait.duration");
    metrics
        .runtime_tuning
        .record_pool_acquire_wait(wait, outcome);
}

/// Transition observation for `lash.parked_work.parks`.
pub fn record_work_parked(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    kind: &'static str,
    reason: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::NewTransition)) {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.parked_work.parks");
    metrics.parked_work.record_park(kind, reason);
}

/// Gauge observation for `lash.parked_work.count`.
pub fn record_parked_work_count(
    metrics: &TelemetryMetrics,
    kind: &'static str,
    reason: &'static str,
    count: u64,
) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.parked_work.count");
    metrics.parked_work.record_count(kind, reason, count);
}

/// Gauge observation for `lash.parked_work.oldest_age`.
pub fn record_parked_work_oldest_age(metrics: &TelemetryMetrics, kind: &'static str, age_ms: u64) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.parked_work.oldest_age");
    metrics.parked_work.record_oldest_age(kind, age_ms);
}

/// Transition observation for `lash.obligation.attempts`.
pub fn record_obligation_attempt(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    kind: &'static str,
    outcome: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::NewTransition)) {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.obligation.attempts");
    metrics.obligations.record_attempt(kind, outcome);
}

/// Gauge observation for `lash.obligations.stalled`.
pub fn record_obligations_stalled(metrics: &TelemetryMetrics, kind: &'static str, count: u64) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.obligations.stalled");
    metrics.obligations.record_stalled(kind, count);
}

/// Gauge observation for `lash.generation_drain.work`.
pub fn record_generation_drain_work(
    metrics: &TelemetryMetrics,
    generation: &str,
    kind: &'static str,
    count: u64,
) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.generation_drain.work");
    metrics
        .generation_drain
        .record_work(generation, kind, count);
}

/// Physical lease observation for `lash.recovery_leader`.
pub fn record_recovery_leadership(
    metrics: &TelemetryMetrics,
    name: &str,
    leading: bool,
    term: u64,
) {
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.recovery_leader");
    metrics.obligations.record_leadership(name, leading, term);
}

/// Live observation for `lash.runtime_commit.budgeted_size`.
pub fn record_runtime_commit_budgeted_size(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    bytes: usize,
    outcome: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::LiveExecution { .. }))
    {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.runtime_commit.budgeted_size");
    metrics
        .runtime_tuning
        .record_runtime_commit_budgeted_size(bytes, outcome);
}

/// Transition observation for one newly committed tool-intent execution.
pub fn record_tool_intent_executed(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    kind: &'static str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::NewTransition)) {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.tool_intent.executed");
    metrics.tool_intent.record_executed(kind);
}

/// Transition observation for one newly committed tool-intent refusal.
pub fn record_tool_intent_refused(
    metrics: &TelemetryMetrics,
    permit: Option<&EmissionPermit>,
    kind: &'static str,
    reason: &str,
) {
    if !permit.is_some_and(|permit| matches!(permit.source(), EmissionSource::NewTransition)) {
        return;
    }
    #[cfg(any(test, feature = "testing"))]
    observe_test_metric("lash.tool_intent.refused");
    metrics.tool_intent.record_refused(kind, reason);
}

#[cfg(any(test, feature = "testing"))]
fn observe_test_metric(name: &'static str) {
    TEST_OBSERVATIONS.with(|slot| {
        if let Some(observations) = slot.borrow_mut().as_mut() {
            observations.push(name);
        }
    });
}

#[cfg(any(test, feature = "testing"))]
thread_local! {
    static TEST_OBSERVATIONS: std::cell::RefCell<Option<Vec<&'static str>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "testing"))]
pub struct TestMetrics;

#[cfg(any(test, feature = "testing"))]
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
                .unwrap_or_else(|| panic!("test metrics are installed"))
                .iter()
                .filter(|observed| **observed == name)
                .count() as u64
        })
    }
}

#[cfg(any(test, feature = "testing"))]
impl Drop for TestMetrics {
    fn drop(&mut self) {
        TEST_OBSERVATIONS.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_and_wrong_ownership_do_not_increment_operational_counters() {
        let observed = TestMetrics::install();
        let metrics = TelemetryMetrics::default();
        let live = EmissionPermit::live_execution(lash_trace::TraceAttemptId::new("body"));
        let transition = EmissionPermit::new_transition();
        record_provider_retry(&metrics, Some(&live), "provider", "throttle");
        record_provider_retry(&metrics, None, "provider", "throttle");
        record_provider_retry(&metrics, Some(&transition), "provider", "throttle");
        record_work_parked(&metrics, Some(&transition), "turn", "replay_divergence");
        record_work_parked(&metrics, None, "turn", "replay_divergence");
        record_work_parked(&metrics, Some(&live), "turn", "replay_divergence");
        record_obligation_attempt(&metrics, Some(&transition), "process_start", "delivered");
        record_obligation_attempt(&metrics, None, "process_start", "delivered");
        record_tool_intent_executed(&metrics, Some(&transition), "start_process");
        record_tool_intent_executed(&metrics, None, "start_process");
        record_tool_intent_executed(&metrics, Some(&live), "start_process");
        record_tool_intent_refused(
            &metrics,
            Some(&transition),
            "start_process",
            "command_failed",
        );
        record_tool_intent_refused(&metrics, None, "start_process", "command_failed");
        assert_eq!(observed.counter_value("lash.tool_intent.executed"), 1);
        assert_eq!(observed.counter_value("lash.tool_intent.refused"), 1);
        assert_eq!(observed.counter_value("lash.provider.retries"), 1);
        assert_eq!(observed.counter_value("lash.parked_work.parks"), 1);
        assert_eq!(observed.counter_value("lash.obligation.attempts"), 1);
    }

    #[test]
    fn physical_store_observations_use_only_the_injected_instruments() {
        let observed = TestMetrics::install();
        let disabled = StoreObserver::default();
        assert!(!disabled.is_observed());
        disabled.pool_acquire_wait(Duration::from_millis(1), "success");
        disabled.recovery_leadership("recovery:fixture", false, 0);
        assert_eq!(
            observed.histogram_count("lash.store.pool.acquire_wait.duration"),
            0
        );
        assert_eq!(observed.counter_value("lash.recovery_leader"), 0);
        let observer = StoreObserver::new(TelemetryMetrics::default());
        observer.pool_acquire_wait(Duration::from_millis(2), "success");
        observer.pool_acquire_wait(Duration::from_millis(3), "error");
        observer.recovery_leadership("recovery:fixture", true, 1);
        observer.recovery_leadership("recovery:fixture", false, 0);
        assert_eq!(
            observed.histogram_count("lash.store.pool.acquire_wait.duration"),
            2
        );
        assert_eq!(observed.counter_value("lash.recovery_leader"), 2);
    }

    #[test]
    fn current_state_gauges_observe_cleared_values() {
        let observed = TestMetrics::install();
        let metrics = TelemetryMetrics::default();
        record_parked_work_count(&metrics, "turn", "replay_divergence", 3);
        record_parked_work_count(&metrics, "turn", "replay_divergence", 0);
        record_parked_work_oldest_age(&metrics, "turn", 0);
        record_generation_drain_work(&metrics, "012345abcdef", "in_flight_turns", 0);
        assert_eq!(observed.counter_value("lash.parked_work.count"), 2);
        assert_eq!(observed.counter_value("lash.parked_work.oldest_age"), 1);
        assert_eq!(observed.counter_value("lash.generation_drain.work"), 1);
    }
}
