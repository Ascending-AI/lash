//! L16 (FIG-6299, D7): sampling decisions and carriers agree. The host's
//! sampler decides once, at the producer, and every lash span of the trace
//! follows that decision on every owner; a sink sees the same decision in
//! each record's position.
//!
//! The first owner's sampler drops one caller's sends, records another's
//! without sampling them (`RecordOnly`) and rewrites the tracestate of every
//! send it samples. It serves no session: a second owner, whose sampler is
//! `AlwaysOff`, runs every turn. Both share one recording sink.
//! The sampler's result attributes (FIG-6300) are exported on the producer
//! span only, and no store row holds them. (The MCP `_meta` clause is the
//! MCP propagation lane's, §3.10.)

use super::tracing::text_call;
use super::*;

use crate::support::TurnInput;
use crate::tracing::otel::trace::{Link, SpanId, SpanKind, TraceId, TraceState};
use crate::tracing::otel::{Context, KeyValue};
use lash_core::testing::runtime_helpers::mock_provider;
use lash_trace::otel::{OtelOptions, OtelTelemetry};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SamplingDecision, SamplingResult, SdkTracerProvider,
    ShouldSample,
};

const KEEP: u8 = 0x51;
const DROP: u8 = 0x52;
const RECORD: u8 = 0x53;
/// The sampler's result attribute: a token no lash row may hold.
const SAMPLER_TOKEN: &str = "sampler-kept-3d9a";

fn caller(caller: u8) -> lash_core::TraceCarrier {
    lash_core::TraceCarrier::parse_w3c(&format!("00-{caller:032x}-{caller:016x}-01"), None)
        .expect("a valid trace context")
}

fn trace_of(caller: u8) -> TraceId {
    TraceId::from_bytes(self::caller(caller).trace_id().to_bytes())
}

/// The first owner's sampler: by the producer's trace, drop, record only,
/// or sample under the tracestate `vendor=x`.
#[derive(Clone, Debug)]
struct HostSampler;

impl ShouldSample for HostSampler {
    fn should_sample(
        &self,
        _: Option<&Context>,
        trace_id: TraceId,
        _: &str,
        _: &SpanKind,
        _: &[KeyValue],
        _: &[Link],
    ) -> SamplingResult {
        SamplingResult {
            decision: if trace_id == trace_of(DROP) {
                SamplingDecision::Drop
            } else if trace_id == trace_of(RECORD) {
                SamplingDecision::RecordOnly
            } else {
                SamplingDecision::RecordAndSample
            },
            attributes: vec![KeyValue::new("host.sampler", SAMPLER_TOKEN)],
            trace_state: TraceState::from_key_value([("vendor", "x")]).expect("a tracestate"),
        }
    }
}

