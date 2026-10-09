//! The host trace sink: what each record costs the execution that emits it,
//! under each content policy, for host-authored custom payloads, through the
//! OpenTelemetry adapter, and behind a sink that is slow and refuses records.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use lash::tracing::{
    JsonlTraceSink, TelemetryContent, TraceEvent, TraceLevel, TraceRecord, TraceSink,
    TraceSinkError,
};
use lash_sansio::sync::MutexExt;

use super::super::{Args, Case, Meter, Receipt, facade};
use super::{Flavor, Fleet};

/// The host's installed sink: it times the sink it wraps on the emitting
/// thread, which is where a record's cost lands.
struct TimedSink {
    inner: Arc<dyn TraceSink>,
    meter: Meter,
    boundary: &'static str,
    /// Blocks the emitter this long before each record reaches the sink.
    delay: Option<Duration>,
    /// Refuses every nth record, as an exporter that cannot keep up does.
    refuse_every: Option<u64>,
    offered: AtomicU64,
    refused: AtomicU64,
    kinds: Mutex<BTreeMap<String, u64>>,
}

impl TimedSink {
    fn new(inner: Arc<dyn TraceSink>, meter: &Meter, boundary: &'static str) -> Self {
        Self {
            inner,
            meter: meter.clone(),
            boundary,
            delay: None,
            refuse_every: None,
            offered: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            kinds: Mutex::new(BTreeMap::new()),
        }
    }
}

