//! Runtime metrics with injected instruments and feature-independent no-op handles.

#[path = "registry.rs"]
pub mod registry;

#[cfg(feature = "otel")]
mod enabled {
    use super::registry::{AttributeKey, Metric};
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};

    const TOOL_INTENT_KIND_ATTRIBUTE: &str = AttributeKey::ToolIntentKind.definition().key;
    const TOOL_INTENT_REFUSAL_ATTRIBUTE: &str = AttributeKey::ToolIntentRefusal.definition().key;
    const PROVIDER_ATTRIBUTE: &str = AttributeKey::Provider.definition().key;
    const PROVIDER_RETRY_KIND_ATTRIBUTE: &str = AttributeKey::ProviderRetryKind.definition().key;
    const SESSION_LANE_WAIT_OUTCOME_ATTRIBUTE: &str =
        AttributeKey::LaneWaitOutcome.definition().key;
    const SESSION_LANE_GIVE_UP_ATTRIBUTE: &str = AttributeKey::LaneGiveUp.definition().key;
    const POSTGRES_POOL_ACQUIRE_OUTCOME_ATTRIBUTE: &str =
        AttributeKey::PoolAcquireOutcome.definition().key;
    const RUNTIME_COMMIT_BUDGET_OUTCOME_ATTRIBUTE: &str =
        AttributeKey::CommitBudgetOutcome.definition().key;
    const PARKED_WORK_KIND_ATTRIBUTE: &str = AttributeKey::ParkKind.definition().key;
    const PARKED_WORK_REASON_ATTRIBUTE: &str = AttributeKey::ParkReason.definition().key;
    const OBLIGATION_KIND_ATTRIBUTE: &str = AttributeKey::ObligationKind.definition().key;
    const OBLIGATION_OUTCOME_ATTRIBUTE: &str = AttributeKey::ObligationOutcome.definition().key;
    const RECOVERY_LEASE_ATTRIBUTE: &str = AttributeKey::RecoveryLease.definition().key;
    const GENERATION_DRAIN_GENERATION_ATTRIBUTE: &str =
        AttributeKey::DrainGeneration.definition().key;
    const GENERATION_DRAIN_KIND_ATTRIBUTE: &str = AttributeKey::DrainKind.definition().key;

    /// Runtime-facing OpenTelemetry instruments for host-tunable operational limits.
    #[derive(Clone)]
    pub struct RuntimeTuningMetrics {
        provider_retries: Counter<u64>,
        provider_throttle_wait_duration: Histogram<u64>,
        session_lane_contention_wait_duration: Histogram<u64>,
        session_lane_give_ups: Counter<u64>,
        queued_work_wake_retries: Counter<u64>,
        postgres_pool_acquire_wait_duration: Histogram<u64>,
        runtime_commit_budgeted_size: Histogram<u64>,
    }

    impl RuntimeTuningMetrics {
        pub fn new(meter: Meter) -> Self {
            Self {
                provider_retries: counter(&meter, Metric::ProviderRetries),
                provider_throttle_wait_duration: histogram(&meter, Metric::ProviderThrottleWait),
                session_lane_contention_wait_duration: histogram(
                    &meter,
                    Metric::LaneContentionWait,
                ),
                session_lane_give_ups: counter(&meter, Metric::LaneGiveUps),
                queued_work_wake_retries: counter(&meter, Metric::WakeRetries),
                postgres_pool_acquire_wait_duration: histogram(&meter, Metric::PoolAcquireWait),
                runtime_commit_budgeted_size: histogram(&meter, Metric::CommitBudgetedSize),
            }
        }

        pub fn record_provider_retry(&self, provider: &str, kind: &'static str) {
            self.provider_retries.add(
                1,
                &[
                    KeyValue::new(PROVIDER_ATTRIBUTE, provider.to_string()),
                    KeyValue::new(PROVIDER_RETRY_KIND_ATTRIBUTE, kind),
                ],
            );
        }

        pub fn record_provider_throttle_wait(&self, provider: &str, wait: std::time::Duration) {
            self.provider_throttle_wait_duration.record(
                duration_millis(wait),
                &[KeyValue::new(PROVIDER_ATTRIBUTE, provider.to_string())],
            );
        }

        pub fn record_session_lane_contention_wait(
            &self,
            wait: std::time::Duration,
            outcome: &'static str,
        ) {
            self.session_lane_contention_wait_duration.record(
                duration_millis(wait),
                &[KeyValue::new(SESSION_LANE_WAIT_OUTCOME_ATTRIBUTE, outcome)],
            );
        }

        pub fn record_session_lane_give_up(&self, reason: &'static str) {
            self.session_lane_give_ups
                .add(1, &[KeyValue::new(SESSION_LANE_GIVE_UP_ATTRIBUTE, reason)]);
        }

        pub fn record_queued_work_wake_retry(&self) {
            self.queued_work_wake_retries.add(1, &[]);
        }

        pub fn record_postgres_pool_acquire_wait(
            &self,
            wait: std::time::Duration,
            outcome: &'static str,
        ) {
            self.postgres_pool_acquire_wait_duration.record(
                duration_millis(wait),
                &[KeyValue::new(
                    POSTGRES_POOL_ACQUIRE_OUTCOME_ATTRIBUTE,
                    outcome,
                )],
            );
        }

        pub fn record_runtime_commit_budgeted_size(&self, bytes: usize, outcome: &'static str) {
            self.runtime_commit_budgeted_size.record(
                u64::try_from(bytes).unwrap_or(u64::MAX),
                &[KeyValue::new(
                    RUNTIME_COMMIT_BUDGET_OUTCOME_ATTRIBUTE,
                    outcome,
                )],
            );
        }
    }

    fn duration_millis(duration: std::time::Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }

    /// Runtime-facing OpenTelemetry instruments for parked work (FIG-3659).
    ///
    /// `kind` names the parked lane (`turn` today; `process` lands with NOW-B),
    /// `reason` the park's reason code.
    #[derive(Clone)]
    pub struct ParkedWorkMetrics {
        parks: Counter<u64>,
        count: Gauge<u64>,
        oldest_age: Gauge<u64>,
    }

    impl ParkedWorkMetrics {
        pub fn new(meter: Meter) -> Self {
            Self {
                parks: counter(&meter, Metric::Parks),
                count: gauge(&meter, Metric::ParkCount),
                oldest_age: gauge(&meter, Metric::ParkOldestAge),
            }
        }

        /// Count one park write — a first park or a same-turn re-park alike.
        pub fn record_park(&self, kind: &'static str, reason: &'static str) {
            self.parks.add(
                1,
                &[
                    KeyValue::new(PARKED_WORK_KIND_ATTRIBUTE, kind),
                    KeyValue::new(PARKED_WORK_REASON_ATTRIBUTE, reason),
                ],
            );
        }

        /// Report the live parked count for one (kind, reason) cell; zero-valued
        /// cells are recorded too so a cleared reason drops to 0.
        pub fn record_count(&self, kind: &'static str, reason: &'static str, count: u64) {
            self.count.record(
                count,
                &[
                    KeyValue::new(PARKED_WORK_KIND_ATTRIBUTE, kind),
                    KeyValue::new(PARKED_WORK_REASON_ATTRIBUTE, reason),
                ],
            );
        }

        /// Report the oldest live park's age in milliseconds; zero when nothing
        /// is parked.
        pub fn record_oldest_age(&self, kind: &'static str, age_ms: u64) {
            self.oldest_age
                .record(age_ms, &[KeyValue::new(PARKED_WORK_KIND_ATTRIBUTE, kind)]);
        }
    }

    /// Runtime-facing OpenTelemetry counters for tool-intent realization.
    #[derive(Clone)]
    pub struct ToolIntentMetrics {
        executed: Counter<u64>,
        refused: Counter<u64>,
    }

    impl ToolIntentMetrics {
        pub fn new(meter: Meter) -> Self {
            Self {
                executed: counter(&meter, Metric::IntentExecuted),
                refused: counter(&meter, Metric::IntentRefused),
            }
        }

        pub fn record_executed(&self, kind: &'static str) {
            self.executed
                .add(1, &[KeyValue::new(TOOL_INTENT_KIND_ATTRIBUTE, kind)]);
        }

        pub fn record_refused(&self, kind: &'static str, reason: &str) {
            self.refused.add(
                1,
                &[
                    KeyValue::new(TOOL_INTENT_KIND_ATTRIBUTE, kind),
                    KeyValue::new(TOOL_INTENT_REFUSAL_ATTRIBUTE, reason.to_owned()),
                ],
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::registry;
        use super::*;
        use opentelemetry::metrics::MeterProvider;
        use opentelemetry_sdk::metrics::{
            InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        };

        #[test]
        fn tool_intent_counters_export_registered_names_and_typed_dimensions() {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            let metrics = ToolIntentMetrics::new(
                provider.meter_with_scope(registry::instrumentation_scope()),
            );

            metrics.record_executed("start_process");
            metrics.record_refused("signal_process", "unsupported_protocol_version");
            provider.force_flush().expect("flush in-memory metrics");

            let exported = exporter
                .get_finished_metrics()
                .expect("read in-memory metrics");
            let mut names = exported
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .map(|metric| metric.name())
                .collect::<Vec<_>>();
            names.sort_unstable();
            assert_eq!(
                names,
                ["lash.tool_intent.executed", "lash.tool_intent.refused"]
            );
            let rendered = format!("{exported:?}");
            assert!(rendered.contains("lash.tool_intent.kind"));
            assert!(rendered.contains("start_process"));
            assert!(rendered.contains("lash.tool_intent.refusal_reason"));
            assert!(rendered.contains("unsupported_protocol_version"));
        }

        #[test]
        fn runtime_tuning_metrics_export_with_stable_names() {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            let metrics = RuntimeTuningMetrics::new(
                provider.meter_with_scope(registry::instrumentation_scope()),
            );

            metrics.record_provider_retry("test", "backoff");
            metrics.record_provider_throttle_wait("test", std::time::Duration::from_millis(10));
            metrics.record_session_lane_contention_wait(
                std::time::Duration::from_millis(20),
                "acquired",
            );
            metrics.record_session_lane_give_up("holder_is_alive");
            metrics.record_queued_work_wake_retry();
            metrics
                .record_postgres_pool_acquire_wait(std::time::Duration::from_millis(30), "success");
            metrics.record_runtime_commit_budgeted_size(40, "admitted");
            provider.force_flush().expect("flush in-memory metrics");

            let exported = exporter
                .get_finished_metrics()
                .expect("read in-memory metrics");
            use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
            let mut instruments = exported
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .map(|metric| {
                    let instrument_type = match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(_)) => "counter",
                        AggregatedMetrics::U64(MetricData::Histogram(_)) => "histogram",
                        data => panic!("unexpected aggregation for `{}`: {data:?}", metric.name()),
                    };
                    (metric.name(), instrument_type, metric.unit())
                })
                .collect::<Vec<_>>();
            instruments.sort_unstable();
            assert_eq!(
                instruments,
                [
                    (
                        "lash.postgres.pool.acquire_wait.duration",
                        "histogram",
                        "ms"
                    ),
                    ("lash.provider.retries", "counter", ""),
                    ("lash.provider.throttle_wait.duration", "histogram", "ms"),
                    ("lash.queued_work.wake_retries", "counter", ""),
                    ("lash.runtime_commit.budgeted_size", "histogram", "By"),
                    (
                        "lash.session_execution_lane.contention_wait.duration",
                        "histogram",
                        "ms"
                    ),
                    ("lash.session_execution_lane.give_ups", "counter", ""),
                ]
            );
        }
    }

    /// Store→engine delivery obligations and the recovery leader lease
    /// (ADR 0109 §1.5).
    #[derive(Clone)]
    pub struct ObligationMetrics {
        attempts: Counter<u64>,
        stalled: Gauge<u64>,
        leading: Gauge<u64>,
        term: Gauge<u64>,
    }

    impl ObligationMetrics {
        pub fn new(meter: Meter) -> Self {
            Self {
                attempts: counter(&meter, Metric::ObligationAttempts),
                stalled: gauge(&meter, Metric::ObligationsStalled),
                leading: gauge(&meter, Metric::RecoveryLeader),
                term: gauge(&meter, Metric::RecoveryTerm),
            }
        }

        /// Count one settled delivery attempt.
        pub fn record_attempt(&self, kind: &'static str, outcome: &'static str) {
            self.attempts.add(
                1,
                &[
                    KeyValue::new(OBLIGATION_KIND_ATTRIBUTE, kind),
                    KeyValue::new(OBLIGATION_OUTCOME_ATTRIBUTE, outcome),
                ],
            );
        }

        /// Report one kind's stalled count, including zero.
        pub fn record_stalled(&self, kind: &'static str, count: u64) {
            self.stalled
                .record(count, &[KeyValue::new(OBLIGATION_KIND_ATTRIBUTE, kind)]);
        }

        /// Report whether this process leads lease `name`, and its term.
        pub fn record_leadership(&self, name: &str, leading: bool, term: u64) {
            let attributes = [KeyValue::new(RECOVERY_LEASE_ATTRIBUTE, name.to_owned())];
            self.leading.record(u64::from(leading), &attributes);
            self.term.record(term, &attributes);
        }
    }

    /// Runtime-facing OpenTelemetry instruments for a build generation's drain
    /// (FIG-3884): the counts an operator polls while the generation runs down.
    #[derive(Clone)]
    pub struct GenerationDrainMetrics {
        work: Gauge<u64>,
    }

    impl GenerationDrainMetrics {
        pub fn new(meter: Meter) -> Self {
            Self {
                work: gauge(&meter, Metric::DrainWork),
            }
        }

        /// Report one (generation, kind) cell's count, including zero so a
        /// drained cell drops back. `kind` is one of `live_processes`,
        /// `parked_processes`, `parked_turns`, `in_flight_turns`.
        pub fn record_work(&self, generation: &str, kind: &'static str, count: u64) {
            self.work.record(
                count,
                &[
                    KeyValue::new(GENERATION_DRAIN_GENERATION_ATTRIBUTE, generation.to_owned()),
                    KeyValue::new(GENERATION_DRAIN_KIND_ATTRIBUTE, kind),
                ],
            );
        }
    }

    fn counter(meter: &Meter, metric: Metric) -> Counter<u64> {
        let def = metric.definition();
        meter.u64_counter(def.name).with_unit(def.unit).build()
    }
    fn histogram(meter: &Meter, metric: Metric) -> Histogram<u64> {
        let def = metric.definition();
        meter.u64_histogram(def.name).with_unit(def.unit).build()
    }
    fn gauge(meter: &Meter, metric: Metric) -> Gauge<u64> {
        let def = metric.definition();
        meter.u64_gauge(def.name).with_unit(def.unit).build()
    }
}
#[cfg(feature = "otel")]
pub use enabled::*;

