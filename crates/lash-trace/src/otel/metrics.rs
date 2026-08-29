use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry::{KeyValue, global};

const INSTRUMENTATION_NAME: &str = "lash-trace";
const WORKER_KIND_ATTRIBUTE: &str = "lash.worker.kind";
const WORKER_ID_ATTRIBUTE: &str = "lash.worker.id";
const TOOL_INTENT_KIND_ATTRIBUTE: &str = "lash.tool_intent.kind";
const TOOL_INTENT_REFUSAL_ATTRIBUTE: &str = "lash.tool_intent.refusal_reason";
const PROVIDER_ATTRIBUTE: &str = "lash.provider";
const PROVIDER_RETRY_KIND_ATTRIBUTE: &str = "lash.provider.retry.kind";
const SESSION_LANE_WAIT_OUTCOME_ATTRIBUTE: &str = "lash.session_execution_lane.wait.outcome";
const SESSION_LANE_GIVE_UP_ATTRIBUTE: &str = "lash.session_execution_lane.give_up";
const POSTGRES_POOL_ACQUIRE_OUTCOME_ATTRIBUTE: &str = "lash.postgres.pool.acquire.outcome";
const RUNTIME_COMMIT_BUDGET_OUTCOME_ATTRIBUTE: &str = "lash.runtime_commit.budget.outcome";

