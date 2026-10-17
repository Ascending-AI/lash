//! A turn joins the trace of what caused it (FIG-6298): a send parents
//! the turn it opens, a batched turn parents its head input and links the
//! rest, an input a running turn consumes is linked from it and never
//! becomes a second parent, and a `SessionTurn` process's child turn joins
//! the process's trace. Each law reads the spans a host's OpenTelemetry SDK
//! exported for a real run over SQLite memory stores.

use super::trace_emission::{Observed, attribute};
use super::tracing::{PendingEchoTool, text_call, tool_call};
use super::*;
use lash_facade_tests::session_process as session_turn_process;

use lash::TurnInput;
use lash_core::testing::runtime_helpers::mock_provider;
use opentelemetry_sdk::trace::SpanData;

type Keys =
    tokio::sync::mpsc::UnboundedReceiver<(lash_execution_contracts::waits::PinnedKey, String)>;

/// The context a host's caller `caller` sends under: a sampled span of its
/// own trace.
fn caller(caller: u8) -> lash_core::TraceCarrier {
    lash_core::TraceCarrier::parse_w3c(&format!("00-{caller:032x}-{caller:016x}-01"), None)
        .expect("a valid trace context")
}

use lash::tracing::otel::trace::{SpanId, TraceId};

/// The trace caller `context` sends on.
fn trace_of(context: u8) -> TraceId {
    TraceId::from_bytes(caller(context).trace_id().to_bytes())
}

fn span_id(span: &SpanData) -> SpanId {
    span.span_context.span_id()
}

fn links(span: &SpanData) -> Vec<SpanId> {
    span.links
        .iter()
        .map(|link| link.span_context.span_id())
        .collect()
}

/// The one exported span of `spans` the predicate picks.
fn one<'a>(spans: &'a [SpanData], what: &str, pick: impl Fn(&SpanData) -> bool) -> &'a SpanData {
    let picked: Vec<_> = spans.iter().filter(|span| pick(span)).collect();
    assert_eq!(picked.len(), 1, "one span is {what}: {spans:#?}");
    picked[0]
}

/// The `lash.send` producer the send under `context` opened: a child of
/// the caller's context.
fn producer(spans: &[SpanData], context: u8) -> &SpanData {
    let parent = SpanId::from_bytes(caller(context).span_id().to_bytes());
    let producer = one(spans, "the producer of a send", |span| {
        span.name == "lash.send" && span.parent_span_id == parent
    });
    assert_eq!(
        producer.span_context.trace_id(),
        trace_of(context),
        "a producer is its caller's child"
    );
    producer
}

const SENT_SESSION: &str = "parentage-sent";

/// FIG-6339: a host can queue a serialized start request and admit it in
/// another process without losing the caller's explicit carrier or Link mode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_serialized_process_start_links_its_explicit_caller() -> Result<()> {
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(vec![text_call("child answered")]).into_handle(),
        mock_llm_profile_spec(),
    );
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    core.session(
        lash::SessionId::parse(session_turn_process::PARENT).expect("nonblank session id"),
    )
    .create(lash::SessionCreation::root(mock_session_spec()))
    .await?;
    let context =
        lash_core::TraceCarrier::parse_w3c(&caller(0xe2).traceparent(), Some("queued=caller"))
            .expect("a valid caller carrier");
    let request = session_turn_process::request("mock-model")
        .trace_context(context.clone())
        .trace_link_mode(lash_trace::TraceLinkMode::Link);
    let encoded = serde_json::to_value(request).expect("serialize the queued start");
    let decoded = serde_json::from_value(encoded).expect("decode the queued start");
    let started = core.processes().start(decoded, core.effect_host()).await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the child's turn settles")?;
    assert!(
        matches!(&output, lash_execution_contracts::process::ProcessAwaitOutput::Settled { output }
            if output.is_success()),
        "the queued start ran successfully: {output:?}"
    );
    let starts = observed.spans("lash.process.start");
    assert_eq!(starts.len(), 1, "one producer for the queued start");
    let start = &starts[0];
    assert_eq!(
        links(start),
        vec![SpanId::from_bytes(context.span_id().to_bytes())],
        "the producer links the queued caller's span"
    );
    let linked = &start.links[0].span_context;
    assert_eq!(linked.trace_id(), trace_of(0xe2));
    assert!(linked.is_sampled());
    assert_eq!(linked.trace_state().header(), "queued=caller");
    assert_eq!(start.parent_span_id, SpanId::INVALID);
    assert_ne!(start.span_context.trace_id(), linked.trace_id());
    core.shutdown().await?;
    Ok(())
}

