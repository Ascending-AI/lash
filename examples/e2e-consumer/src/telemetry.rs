//! Opt-in OTLP/HTTP JSON export for the executing consumer fixture (S34).
//! The SDK and its lifetime belong to this host, not to Lash.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use lash::tracing::otel::trace::{SpanKind, Status};
use lash::tracing::otel::{Array, KeyValue, Value};
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider, SpanData, SpanExporter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportReceipt {
    pub attempted_spans: u64,
    pub acknowledged_spans: u64,
    pub dropped_spans: u64,
    pub flush_error: Option<String>,
    pub shutdown_error: Option<String>,
}

#[derive(Debug, Default)]
struct Counts {
    attempted: AtomicU64,
    acknowledged: AtomicU64,
    dropped: AtomicU64,
}

pub struct HostTelemetry {
    tracer: SdkTracerProvider,
    meter: SdkMeterProvider,
    counts: Arc<Counts>,
}

impl HostTelemetry {
    pub fn new(endpoint: &str) -> Result<Self> {
        let endpoint = reqwest::Url::parse(endpoint).context("parse fixture OTLP endpoint")?;
        ensure!(
            endpoint.scheme() == "http" && endpoint.path() == "/v1/traces",
            "fixture needs an explicit HTTP /v1/traces endpoint"
        );
        ensure!(
            endpoint.host_str().is_some_and(|host| host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())),
            "fixture collector must be on loopback"
        );
        let counts = Arc::new(Counts::default());
        let exporter = HttpJsonExporter {
            endpoint,
            client: reqwest::Client::builder()
                .timeout(EXPORT_TIMEOUT)
                .no_proxy()
                .build()?,
            // BatchSpanProcessor owns the exporter on its own OS thread. This
            // runtime supplies the HTTP reactor; no host reactor is blocked.
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?,
            counts: counts.clone(),
            resource: Vec::new(),
        };
        let processor = BatchSpanProcessor::builder(exporter)
            .with_batch_config(
                BatchConfigBuilder::default()
                    .with_scheduled_delay(Duration::from_secs(3600))
                    .build(),
            )
            .build();
        Ok(Self {
            tracer: SdkTracerProvider::builder()
                .with_sampler(Sampler::AlwaysOn)
                .with_span_processor(processor)
                .build(),
            meter: SdkMeterProvider::builder().build(),
            counts,
        })
    }

    pub fn install(&self, builder: lash::LashCoreBuilder) -> lash::LashCoreBuilder {
        builder.telemetry(lash::tracing::OtelTelemetry::new(
            &self.tracer,
            &self.meter,
            lash::tracing::OtelOptions::default(),
        ))
    }

    /// A flush error describes telemetry delivery and never a Run outcome.
    pub fn flush(&self) -> ExportReceipt {
        let error = self
            .tracer
            .force_flush()
            .err()
            .map(|error| error.to_string());
        self.receipt(error, None)
    }

    /// Call after quiescing the host and flushing its JSONL trace sink.
    pub fn shutdown(self) -> ExportReceipt {
        let flush_error = self
            .tracer
            .force_flush()
            .err()
            .map(|error| error.to_string());
        let tracer_error = self.tracer.shutdown().err().map(|error| error.to_string());
        let meter_error = self.meter.shutdown().err().map(|error| error.to_string());
        let shutdown_error = tracer_error.or(meter_error);
        self.receipt(flush_error, shutdown_error)
    }

    fn receipt(
        &self,
        flush_error: Option<String>,
        shutdown_error: Option<String>,
    ) -> ExportReceipt {
        ExportReceipt {
            attempted_spans: self.counts.attempted.load(Ordering::SeqCst),
            acknowledged_spans: self.counts.acknowledged.load(Ordering::SeqCst),
            dropped_spans: self.counts.dropped.load(Ordering::SeqCst),
            flush_error,
            shutdown_error,
        }
    }
}

#[derive(Debug)]
struct HttpJsonExporter {
    endpoint: reqwest::Url,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    counts: Arc<Counts>,
    resource: Vec<KeyValue>,
}