/// Runtime-facing OpenTelemetry instruments for host-tunable operational limits.
#[doc(hidden)]
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
    pub fn from_global_provider() -> Self {
        Self::new(global::meter_provider().meter(INSTRUMENTATION_NAME))
    }

    pub fn new(meter: Meter) -> Self {
        Self {
            provider_retries: meter
                .u64_counter("lash.provider.retries")
                .with_description("Provider calls retried by the reliability ladder")
                .build(),
            provider_throttle_wait_duration: meter
                .u64_histogram("lash.provider.throttle_wait.duration")
                .with_description("Provider-requested throttle wait honored before retry")
                .with_unit("ms")
                .build(),
            session_lane_contention_wait_duration: meter
                .u64_histogram("lash.session_execution_lane.contention_wait.duration")
                .with_description("Time spent waiting for a contended session execution lane")
                .with_unit("ms")
                .build(),
            session_lane_give_ups: meter
                .u64_counter("lash.session_execution_lane.give_ups")
                .with_description("Contended session execution lane waits abandoned for redrive")
                .build(),
            queued_work_wake_retries: meter
                .u64_counter("lash.queued_work.wake_retries")
                .with_description("Queued-work wake attempts retried after transient failure")
                .build(),
            postgres_pool_acquire_wait_duration: meter
                .u64_histogram("lash.postgres.pool.acquire_wait.duration")
                .with_description("Time spent acquiring a PostgreSQL runtime connection")
                .with_unit("ms")
                .build(),
            runtime_commit_budgeted_size: meter
                .u64_histogram("lash.runtime_commit.budgeted_size")
                .with_description("Logical persisted-payload size checked against commit budget")
                .with_unit("By")
                .build(),
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

/// Runtime-facing OpenTelemetry counters for tool-intent realization.
#[doc(hidden)]
#[derive(Clone)]
pub struct ToolIntentMetrics {
    executed: Counter<u64>,
    refused: Counter<u64>,
}

impl ToolIntentMetrics {
    pub fn from_global_provider() -> Self {
        Self::new(global::meter_provider().meter(INSTRUMENTATION_NAME))
    }

    pub fn new(meter: Meter) -> Self {
        Self {
            executed: meter
                .u64_counter("lash.tool_intent.executed")
                .with_description("Tool intents realized as durable host commands")
                .build(),
            refused: meter
                .u64_counter("lash.tool_intent.refused")
                .with_description("Tool intents refused before or during realization")
                .build(),
        }
    }

    pub fn record_executed(&self, kind: &'static str) {
        self.executed
            .add(1, &[KeyValue::new(TOOL_INTENT_KIND_ATTRIBUTE, kind)]);
    }

    pub fn record_refused(&self, kind: &'static str, reason: &'static str) {
        self.refused.add(
            1,
            &[
                KeyValue::new(TOOL_INTENT_KIND_ATTRIBUTE, kind),
                KeyValue::new(TOOL_INTENT_REFUSAL_ATTRIBUTE, reason),
            ],
        );
    }
}

/// Runtime-facing OpenTelemetry instruments for native worker saturation.
#[doc(hidden)]
#[derive(Clone)]
pub struct WorkerCapacityMetrics {
    slots_in_use: Gauge<u64>,
    slots_available: Gauge<u64>,
    intake_depth: Gauge<u64>,
}

impl WorkerCapacityMetrics {
    pub fn from_global_provider() -> Self {
        Self::new(global::meter_provider().meter(INSTRUMENTATION_NAME))
    }

    pub fn new(meter: Meter) -> Self {
        Self {
            slots_in_use: meter
                .u64_gauge("lash.worker.slots.in_use")
                .with_description("native worker slots currently held")
                .build(),
            slots_available: meter
                .u64_gauge("lash.worker.slots.available")
                .with_description("native worker slots immediately available")
                .build(),
            intake_depth: meter
                .u64_gauge("lash.worker.intake.depth")
                .with_description(
                    "Coalesced native work held in the local intake buffer; not durable backlog",
                )
                .build(),
        }
    }

    pub fn record_slots(
        &self,
        worker_id: &str,
        kind: &'static str,
        in_use: usize,
        available: usize,
    ) {
        let attributes = [
            KeyValue::new(WORKER_ID_ATTRIBUTE, worker_id.to_string()),
            KeyValue::new(WORKER_KIND_ATTRIBUTE, kind),
        ];
        self.slots_in_use
            .record(u64::try_from(in_use).unwrap_or(u64::MAX), &attributes);
        self.slots_available
            .record(u64::try_from(available).unwrap_or(u64::MAX), &attributes);
    }

    pub fn record_intake_depth(&self, worker_id: &str, kind: &'static str, depth: usize) {
        self.intake_depth.record(
            u64::try_from(depth).unwrap_or(u64::MAX),
            &[
                KeyValue::new(WORKER_ID_ATTRIBUTE, worker_id.to_string()),
                KeyValue::new(WORKER_KIND_ATTRIBUTE, kind),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    #[test]
    fn worker_capacity_gauges_export_with_stable_names_and_unversioned_scope() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = WorkerCapacityMetrics::new(provider.meter(INSTRUMENTATION_NAME));

        metrics.record_slots("worker-a", "process", 2, 3);
        metrics.record_intake_depth("worker-a", "process", 5);
        provider.force_flush().expect("flush in-memory metrics");

        let exported = exporter
            .get_finished_metrics()
            .expect("read in-memory metrics");
        let scopes = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .collect::<Vec<_>>();
        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].scope().name(), INSTRUMENTATION_NAME);
        assert_eq!(scopes[0].scope().version(), None);
        let mut names = scopes[0]
            .metrics()
            .map(|metric| metric.name())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "lash.worker.intake.depth",
                "lash.worker.slots.available",
                "lash.worker.slots.in_use",
            ]
        );
        let rendered = format!("{exported:?}");
        assert!(rendered.contains("lash.worker.id"));
        assert!(rendered.contains("worker-a"));
    }

    #[test]
    fn tool_intent_counters_export_registered_names_and_typed_dimensions() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = ToolIntentMetrics::new(provider.meter(INSTRUMENTATION_NAME));

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
        let metrics = RuntimeTuningMetrics::new(provider.meter(INSTRUMENTATION_NAME));

        metrics.record_provider_retry("test", "backoff");
        metrics.record_provider_throttle_wait("test", std::time::Duration::from_millis(10));
        metrics
            .record_session_lane_contention_wait(std::time::Duration::from_millis(20), "acquired");
        metrics.record_session_lane_give_up("holder_is_alive");
        metrics.record_queued_work_wake_retry();
        metrics.record_postgres_pool_acquire_wait(std::time::Duration::from_millis(30), "success");
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
