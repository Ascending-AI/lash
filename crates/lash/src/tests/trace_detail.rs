//! L21 (a), (b) and (e) (FIG-6300): detail and export select by kind.
//! Extended model detail brings provider and stream event records; a kind
//! that is off is never built; a kind the exporter skips is built and
//! reaches every sink, and is not exported.

use super::trace_enrichment::exporting;
use super::*;

use crate::support::TurnInput;
use crate::tracing::{
    TraceDetail, TraceDetailPolicy, TraceExportPolicy, TraceRecord, TraceSpanKind as K,
};
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use lash_core::llm::types::{LlmOutputPart, LlmProviderTraceEvent, StreamBlockIdentity};
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};
use lash_trace::otel::OtelOptions;
use opentelemetry_sdk::trace::{Sampler, SpanData};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A model whose every call fails its first attempt retryably, then
/// streams its answer and reports its provider request when asked.
fn retried_model() -> ProviderHandle {
    let attempts = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(Some(2))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request: LlmRequest| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt.is_multiple_of(2) {
                    return Err(LlmTransportError::new("transient")
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
                }
                if let Some(trace) = request.provider_trace.as_ref() {
                    trace.send(LlmProviderTraceEvent::request(
                        "mock",
                        "responses",
                        r#"{"model":"mock-model"}"#.to_owned(),
                    ));
                }
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: StreamBlockIdentity::new("text:0", 0),
                    text: "done".to_owned(),
                }));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "done".to_owned(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// One served turn whose model call takes two attempts, under `detail` and
/// `export`: the sink's records and the exported spans.
async fn turn_under(
    detail: TraceDetailPolicy,
    export: TraceExportPolicy,
) -> Result<(Vec<TraceRecord>, Vec<SpanData>)> {
    let sink = Arc::new(crate::testing::RecordingTraceSink::new());
    let ((spans, _tracer), builder) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(retried_model(), mock_llm_profile_spec())
        .trace_detail(detail)
        .trace_sink(sink.clone()),
        Sampler::AlwaysOn,
        OtelOptions {
            export,
            ..OtelOptions::standard()
        },
        None,
    );
    let core = builder.build(crate::testing::runtime_lease_owner())?;
    let output = core
        .session(crate::SessionId::parse("l21-detail").expect("an id"))
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("retried"))
        .output()
        .await?;
    assert_eq!(output.result.llm_calls[0].attempts.len(), 2, "two attempts");
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !spans
            .get_finished_spans()
            .expect("exported spans")
            .iter()
            .any(|span| span.name.starts_with("invoke_agent"))
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the turn is exported");
    core.flush_trace_sink()?;
    let exported = spans.get_finished_spans().expect("exported spans");
    core.shutdown().await?;
    Ok((sink.records(), exported))
}

fn of_kind<'a>(records: &'a [TraceRecord], kind: &str) -> Vec<&'a TraceRecord> {
    records
        .iter()
        .filter(|record| record.event.kind().as_str() == kind)
        .collect()
}

fn chats(spans: &[SpanData]) -> Vec<&SpanData> {
    spans
        .iter()
        .filter(|span| span.name == "chat" || span.name.starts_with("chat "))
        .collect()
}

/// The keys of `span`'s attributes that do not vary between runs.
fn stable_keys(span: &SpanData) -> Vec<String> {
    let mut keys: Vec<_> = span
        .attributes
        .iter()
        .map(|attribute| attribute.key.as_str().to_owned())
        .collect();
    keys.sort();
    keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detail_and_export_select_by_kind_and_skip_before_building() -> Result<()> {
    let (standard_records, standard_spans) =
        turn_under(TraceDetailPolicy::standard(), TraceExportPolicy::standard()).await?;
    assert!(of_kind(&standard_records, "provider_event").is_empty());
    assert!(of_kind(&standard_records, "runtime_stream_event").is_empty());
    assert_eq!(
        of_kind(&standard_records, "llm_attempt_completed").len(),
        2,
        "each attempt is recorded under standard detail"
    );
    let standard_chat = chats(&standard_spans);
    assert_eq!(standard_chat.len(), 1);

    // (a) Extended model detail: provider and stream event records reach
    // the sink.
    let (extended, _) = turn_under(
        TraceDetailPolicy::extended_models(),
        TraceExportPolicy::standard(),
    )
    .await?;
    assert!(!of_kind(&extended, "provider_event").is_empty());
    assert!(!of_kind(&extended, "runtime_stream_event").is_empty());

    // (b) Attempts off: no attempt record, and the model call's span is
    // unchanged.
    let (no_attempts, no_attempt_spans) = turn_under(
        TraceDetailPolicy::builder()
            .set(K::ModelAttempt, TraceDetail::Off)
            .build()
            .expect("a leaf off"),
        TraceExportPolicy::standard(),
    )
    .await?;
    assert!(of_kind(&no_attempts, "llm_attempt_completed").is_empty());
    let chat = chats(&no_attempt_spans);
    assert_eq!(chat.len(), 1);
    assert_eq!(chat[0].name, standard_chat[0].name);
    assert_eq!(stable_keys(chat[0]), stable_keys(standard_chat[0]));
    assert!(
        no_attempt_spans
            .iter()
            .all(|span| !span.name.contains("attempt")),
        "no attempt span"
    );

    // (e) Model calls and their attempts skipped by the exporter: built,
    // recorded, and not exported.
    let (skipped, skipped_spans) = turn_under(
        TraceDetailPolicy::standard(),
        TraceExportPolicy::builder()
            .skip(K::Model)
            .skip(K::ModelAttempt)
            .build()
            .expect("a leaf skipped with its children"),
    )
    .await?;
    assert!(chats(&skipped_spans).is_empty(), "no chat span is exported");
    assert!(
        skipped_spans
            .iter()
            .any(|span| span.name.starts_with("invoke_agent")),
        "the turn is exported"
    );
    assert_eq!(of_kind(&skipped, "llm_call_completed").len(), 1);
    assert_eq!(of_kind(&skipped, "llm_attempt_completed").len(), 2);
    for spans in [&standard_spans, &no_attempt_spans, &skipped_spans] {
        for span in spans.iter() {
            assert!(
                spans
                    .iter()
                    .any(|parent| parent.span_context.span_id() == span.parent_span_id)
                    || span.parent_span_id == crate::tracing::otel::trace::SpanId::INVALID
                    || span.name == "lash.send",
                "{} is exported under a parent that was",
                span.name
            );
        }
    }
    Ok(())
}