#[cfg(not(feature = "otel"))]
mod disabled {
    #[derive(Clone, Default)]
    pub struct RuntimeTuningMetrics;
    impl RuntimeTuningMetrics {
        pub fn record_provider_retry(&self, provider: &str, kind: &'static str) {
            let _ = (provider, kind);
        }
        pub fn record_provider_throttle_wait(&self, provider: &str, wait: std::time::Duration) {
            let _ = (provider, wait);
        }
        pub fn record_session_lane_contention_wait(
            &self,
            wait: std::time::Duration,
            outcome: &'static str,
        ) {
            let _ = (wait, outcome);
        }
        pub fn record_session_lane_give_up(&self, reason: &'static str) {
            let _ = (reason,);
        }
        pub fn record_queued_work_wake_retry(&self) {}
        pub fn record_postgres_pool_acquire_wait(
            &self,
            wait: std::time::Duration,
            outcome: &'static str,
        ) {
            let _ = (wait, outcome);
        }
        pub fn record_runtime_commit_budgeted_size(&self, bytes: usize, outcome: &'static str) {
            let _ = (bytes, outcome);
        }
    }
    #[derive(Clone, Default)]
    pub struct ParkedWorkMetrics;
    impl ParkedWorkMetrics {
        pub fn record_park(&self, kind: &'static str, reason: &'static str) {
            let _ = (kind, reason);
        }
        pub fn record_count(&self, kind: &'static str, reason: &'static str, count: u64) {
            let _ = (kind, reason, count);
        }
        pub fn record_oldest_age(&self, kind: &'static str, age_ms: u64) {
            let _ = (kind, age_ms);
        }
    }
    #[derive(Clone, Default)]
    pub struct ToolIntentMetrics;
    impl ToolIntentMetrics {
        pub fn record_executed(&self, kind: &'static str) {
            let _ = (kind,);
        }
        pub fn record_refused(&self, kind: &'static str, reason: &str) {
            let _ = (kind, reason);
        }
    }
    #[derive(Clone, Default)]
    pub struct ObligationMetrics;
    impl ObligationMetrics {
        pub fn record_attempt(&self, kind: &'static str, outcome: &'static str) {
            let _ = (kind, outcome);
        }
        pub fn record_stalled(&self, kind: &'static str, count: u64) {
            let _ = (kind, count);
        }
        pub fn record_leadership(&self, name: &str, leading: bool, term: u64) {
            let _ = (name, leading, term);
        }
    }
    #[derive(Clone, Default)]
    pub struct GenerationDrainMetrics;
    impl GenerationDrainMetrics {
        pub fn record_work(&self, generation: &str, kind: &'static str, count: u64) {
            let _ = (generation, kind, count);
        }
    }
}
#[cfg(not(feature = "otel"))]
pub use disabled::*;