/// L3's first edge: a send under a caller's context opens a `lash.send`
/// producer under it, the turn's admission is that producer's child, and the
/// turn's `invoke_agent` is on the send's trace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sent_turn_joins_the_trace_of_its_send() -> Result<()> {
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(vec![text_call("done")]).into_handle(),
        mock_llm_profile_spec(),
    );
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse(SENT_SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let output = session
        .send(TurnInput::text("hello"))
        .trace_context(caller(0xe1))
        .output()
        .await?;
    assert!(output.is_success(), "{:?}", output.outcome);
    observed
        .until("the turn's terminal is exported", |observed| {
            !observed.spans("invoke_agent").is_empty()
        })
        .await;
    let sends = observed.spans("lash.send");
    let send = producer(&sends, 0xe1);
    let invoked = observed.spans("invoke_agent");
    assert_eq!(invoked.len(), 1, "{invoked:#?}");
    assert_eq!(
        invoked[0].parent_span_id,
        span_id(send),
        "the turn is its send's child"
    );
    assert_eq!(
        invoked[0].span_context.trace_id(),
        send.span_context.trace_id(),
        "invoke_agent's trace is the send's"
    );
    core.shutdown().await?;
    Ok(())
}

/// FIG-4829: the first acceptance of a send retains its producer as the
/// input's parent. A retry of the same `id` under another context is the
/// same submission: it reads the first parent back, and its own producer
/// records that it reused it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_send_keeps_the_parent_its_first_acceptance_retained() -> Result<()> {
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(Vec::new()).into_handle(),
        mock_llm_profile_spec(),
    )
    .serve_sessions(false);
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let durable = core
        .session(lash::SessionId::parse(SENT_SESSION).expect("nonblank host identity"))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let send = |context: u8| {
        durable
            .send(TurnInput::text("the same words"))
            .id(lash::TurnId::parse("retried-send").expect("nonblank host identity"))
            .trace_context(caller(context))
    };
    let first = send(1).await?;
    let retried = send(2).await?;
    assert_eq!(retried.input_id(), first.input_id());

    let sends = observed.spans("lash.send");
    let (first, retried) = (producer(&sends, 1), producer(&sends, 2));
    assert_eq!(
        attribute(first, "lash.producer.outcome").as_deref(),
        Some("selected")
    );
    assert_eq!(
        attribute(retried, "lash.producer.outcome").as_deref(),
        Some("reused")
    );
    let parents = durable
        .pending_turn_inputs()
        .await?
        .into_iter()
        .map(|read| {
            read.input
                .trace_cause
                .as_parent()
                .map(|parent| parent.carrier().span_id())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parents
            .into_iter()
            .map(|parent| parent.map(|span| SpanId::from_bytes(span.to_bytes())))
            .collect::<Vec<_>>(),
        vec![Some(span_id(first))],
        "the input retains the first acceptance's producer"
    );
    core.shutdown().await?;
    Ok(())
}

const CONSUMED_SESSION: &str = "parentage-consumed";
const T1: &str = "parentage-consumed-t1";
/// The callers of the law's sends: A opens T1, B steers T1, and C and D
/// arrive while T1 runs, admitted together as T2.
const A: u8 = 0xa;
const B: u8 = 0xb;
const C: u8 = 0xc;
const D: u8 = 0xd;

/// One node of the consumed-input law: a core whose drain composes every
/// eligible input into one run, whose `echo_tool` parks until the host
/// resolves it, observed.
fn consuming_node(
    backend: lash_execution_contracts::backend::Backend,
    owner: lash::persistence::LeaseOwnerIdentity,
    calls: Vec<lash_core::testing::runtime_helpers::MockCall>,
) -> Result<(Observed, LashCore, Keys)> {
    let (keys, published) = tokio::sync::mpsc::unbounded_channel();
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .queued_work_batching(
            lash::QueuedWorkBatchingConfig::new(4).with_drain_mode(lash::DrainMode::All),
        )
        .serve_test_llm_profile(mock_provider(calls).into_handle(), mock_llm_profile_spec())
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::new(PendingEchoTool(keys)),
        );
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(owner)?;
    Ok((observed, core, published))
}

fn node(name: &str) -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new("parentage-consumed"),
        lash::persistence::LeaseIncarnationId::new(name),
    )
}

