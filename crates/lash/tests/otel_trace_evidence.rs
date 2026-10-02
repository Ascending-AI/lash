//! Compile witnesses for the host-installed telemetry adapter and its provider API.
#![cfg(feature = "otel-trace")]
#![allow(dead_code)]

use lash::tracing::{OtelOptions, OtelPayloadExport, OtelSpanEnricher, OtelTelemetry, otel};
use std::sync::Arc;

struct Enricher;
impl OtelSpanEnricher for Enricher {
    fn attributes(&self, _: &lash::tracing::TraceRecord, out: &mut Vec<otel::KeyValue>) {
        out.push(otel::KeyValue::new("host.attribute", true));
    }
}

fn install<P: otel::trace::TracerProvider, M: otel::metrics::MeterProvider>(
    builder: lash::LashCoreBuilder,
    tracer: &P,
    meter: &M,
) -> lash::LashCoreBuilder
where
    P::Tracer: Send + Sync + 'static,
    <P::Tracer as otel::trace::Tracer>::Span: Send + Sync + 'static,
{
    let telemetry = OtelTelemetry::new(
        tracer,
        meter,
        OtelOptions {
            include_context_metadata: false,
            payloads: OtelPayloadExport::Bounded {
                max_record_bytes: 256,
                max_events: 2,
            },
            enrich: Some(Arc::new(Enricher)),
        },
    );
    let _: &lash::tracing::TelemetryMetrics = telemetry.metrics();
    let _: &OtelOptions = telemetry.options();
    builder.telemetry(telemetry)
}