/// An owner's exporter, and `builder` exporting to it under `sampler`.
fn exporting(
    builder: crate::core::LashCoreBuilder,
    sampler: impl ShouldSample + 'static,
) -> (
    (InMemorySpanExporter, crate::tracing::LashTracerProvider),
    crate::core::LashCoreBuilder,
) {
    let spans = InMemorySpanExporter::default();
    let tracer = crate::tracing::LashTracerProvider::new(
        SdkTracerProvider::builder().with_simple_exporter(spans.clone()),
        sampler,
        lash_trace::otel::sdk::trace::RandomIdGenerator::default(),
    );
    let meter = SdkMeterProvider::builder().build();
    let builder = builder.telemetry(OtelTelemetry::new(&tracer, &meter, OtelOptions::default()));
    ((spans, tracer), builder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_owner_exports_the_producers_decision_and_the_sink_sees_it() -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let database = directory.path().join("lash.db");
    let sink = Arc::new(crate::testing::RecordingTraceSink::new());
    let ((first_spans, _first), first) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(
            super::trace_enrichment::file_backend(&database).await,
        ))
        .serve_test_llm_profile(
            mock_provider(Vec::new()).into_handle(),
            mock_llm_profile_spec(),
        )
        .serve_sessions(false)
        .trace_sink(sink.clone()),
        HostSampler,
    );
    let first = first.build(crate::testing::runtime_lease_owner())?;
    for context in [KEEP, DROP, RECORD] {
        first
            .session(crate::SessionId::parse(format!("l16-{context:x}").as_str()).expect("an id"))
            .create(crate::SessionCreation::root(mock_session_spec()))
            .await?
            .send(TurnInput::text("sampled or not"))
            .trace_context(caller(context))
            .await?;
    }
    let ((second_spans, _second), second) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(
            super::trace_enrichment::file_backend(&database).await,
        ))
        .serve_test_llm_profile(
            mock_provider(vec![
                text_call("done"),
                text_call("done"),
                text_call("done"),
            ])
            .into_handle(),
            mock_llm_profile_spec(),
        )
        .trace_sink(sink.clone()),
        Sampler::AlwaysOff,
    );
    let second = second.build(crate::testing::runtime_lease_owner())?;
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let ended = sink
                .records()
                .iter()
                .filter(|record| record.event.kind().as_str() == "turn_completed")
                .count();
            if ended == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second owner ends every turn");
    second.flush_trace_sink()?;

    let records = sink.records();
    let exported: Vec<_> = [&first_spans, &second_spans]
        .into_iter()
        .flat_map(|spans| spans.get_finished_spans().expect("exported spans"))
        .collect();
    let on = |caller: u8| {
        exported
            .iter()
            .filter(move |span| span.span_context.trace_id() == trace_of(caller))
    };
    for span in &exported {
        let record = records
            .iter()
            .filter_map(|record| record.position.as_ref())
            .find(|position| {
                position.publishes()
                    && SpanId::from_bytes(position.span.span_id().to_bytes())
                        == span.span_context.span_id()
            })
            .unwrap_or_else(|| panic!("{} has a sink record", span.name));
        assert_eq!(
            span.span_context.is_sampled(),
            record.span.flags().is_sampled(),
            "{}'s sampled flag is its record's",
            span.name
        );
        assert_eq!(
            span.span_context.trace_state().header(),
            record.span.tracestate().as_str(),
            "{}'s tracestate is its record's",
            span.name
        );
        assert!(
            span.parent_span_id == SpanId::from_bytes(caller(KEEP).span_id().to_bytes())
                || exported
                    .iter()
                    .any(|parent| parent.span_context.span_id() == span.parent_span_id),
            "{} is exported under a parent that was not",
            span.name
        );
    }
    let kept: Vec<_> = on(KEEP).map(|span| span.name.clone()).collect();
    for name in ["lash.send", "invoke_agent", "chat"] {
        assert!(
            kept.iter()
                .any(|kept| kept == name || kept.starts_with(&format!("{name} "))),
            "the sampled trace exports its {name} span on the owner that ran it: {kept:?}"
        );
    }
    assert!(
        second_spans
            .get_finished_spans()
            .expect("exported spans")
            .iter()
            .any(|span| span.name == "invoke_agent"),
        "the AlwaysOff owner exports the rest of a sampled trace"
    );
    for unsampled in [DROP, RECORD] {
        assert_eq!(
            on(unsampled).count(),
            0,
            "an unsampled trace ({unsampled:x}) exports no lash span"
        );
        let positions: Vec<_> = records
            .iter()
            .filter_map(|record| record.position.as_ref())
            .filter(|position| {
                TraceId::from_bytes(position.span.trace_id().to_bytes()) == trace_of(unsampled)
            })
            .collect();
        assert!(
            !positions.is_empty(),
            "the sink sees the {unsampled:x} trace"
        );
        assert!(
            positions
                .iter()
                .all(|position| !position.span.flags().is_sampled()),
            "an unsampled trace's records are unsampled"
        );
    }
    for span in &exported {
        let carried = span
            .attributes
            .iter()
            .any(|attribute| attribute.key.as_str() == "host.sampler");
        assert_eq!(
            carried,
            span.name == "lash.send",
            "{} carries the sampler's attribute only as the producer",
            span.name
        );
    }
    first.shutdown().await?;
    second.shutdown().await?;
    super::trace_enrichment::assert_not_stored(directory.path(), SAMPLER_TOKEN);
    Ok(())
}