/// Instruments from the runtime's injected meter, or no-op handles when absent.
#[derive(Clone)]
pub struct TelemetryMetrics {
    pub runtime_tuning: RuntimeTuningMetrics,
    pub parked_work: ParkedWorkMetrics,
    pub tool_intent: ToolIntentMetrics,
    pub obligations: ObligationMetrics,
    pub generation_drain: GenerationDrainMetrics,
}
#[cfg(feature = "otel")]
impl TelemetryMetrics {
    /// Creates all instruments with Lash's versioned instrumentation scope.
    pub fn from_provider(provider: &impl opentelemetry::metrics::MeterProvider) -> Self {
        Self::new(provider.meter_with_scope(registry::instrumentation_scope()))
    }

    pub fn new(meter: opentelemetry::metrics::Meter) -> Self {
        Self {
            runtime_tuning: RuntimeTuningMetrics::new(meter.clone()),
            parked_work: ParkedWorkMetrics::new(meter.clone()),
            tool_intent: ToolIntentMetrics::new(meter.clone()),
            obligations: ObligationMetrics::new(meter.clone()),
            generation_drain: GenerationDrainMetrics::new(meter),
        }
    }
}
impl Default for TelemetryMetrics {
    fn default() -> Self {
        #[cfg(feature = "otel")]
        {
            // InstrumentProvider's defaults build no-op instruments without global state.
            struct Unobserved;
            impl opentelemetry::metrics::InstrumentProvider for Unobserved {}
            Self::new(opentelemetry::metrics::Meter::new(std::sync::Arc::new(
                Unobserved,
            )))
        }
        #[cfg(not(feature = "otel"))]
        {
            Self {
                runtime_tuning: RuntimeTuningMetrics,
                parked_work: ParkedWorkMetrics,
                tool_intent: ToolIntentMetrics,
                obligations: ObligationMetrics,
                generation_drain: GenerationDrainMetrics,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "otel"))]
    #[test]
    fn no_op_handles_require_no_exporter_or_runtime() {
        let metrics = TelemetryMetrics::default();
        assert_eq!(std::mem::size_of_val(&metrics), 0);
        metrics.tool_intent.record_executed("start_process");
        metrics.runtime_tuning.record_queued_work_wake_retry();
        metrics.parked_work.record_count("turn", "pending", 0);
        metrics.obligations.record_stalled("turn", 0);
        metrics
            .generation_drain
            .record_work("build", "parked_turns", 0);
    }

