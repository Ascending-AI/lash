use opentelemetry::metrics::{Counter, Gauge, Meter};
use opentelemetry::{KeyValue, global};

const INSTRUMENTATION_NAME: &str = "lash-trace";
const WORKER_KIND_ATTRIBUTE: &str = "lash.worker.kind";
const WORKER_ID_ATTRIBUTE: &str = "lash.worker.id";
const TOOL_INTENT_KIND_ATTRIBUTE: &str = "lash.tool_intent.kind";
const TOOL_INTENT_REFUSAL_ATTRIBUTE: &str = "lash.tool_intent.refusal_reason";

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

/// Runtime-facing OpenTelemetry instruments for inline worker saturation.
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
                .with_description("Inline worker slots currently held")
                .build(),
            slots_available: meter
                .u64_gauge("lash.worker.slots.available")
                .with_description("Inline worker slots immediately available")
                .build(),
            intake_depth: meter
                .u64_gauge("lash.worker.intake.depth")
                .with_description(
                    "Coalesced inline work held in the local intake buffer; not durable backlog",
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
}
