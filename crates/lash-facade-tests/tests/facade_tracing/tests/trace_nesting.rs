//! L3 and L11 (FIG-6299). L3: a served turn nests as `lash.send` →
//! `invoke_agent` → {`chat`, `execute_tool`}, each child starting at or
//! after its parent. L11: lash mints the ids, so a host with no OpenTelemetry
//! adapter gets real positions in its caller's trace and the producer's
//! `Whole` record, and with the adapter the exported `lash.send` span is
//! that record's span.

use super::trace_emission::Observed;
use super::tracing::{text_call, tool_call};
use super::*;

use lash::TurnInput;
use lash::tracing::otel::trace::{SpanId, TraceId};
use lash_core::testing::runtime_helpers::{EchoTool, mock_provider};
use lash_trace::{SpanRole, TraceSpanKind};
use opentelemetry_sdk::trace::SpanData;

/// The caller every law sends under: a sampled span of its own trace.
fn caller() -> lash_core::TraceCarrier {
    lash_core::TraceCarrier::parse_w3c(&format!("00-{:032x}-{:016x}-01", 0xc3, 0xc3), None)
        .expect("a valid trace context")
}

fn span_id(carrier: &lash_core::TraceCarrier) -> SpanId {
    SpanId::from_bytes(carrier.span_id().to_bytes())
}

/// A core over `builder`'s observers that serves one turn calling
/// `echo_tool` under [`caller`] and answering.
async fn sent(
    session: &str,
    builder: impl FnOnce(lash::LashCoreBuilder) -> lash::LashCoreBuilder,
) -> Result<LashCore> {
    let core = builder(
        explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            mock_provider(vec![
                tool_call("call-echo", "tools__echo_tool", r#"{"value":"nested"}"#),
                text_call("done"),
            ])
            .into_handle(),
            mock_llm_profile_spec(),
        )
        .tools(lash::tool_provider_key!("tools"), Arc::new(EchoTool)),
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let output = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(session).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?
    .send(TurnInput::text("call the tool"))
    .trace_context(caller())
    .output()
    .await?;
    assert!(output.is_success(), "{:?}", output.outcome);
    Ok(core)
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    spans
        .iter()
        .filter(|span| span.name == name || span.name.starts_with(&format!("{name} ")))
        .collect()
}

/// L3 (the tracing lane T2's rows): the turn is its send's child, its
/// model calls and its tool call are the turn's, all on the caller's
/// trace, and no child starts before its parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_served_turn_nests_its_calls_under_its_send() -> Result<()> {
    let mut observed = None;
    let core = sent("l3-nesting", |builder| {
        let (installed, builder) = Observed::install(builder);
        observed = Some(installed);
        builder
    })
    .await?;
    let observed = observed.expect("installed");
    observed
        .until("the turn is exported", |observed| {
            !observed.spans("invoke_agent").is_empty()
        })
        .await;
    let spans = observed.exported();
    let [send] = named(&spans, "lash.send")[..] else {
        panic!("one send: {spans:#?}");
    };
    let [turn] = named(&spans, "invoke_agent")[..] else {
        panic!("one turn: {spans:#?}");
    };
    let chats = named(&spans, "chat");
    let tools = named(&spans, "execute_tool");
    assert_eq!((chats.len(), tools.len()), (2, 1), "{spans:#?}");
    assert_eq!(
        send.parent_span_id,
        span_id(&caller()),
        "the send is its caller's child"
    );
    let edges = std::iter::once((send, turn))
        .chain(chats.iter().map(|chat| (turn, *chat)))
        .chain(tools.iter().map(|tool| (turn, *tool)));
    for (parent, child) in edges {
        assert_eq!(
            child.parent_span_id,
            parent.span_context.span_id(),
            "{} is {}'s child",
            child.name,
            parent.name
        );
        assert!(
            child.start_time >= parent.start_time,
            "{} starts before its parent {}",
            child.name,
            parent.name
        );
    }
    for span in &spans {
        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_bytes(caller().trace_id().to_bytes()),
            "{} is on the caller's trace",
            span.name
        );
    }
    core.shutdown().await?;
    Ok(())
}

/// L11: what a sink-only host's sink, or an observed host's, receives.
/// Every record is positioned in the caller's trace; the turn's opening and
/// closing records name one span; the model and tool spans are its
/// children; and the producer's `Whole` record is the turn's parent. With
/// the adapter installed the exported `lash.send` span is that record's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sink_receives_real_positions_and_the_producer_record() -> Result<()> {
    for adapter in [false, true] {
        let sink = Arc::new(lash::testing::RecordingTraceSink::new());
        let mut observed = None;
        let core = sent(&format!("l11-{adapter}"), |builder| {
            let builder = builder.trace_sink(sink.clone());
            if !adapter {
                return builder;
            }
            let (installed, builder) = Observed::install(builder);
            observed = Some(installed);
            builder
        })
        .await?;
        core.flush_trace_sink()?;
        let records = sink.records();
        let positions: Vec<_> = records
            .iter()
            .map(|record| {
                let position = record.position.as_ref().unwrap_or_else(|| {
                    panic!("a {:?} record carries no position", record.event.kind())
                });
                assert_eq!(
                    position.span.trace_id(),
                    caller().trace_id(),
                    "a {:?} record is on the caller's trace",
                    record.event.kind()
                );
                position
            })
            .collect();
        let turn: Vec<_> = positions
            .iter()
            .filter(|position| position.kind == TraceSpanKind::Agent)
            .collect();
        assert!(
            turn.iter().any(|position| position.role == SpanRole::Opens)
                && turn
                    .iter()
                    .any(|position| position.role == SpanRole::Closes),
            "the turn opens and closes: {turn:#?}"
        );
        let span = turn[0].span.span_id();
        assert!(
            turn.iter().all(|position| position.span.span_id() == span),
            "the turn's records name one span: {turn:#?}"
        );
        for child in positions
            .iter()
            .filter(|position| matches!(position.kind, TraceSpanKind::Model | TraceSpanKind::Tool))
        {
            assert_eq!(
                child.parent.as_ref().map(|parent| parent.span_id()),
                Some(span),
                "a {:?} span is the turn's child",
                child.kind
            );
        }
        let [producer] = positions
            .iter()
            .filter(|position| position.kind == TraceSpanKind::Producer)
            .collect::<Vec<_>>()[..]
        else {
            panic!("one producer record: {positions:#?}");
        };
        assert_eq!(producer.role, SpanRole::Whole);
        assert_eq!(
            turn[0].parent.as_ref().map(|parent| parent.span_id()),
            Some(producer.span.span_id()),
            "the turn's parent is the producer the sink received"
        );
        if let Some(observed) = observed {
            let spans = observed.spans("lash.send");
            assert_eq!(spans.len(), 1, "{spans:#?}");
            assert_eq!(
                spans[0].span_context.span_id(),
                span_id(&producer.span),
                "the exported send is the producer record's span"
            );
        }
        core.shutdown().await?;
    }
    Ok(())
}