fn event_kind(record: &TraceRecord) -> String {
    serde_json::to_value(&record.event)
        .ok()
        .and_then(|event| event["type"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

impl TraceSink for TimedSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let offered = self.offered.fetch_add(1, Ordering::Relaxed) + 1;
        let start = Instant::now();
        if let Some(delay) = self.delay {
            std::thread::sleep(delay);
        }
        let result = if self
            .refuse_every
            .is_some_and(|every| offered.is_multiple_of(every))
        {
            self.refused.fetch_add(1, Ordering::Relaxed);
            Err(TraceSinkError::Write {
                path: "slow-sink".into(),
                source: std::io::Error::other("the exporter refused the record"),
            })
        } else {
            self.inner.append(record)
        };
        self.meter.operation(
            self.boundary,
            &record.id,
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        *self
            .kinds
            .lock_recover()
            .entry(event_kind(record))
            .or_default() += 1;
        result
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        self.inner.flush()
    }
}

/// A sink that keeps nothing, so a workload times only what precedes it.
struct Discard;
impl TraceSink for Discard {
    fn append(&self, _: &TraceRecord) -> Result<(), TraceSinkError> {
        Ok(())
    }
}

/// The exporter behind the host's OpenTelemetry provider: it times each
/// export the adapter's spans cause.
#[derive(Debug)]
struct TimedExporter {
    inner: opentelemetry_sdk::trace::InMemorySpanExporter,
    meter: Meter,
    exports: Arc<AtomicU64>,
}

impl opentelemetry_sdk::trace::SpanExporter for TimedExporter {
    async fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let start = Instant::now();
        let result = self.inner.export(batch).await;
        self.meter.operation(
            "trace.otel.export",
            format!("export:{}", self.exports.load(Ordering::Relaxed)),
            if result.is_ok() { "ok" } else { "error" },
            start,
        );
        self.exports.fetch_add(1, Ordering::Relaxed);
        result
    }
}

/// One session settles `--operations` sends with the workload's sink
/// installed. `--callers` sizes what the variant varies: the custom payload
/// in KiB, or the slow sink's delay in milliseconds.
pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let meter = Meter::new(args.ledger_cap);
    let path = args.store_dir.join("trace.jsonl");
    let inner: Arc<dyn TraceSink> = match args.case {
        Case::TraceSinkOtel | Case::TraceSinkSlow => Arc::new(Discard),
        _ => Arc::new(JsonlTraceSink::new(path.clone())),
    };
    let mut sink = TimedSink::new(inner, &meter, "trace.sink.append");
    if matches!(args.case, Case::TraceSinkSlow) {
        sink.delay = Some(Duration::from_millis(args.callers as u64));
        sink.refuse_every = Some(4);
    }
    let sink = Arc::new(sink);
    let content = if matches!(args.case, Case::TraceSinkCaptured) {
        TelemetryContent::Captured
    } else {
        TelemetryContent::Omitted
    };

    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let exports = Arc::new(AtomicU64::new(0));
    let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_simple_exporter(TimedExporter {
            inner: exporter.clone(),
            meter: meter.clone(),
            exports: exports.clone(),
        })
        .build();
    let meters = opentelemetry_sdk::metrics::SdkMeterProvider::default();
    let otel = matches!(args.case, Case::TraceSinkOtel);

    let fleet = Fleet::open(args, &meter, 1, Flavor::Chat, |builder, _| {
        let builder = builder
            .trace_sink(sink.clone())
            .trace_level(TraceLevel::Extended)
            .telemetry_content(content);
        Ok(if otel {
            builder.telemetry(lash::tracing::OtelTelemetry::new(
                &tracer,
                &meters,
                lash::tracing::OtelOptions::standard(),
            ))
        } else {
            builder
        })
    })
    .await?;
    let result = async {
        let node = &fleet.nodes[0];
        let session = facade::create(&node.core, &super::unique("trace")).await?;
        let window = Instant::now();
        for n in 0..args.operations {
            facade::send(&session, &format!("trace-{n}"), &meter).await?;
        }
        let built_in = sink.offered.load(Ordering::Relaxed);
        meter.window("trace.sink.records", built_in as usize, window);
        ensure!(built_in > 0, "the turns emitted no trace record");

        let mut custom = 0;
        if matches!(args.case, Case::TraceSinkCustom) {
            // A custom record is its producer's payload and passes the host's
            // sink unread. These are host-authored and offered to the
            // installed sink; no plugin emission path is driven.
            let timed = TimedSink::new(sink.clone(), &meter, "trace.sink.append.custom");
            let payload = serde_json::json!({"text": "x".repeat(args.callers * 1024)});
            let window = Instant::now();
            for n in 0..args.operations {
                timed.append(&TraceRecord {
                    schema_version: lash::tracing::TRACE_SCHEMA_VERSION,
                    id: format!("observation-workload-custom-{n}"),
                    timestamp: chrono::Utc::now(),
                    content,
                    context: Default::default(),
                    event: TraceEvent::Custom {
                        name: "observation_workload.custom".into(),
                        payload: payload.clone(),
                    },
                })?;
                custom += 1;
            }
            meter.window("trace.sink.custom_records", custom, window);
        }
        node.core.flush_trace_sink()?;
        ensure!(
            !otel || exports.load(Ordering::Relaxed) > 0,
            "the OpenTelemetry adapter exported no span"
        );
        anyhow::Ok((built_in, custom))
    }
    .await;
    let store = fleet.store;
    fleet.close().await?;
    let (built_in, custom) = result?;
    let refused = sink.refused.load(Ordering::Relaxed);
    let spans = exporter
        .get_finished_spans()
        .map_err(|error| anyhow::anyhow!("read exported spans: {error}"))?
        .len();
    let counters = serde_json::json!({
        "content": content,
        "records_offered": sink.offered.load(Ordering::Relaxed),
        "records_refused_and_lost": refused,
        "records_by_kind": *sink.kinds.lock_recover(),
        "built_in_records_per_send": built_in as f64 / args.operations as f64,
        "custom_records": custom,
        "custom_payload_bytes": if custom > 0 { args.callers * 1024 } else { 0 },
        "jsonl_bytes": std::fs::metadata(&path).map(|file| file.len()).unwrap_or(0),
        "otel_exports": exports.load(Ordering::Relaxed),
        "otel_spans_exported": spans,
        "sink_delay_ms": sink.delay.map(|delay| delay.as_millis()),
        "emitter_blocked_ms": sink.delay.map(|delay| delay.as_millis() * u128::from(built_in)),
    });
    Ok(Receipt::measured(
        args.case,
        "facade-send+host-trace-sink",
        store,
        args.operations,
        &meter,
        counters,
        serde_json::json!({
            "settled_sends": args.operations,
            "records_offered": built_in + custom as u64,
            "records_refused_and_lost": refused,
            "sink_runs_on_the_emitting_thread": true,
        }),
    ))
}