impl SpanExporter for HttpJsonExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let count = batch.len() as u64;
        self.counts.attempted.fetch_add(count, Ordering::SeqCst);
        let body = json!({"resourceSpans": [{
            "resource": {"attributes": attributes(&self.resource)},
            "scopeSpans": batch.iter().map(|span| json!({
                "scope": {"name": span.instrumentation_scope.name(), "version": span.instrumentation_scope.version().unwrap_or_default()},
                "spans": [encode_span(span)]
            })).collect::<Vec<_>>()
        }]});
        let client = self.client.clone();
        let endpoint = self.endpoint.clone();
        let result = self
            .runtime
            .spawn(async move {
                let response = client
                    .post(endpoint)
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
                let response: Json = response.json().await?;
                // Partial success is not an acknowledgement of every span.
                if response.get("partialSuccess").is_some_and(|partial| {
                    partial.get("rejectedSpans").is_some_and(|rejected| {
                        rejected.as_str() != Some("0") && rejected.as_u64() != Some(0)
                    })
                }) {
                    return Err(anyhow::anyhow!("collector rejected spans: {response}"));
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
        match result {
            Ok(Ok(())) => {
                self.counts.acknowledged.fetch_add(count, Ordering::SeqCst);
                Ok(())
            }
            other => {
                self.counts.dropped.fetch_add(count, Ordering::SeqCst);
                Err(OTelSdkError::InternalFailure(format!(
                    "OTLP export failed: {other:?}"
                )))
            }
        }
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.resource = resource
            .iter()
            .map(|(key, value)| KeyValue::new(key.clone(), value.clone()))
            .collect();
    }
}

fn attributes(values: &[KeyValue]) -> Vec<Json> {
    values
        .iter()
        .map(|item| json!({"key": item.key.as_str(), "value": any_value(&item.value)}))
        .collect()
}

fn any_value(value: &Value) -> Json {
    match value {
        Value::Bool(value) => json!({"boolValue": value}),
        Value::I64(value) => json!({"intValue": value.to_string()}),
        Value::F64(value) => json!({"doubleValue": value}),
        Value::String(value) => json!({"stringValue": value.as_str()}),
        Value::Array(array) => {
            let values: Vec<Json> = match array {
                Array::Bool(values) => values
                    .iter()
                    .map(|value| any_value(&Value::Bool(*value)))
                    .collect(),
                Array::I64(values) => values
                    .iter()
                    .map(|value| any_value(&Value::I64(*value)))
                    .collect(),
                Array::F64(values) => values
                    .iter()
                    .map(|value| any_value(&Value::F64(*value)))
                    .collect(),
                Array::String(values) => values
                    .iter()
                    .map(|value| any_value(&Value::String(value.clone())))
                    .collect(),
                _ => unreachable!("fixture only receives SDK scalar arrays"),
            };
            json!({"arrayValue": {"values": values}})
        }
        _ => unreachable!("fixture only receives SDK scalar/array attributes"),
    }
}

fn nanos(time: SystemTime) -> String {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn encode_span(span: &SpanData) -> Json {
    let status = match &span.status {
        Status::Unset => json!({"code": 0}),
        Status::Ok => json!({"code": 1}),
        Status::Error { description } => json!({"code": 2, "message": description}),
    };
    json!({
        "traceId": span.span_context.trace_id().to_string(),
        "spanId": span.span_context.span_id().to_string(),
        "parentSpanId": span.parent_span_id.to_string(),
        "traceState": span.span_context.trace_state().header(),
        "flags": u32::from(span.span_context.trace_flags().to_u8()),
        "name": span.name,
        "kind": match span.span_kind { SpanKind::Internal => 1, SpanKind::Server => 2, SpanKind::Client => 3, SpanKind::Producer => 4, SpanKind::Consumer => 5 },
        "startTimeUnixNano": nanos(span.start_time),
        "endTimeUnixNano": nanos(span.end_time),
        "attributes": attributes(&span.attributes),
        "droppedAttributesCount": span.dropped_attributes_count,
        "events": span.events.iter().map(|event| json!({"name": event.name, "timeUnixNano": nanos(event.timestamp), "attributes": attributes(&event.attributes), "droppedAttributesCount": event.dropped_attributes_count})).collect::<Vec<_>>(),
        "droppedEventsCount": span.events.dropped_count,
        "links": span.links.iter().map(|link| json!({"traceId": link.span_context.trace_id().to_string(), "spanId": link.span_context.span_id().to_string(), "traceState": link.span_context.trace_state().header(), "flags": u32::from(link.span_context.trace_flags().to_u8()), "attributes": attributes(&link.attributes), "droppedAttributesCount": link.dropped_attributes_count})).collect::<Vec<_>>(),
        "droppedLinksCount": span.links.dropped_count,
        "status": status,
    })
}