    #[cfg(feature = "otel")]
    #[test]
    fn injected_providers_export_the_registry_without_cross_talk() {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        use opentelemetry_sdk::metrics::{
            InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        };
        use registry::MetricKind;
        let first = InMemoryMetricExporter::default();
        let second = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(first.clone()).build())
            .build();
        let other = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(second.clone()).build())
            .build();
        let metrics = TelemetryMetrics::from_provider(&provider);
        let other_metrics = TelemetryMetrics::from_provider(&other);
        let wait = std::time::Duration::from_millis(10);
        metrics
            .runtime_tuning
            .record_provider_retry("test", "backoff");
        metrics
            .runtime_tuning
            .record_provider_throttle_wait("test", wait);
        metrics
            .runtime_tuning
            .record_session_lane_contention_wait(wait, "acquired");
        metrics.runtime_tuning.record_session_lane_give_up("busy");
        metrics.runtime_tuning.record_queued_work_wake_retry();
        metrics
            .runtime_tuning
            .record_postgres_pool_acquire_wait(wait, "success");
        metrics
            .runtime_tuning
            .record_runtime_commit_budgeted_size(42, "admitted");
        metrics.parked_work.record_park("turn", "pending");
        metrics.parked_work.record_count("turn", "pending", 1);
        metrics.parked_work.record_oldest_age("turn", 10);
        metrics.tool_intent.record_executed("start_process");
        metrics
            .tool_intent
            .record_refused("signal_process", "closed");
        metrics.obligations.record_attempt("turn", "success");
        metrics.obligations.record_stalled("turn", 1);
        metrics.obligations.record_leadership("recovery", true, 1);
        metrics
            .generation_drain
            .record_work("build", "parked_turns", 1);
        // Neither a different injected provider nor the default may receive these observations.
        other_metrics.tool_intent.record_executed("other_provider");
        TelemetryMetrics::default()
            .tool_intent
            .record_executed("default_noop");
        provider.force_flush().expect("flush first provider");
        other.force_flush().expect("flush second provider");
        let exported = first.get_finished_metrics().expect("first export");
        let mut actual = Vec::new();
        for resource in &exported {
            for scope in resource.scope_metrics() {
                assert_eq!(scope.scope().name(), registry::LASH_INSTRUMENTATION_NAME);
                assert_eq!(
                    scope.scope().version(),
                    Some(registry::LASH_INSTRUMENTATION_CONTRACT)
                );
                for metric in scope.metrics() {
                    let kind = match metric.data() {
                        AggregatedMetrics::U64(MetricData::Sum(_)) => MetricKind::Counter,
                        AggregatedMetrics::U64(MetricData::Histogram(_)) => MetricKind::Histogram,
                        AggregatedMetrics::U64(MetricData::Gauge(_)) => MetricKind::Gauge,
                        data => panic!("unexpected data: {data:?}"),
                    };
                    actual.push((metric.name(), metric.unit(), kind));
                }
            }
        }
        actual.sort_unstable_by_key(|entry| entry.0);
        let mut expected = registry::METRICS
            .iter()
            .map(|definition| (definition.name, definition.unit, definition.kind))
            .collect::<Vec<_>>();
        expected.sort_unstable_by_key(|entry| entry.0);
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 17);
        let isolated = second.get_finished_metrics().expect("second export");
        let names = isolated
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .map(|metric| metric.name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["lash.tool_intent.executed"]);
        let rendered = format!("{exported:?}");
        assert!(!rendered.contains("other_provider"));
        assert!(!rendered.contains("default_noop"));
    }
}