fn pending_call(call_id: &str) -> lash_core::testing::runtime_helpers::MockCall {
    tool_call(call_id, "echo_tool", r#"{"value":"held"}"#)
}

/// Open T1 under A and, while its first call is held, steer B into it and
/// queue C then D for the next turn; returns T1's send.
async fn send_the_law(
    core: &LashCore,
    keys: &mut Keys,
) -> Result<tokio::task::JoinHandle<Result<lash::TurnOutput>>> {
    let session = created_session(
        core,
        lash::SessionId::parse(CONSUMED_SESSION).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let opened = tokio::spawn(async move {
        session
            .send(TurnInput::text("call the held tool"))
            .id(lash::TurnId::parse(T1).expect("nonblank host identity"))
            .trace_context(caller(A))
            .output()
            .await
    });
    let (key, value) = keys.recv().await.expect("T1's first call is held");
    let durable = core
        .session(lash::SessionId::parse(CONSUMED_SESSION).expect("nonblank host identity"))
        .durable()
        .await?;
    drop(
        durable
            .send(TurnInput::text("steer the running turn"))
            .ingress(lash_core::TurnInputIngress::active_turn(
                lash::TurnId::parse(T1).expect("nonblank host identity"),
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .trace_context(caller(B))
            .await?,
    );
    for (context, text) in [(C, "the next turn's head"), (D, "the next turn's tail")] {
        drop(
            durable
                .send(TurnInput::text(text))
                .trace_context(caller(context))
                .await?,
        );
    }
    assert_eq!(
        core.completions()
            .resolve(
                key.as_str(),
                lash_core::Resolution::Ok(serde_json::json!({ "payload": value })),
            )
            .await?,
        lash_core::ResolveAnswer::Resolved
    );
    Ok(opened)
}

/// The law's spans, whichever node exported them: T1's parent is A's
/// producer and its `invoke_agent` links exactly B's; T2's parent is C's
/// producer and it links exactly D's; every producer parents at most one
/// turn, and B's parents none.
fn assert_consumed_inputs_are_linked(spans: &[SpanData]) {
    let named = |name: &str| {
        spans
            .iter()
            .filter(|span| span.name == name)
            .cloned()
            .collect::<Vec<_>>()
    };
    let sends = named("lash.send");
    assert_eq!(sends.len(), 4, "one producer per send: {sends:#?}");
    let [a, b, c, d] = [A, B, C, D].map(|context| span_id(producer(&sends, context)));
    let invoked = named("invoke_agent");
    assert_eq!(invoked.len(), 2, "two terminals: {invoked:#?}");
    let first = one(&invoked, "T1's terminal", |span| {
        attribute(span, "lash.turn.id").as_deref() == Some(T1)
    });
    let second = one(&invoked, "T2's terminal", |span| {
        attribute(span, "lash.turn.id").as_deref() != Some(T1)
    });
    assert_eq!(first.parent_span_id, a, "T1's parent is A's");
    assert_eq!(links(first), vec![b], "T1 links only the input it consumed");
    assert_eq!(
        first.span_context.trace_id(),
        trace_of(A),
        "T1 is on A's trace"
    );
    assert_eq!(
        second.span_context.trace_id(),
        trace_of(C),
        "T2 is on C's trace"
    );
    assert_eq!(second.parent_span_id, c, "T2's parent is its head's");
    assert_eq!(links(second), vec![d], "T2 links the rest of its batch");
    for producer in [a, b, c, d] {
        let parented = invoked
            .iter()
            .filter(|span| span.parent_span_id == producer)
            .count();
        assert!(parented <= 1, "a producer parents at most one turn");
        let linked = invoked
            .iter()
            .filter(|span| links(span).contains(&producer))
            .count();
        assert!(linked <= 1, "a producer is linked from at most one turn");
    }
    assert!(
        invoked.iter().all(|span| span.parent_span_id != b),
        "the consumed input parents no turn"
    );
}

/// Every finished span of each node's exporter.
fn finished(nodes: &[&Observed]) -> Vec<SpanData> {
    nodes
        .iter()
        .flat_map(|observed| {
            ["lash.send", "invoke_agent"]
                .into_iter()
                .flat_map(|name| observed.spans(name))
        })
        .collect()
}

/// L19: an input a running turn consumes is linked from it, never a second
/// parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumed_input_is_linked_never_a_second_parent() -> Result<()> {
    let (observed, core, mut keys) = consuming_node(
        sqlite_memory_store_backend().await,
        lash_facade_tests::runtime_lease_owner(),
        vec![
            pending_call("call-held"),
            text_call("T1 answered"),
            text_call("T2 answered"),
        ],
    )?;
    let opened = send_the_law(&core, &mut keys).await?;
    let output = opened.await.expect("T1's send joins")?;
    assert!(output.is_success(), "{:?}", output.outcome);
    observed
        .until("both turns' terminals are exported", |observed| {
            observed.spans("invoke_agent").len() == 2
        })
        .await;
    assert_consumed_inputs_are_linked(&finished(&[&observed]));
    core.shutdown().await?;
    Ok(())
}

/// L19 across a cut: T1 is held again after it consumed B, its node is cut,
/// and another owner over the same stores finishes T1 and runs T2. B's
/// cause is retained in T1's checkpoint, so the turns link and parent
/// exactly as on one node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumed_input_stays_linked_across_a_resume_on_another_owner() -> Result<()> {
    let stores = sqlite_memory_store_set().await;
    let backend = || {
        lash_conformance::backend_over(
            Arc::clone(&stores) as Arc<dyn lash_execution_contracts::backend::StoreSet>
        )
    };
    let (observed_a, node_a, mut keys) = consuming_node(
        backend(),
        node("node-a"),
        vec![pending_call("call-held"), pending_call("call-held-again")],
    )?;
    let opened = send_the_law(&node_a, &mut keys).await?;
    // T1's second call is held after the checkpoint that delivered B.
    let (held_again, _) = keys.recv().await.expect("T1's second call is held");
    node_a.shutdown().await?;
    drop(opened);

    let (observed_b, node_b, _keys) = consuming_node(
        backend(),
        node("node-b"),
        vec![text_call("T1 answered"), text_call("T2 answered")],
    )?;
    let session = node_b
        .session(lash::SessionId::parse(CONSUMED_SESSION).expect("nonblank host identity"))
        .open()
        .await?;
    assert_eq!(
        node_b
            .completions()
            .resolve(
                held_again.as_str(),
                lash_core::Resolution::Ok(serde_json::json!({ "payload": "raw:held" })),
            )
            .await?,
        lash_core::ResolveAnswer::Resolved
    );
    let output = session
        .attach_id(lash::TurnId::parse(T1).expect("nonblank host identity"))
        .output()
        .await?;
    assert!(output.is_success(), "{:?}", output.outcome);
    observed_b
        .until("both turns' terminals are exported", |observed| {
            observed.spans("invoke_agent").len() == 2
        })
        .await;
    assert_consumed_inputs_are_linked(&finished(&[&observed_a, &observed_b]));
    node_b.shutdown().await?;
    Ok(())
}

/// A `SessionTurn` process's child turn joins its cause, the process: it is
/// the process span's child, on the process's trace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_child_shares_the_trace_of_its_process() -> Result<()> {
    let provider = lash::testing::TestProvider::builder()
        .kind("parentage-session-turn")
        .complete(|_| async { Ok(text_response("child answered")) })
        .build()
        .into_handle();
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec());
    let (observed, builder) = Observed::install(builder);
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    core.session(
        lash::SessionId::parse(session_turn_process::PARENT).expect("nonblank host identity"),
    )
    .create(lash::SessionCreation::root(mock_session_spec()))
    .await?;
    let started = core
        .processes()
        .start(
            session_turn_process::request("mock-model"),
            core.effect_host(),
        )
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&started.process_id),
    )
    .await
    .expect("the child's turn settles")?;
    assert!(
        matches!(&output, lash_execution_contracts::process::ProcessAwaitOutput::Settled { output }
            if output.is_success()),
        "the child ran successfully: {output:?}"
    );
    observed
        .until("the process and its child turn are exported", |observed| {
            !observed.spans("lash.process").is_empty() && !observed.spans("invoke_agent").is_empty()
        })
        .await;
    let process = observed.spans("lash.process");
    assert_eq!(process.len(), 1, "{process:#?}");
    let invoked = observed.spans("invoke_agent");
    assert_eq!(invoked.len(), 1, "the child's turn: {invoked:#?}");
    assert_eq!(
        invoked[0].parent_span_id,
        process[0].span_context.span_id(),
        "the child turn is the process's child"
    );
    assert_eq!(
        observed.spans("invoke_agent")[0].span_context.trace_id(),
        process[0].span_context.trace_id(),
        "the child turn shares the process's trace id"
    );
    core.shutdown().await?;
    Ok(())
}
