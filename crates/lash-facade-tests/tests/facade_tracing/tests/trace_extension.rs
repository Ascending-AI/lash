//! L8(a), L25 and L17 (FIG-6304): extension spans opened by a tool body,
//! a turn hook and a tool hook are bound from the callback's registration
//! and parented where the callback runs; `tracing` spans in a tool body
//! and in a task it spawns nest under its `execute_tool` span. L21(f):
//! skipping an extension span by export or by selector takes its point
//! and the model call under it.

use super::trace_enrichment::exporting;
use super::tracing::{text_call, tool_call};
use super::*;

use lash::TurnInput;
use lash::plugins::{
    BeforeToolDecision, HookKey, PluginDeclaration, PluginSpec, ToolArgsCheckInput, TurnHookContext,
};
use lash::tracing::otel::trace::{SpanId, TraceContextExt, Tracer, TracerProvider as _};
use lash::tracing::{TraceEvent, TraceRecord};
use lash_core::testing::runtime_helpers::mock_provider;
use lash_trace::otel::OtelOptions;
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracerProvider, SpanData};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const OWNER: &str = "extension-laws";

fn spanning_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:spanning",
        "spanning",
        "Open extension and tracing spans",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// What the tool body saw of its capability.
#[derive(Default)]
struct Seen {
    recording: Mutex<Option<bool>>,
    call: Mutex<Option<lash_core::CallAddress>>,
    tool_span: Mutex<Option<lash::tracing::TraceCarrier>>,
    hooks: Mutex<Vec<(String, bool, Option<lash_core::CallAddress>)>>,
}

/// A tool whose body opens an extension span with a child, a `tracing`
/// span, and a spawned task that opens another.
struct Spanning(Arc<Seen>);

#[async_trait]
impl ToolProvider for Spanning {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![spanning_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "spanning").then(|| Arc::new(spanning_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let trace = call.context.trace().clone();
        *self.0.recording.lock().unwrap() = Some(trace.is_recording());
        *self.0.call.lock().unwrap() = trace.call_address().cloned();
        let span = trace.span("tool_body");
        *self.0.tool_span.lock().unwrap() = call.context.tool_span_context();
        span.child("tool_child").end();
        span.end();
        ::tracing::info_span!("fetch").in_scope(|| {});
        tokio::spawn(trace.instrument(async {
            ::tracing::info_span!("fetch_task").in_scope(|| {});
        }))
        .await
        .expect("the task");
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

fn plugin(seen: Arc<Seen>) -> Arc<dyn PluginFactory> {
    let before = seen.clone();
    let check = seen.clone();
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial(OWNER),
        PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), Arc::new(Spanning(seen)))
            .with_before_turn(
                HookKey::new("spans").unwrap(),
                Arc::new(move |context: TurnHookContext| {
                    let trace = context.trace.clone();
                    before.hooks.lock().unwrap().push((
                        "before_turn".to_owned(),
                        trace.is_recording(),
                        trace.call_address().cloned(),
                    ));
                    Box::pin(async move {
                        trace.span("before_turn").end();
                        Ok(Default::default())
                    })
                }),
            )
            .with_tool_args_check(
                HookKey::new("spans").unwrap(),
                Arc::new(move |input: ToolArgsCheckInput| {
                    let trace = input.context.trace.clone();
                    check.hooks.lock().unwrap().push((
                        "args_check".to_owned(),
                        trace.is_recording(),
                        trace.call_address().cloned(),
                    ));
                    Box::pin(async move {
                        trace.span("args_check").end();
                        Ok(BeforeToolDecision::Allow)
                    })
                }),
            ),
    ))
}

/// One served turn that calls `spanning` once, with every record and the
/// exported spans.
async fn spanning_turn(
    session: &str,
    seen: Arc<Seen>,
) -> Result<(Vec<TraceRecord>, Vec<SpanData>, InMemorySpanExporter)> {
    let sink = Arc::new(lash::testing::RecordingTraceSink::new());
    let ((spans, _tracer), builder) = exporting(
        explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(
            mock_provider(vec![
                tool_call("call-spanning", "spanning", "{}"),
                text_call("done"),
            ])
            .into_handle(),
            mock_llm_profile_spec(),
        )
        .plugin(plugin(seen))
        .trace_sink(sink.clone()),
        Sampler::AlwaysOn,
        OtelOptions::standard(),
        None,
    );
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let output = created_session(&core, lash::SessionId::parse(session).expect("an id"))
        .await
        .open()
        .await?
        .send(TurnInput::text("open spans"))
        .output()
        .await?;
    assert!(
        matches!(output.outcome, lash::TurnOutcome::Finished(_)),
        "{:?}",
        output.outcome
    );
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
    Ok((sink.records(), exported, spans))
}

fn exported<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name == name || span.name.starts_with(&format!("{name} ")))
        .unwrap_or_else(|| {
            panic!(
                "{name} is exported: {:?}",
                spans.iter().map(|span| &span.name).collect::<Vec<_>>()
            )
        })
}

/// L8(a) and L25: each extension span is attributed to the registration
/// of the callback that opened it and parented where it ran: the tool
/// body's under the `execute_tool` span and its child under it, the turn
/// hook's under the turn span, the tool-args check's under the tool span.
/// The tool body's `call_address()` is its call's; the hooks' is `None`.
/// The body reads its own tool span's context through
/// `tool_span_context()`, equal to the exported span's, from inside a span
/// of its own (Figments feedback 24 (a)1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extension_spans_are_bound_from_the_registration_and_parented_where_they_run() -> Result<()>
{
    let seen = Arc::new(Seen::default());
    let (records, spans, _) = spanning_turn("l25-extension", seen.clone()).await?;

    let extension = |name: &str| {
        records
            .iter()
            .find_map(|record| match &record.event {
                TraceEvent::ExtensionSpan(span) if span.name == name => {
                    Some((record.clone(), span.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "{name} is recorded: {:?}, hooks {:?}, body recording {:?}",
                    records
                        .iter()
                        .filter_map(|record| match &record.event {
                            TraceEvent::ExtensionSpan(span) => Some(span.name.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    seen.hooks.lock().unwrap(),
                    seen.recording.lock().unwrap(),
                )
            })
    };
    for (name, key) in [
        ("tool_body", "tool_provider:tools"),
        ("tool_child", "tool_provider:tools"),
        ("before_turn", "before_turn:spans"),
        ("args_check", "tool_args_check:spans"),
    ] {
        let (_, span) = extension(name);
        assert_eq!(
            (span.plugin_id.as_str(), span.callback.as_str()),
            (OWNER, key),
            "{name}"
        );
    }

    let tool = exported(&spans, "execute_tool");
    let turn = exported(&spans, "invoke_agent");
    let body = exported(&spans, "tool_body");
    for (name, parent) in [
        ("tool_body", tool),
        ("tool_child", body),
        ("before_turn", turn),
        ("args_check", tool),
    ] {
        let span = exported(&spans, name);
        assert_eq!(
            span.parent_span_id,
            parent.span_context.span_id(),
            "{name} is parented under {}",
            parent.name
        );
        assert_eq!(span.span_context.trace_id(), turn.span_context.trace_id());
    }

    assert_eq!(*seen.recording.lock().unwrap(), Some(true));
    let call = seen.call.lock().unwrap().clone().expect("the body's call");
    assert_eq!(
        super::trace_enrichment::attribute(tool, "gen_ai.tool.call.id").as_deref(),
        Some(call.call_id.as_str()),
        "the body's call address joins its tool span"
    );
    let context = seen
        .tool_span
        .lock()
        .unwrap()
        .clone()
        .expect("the body's tool span context");
    assert!(
        context.traceparent().contains(&format!(
            "{}-{}",
            tool.span_context.trace_id(),
            tool.span_context.span_id()
        )),
        "the body reads its own exported tool span's context: {}",
        context.traceparent()
    );
    let hooks = seen.hooks.lock().unwrap().clone();
    assert_eq!(hooks.len(), 2, "{hooks:?}");
    for (name, recording, call) in hooks {
        assert!(recording, "{name} records");
        assert_eq!(call, None, "{name} has no call address");
    }
    Ok(())
}

/// The OpenTelemetry context a `tracing` span carries.
struct OtelData(lash::tracing::otel::Context);

thread_local! {
    static ACTIVE: std::cell::RefCell<Vec<lash::tracing::otel::ContextGuard>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `tracing-opentelemetry` 0.33's parent rule at its default
/// `context_activation = true`, which lash's ambient context targets
/// (spec §7.7, `layer.rs:668, 945-949`): entering a span makes its context
/// current, and a contextual span's parent is the current context.
struct ActivatingLayer(opentelemetry_sdk::trace::SdkTracer);

impl<S> tracing_subscriber::Layer<S> for ActivatingLayer
where
    S: ::tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &::tracing::span::Attributes<'_>,
        id: &::tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let parent = match attrs.parent() {
            Some(parent) => ctx
                .span(parent)
                .and_then(|span| {
                    span.extensions()
                        .get::<OtelData>()
                        .map(|data| data.0.clone())
                })
                .unwrap_or_default(),
            None if attrs.is_root() => lash::tracing::otel::Context::new(),
            None => lash::tracing::otel::Context::current(),
        };
        let span = self.0.start_with_context(attrs.metadata().name(), &parent);
        if let Some(entry) = ctx.span(id) {
            entry
                .extensions_mut()
                .insert(OtelData(parent.with_span(span)));
        }
    }

    fn on_enter(&self, id: &::tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(data) = ctx.span(id).and_then(|span| {
            span.extensions()
                .get::<OtelData>()
                .map(|data| data.0.clone())
        }) {
            ACTIVE.with(|active| active.borrow_mut().push(data.attach()));
        }
    }

    fn on_exit(&self, id: &::tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if ctx
            .span(id)
            .is_some_and(|span| span.extensions().get::<OtelData>().is_some())
        {
            ACTIVE.with(|active| drop(active.borrow_mut().pop()));
        }
    }

    fn on_close(&self, id: ::tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(data) = ctx
            .span(&id)
            .and_then(|span| span.extensions_mut().remove::<OtelData>())
        {
            data.0.span().end();
        }
    }
}

/// L17: with a host layer at its default settings, a `tracing` span the
/// tool body opens, and one a task it spawned under `trace.instrument`
/// opens, export under the `execute_tool` span in its trace, not under the
/// unrelated host span current when the session actor was spawned.
#[tokio::test(flavor = "current_thread")]
async fn tracing_spans_in_a_tool_body_nest_under_its_tool_span() -> Result<()> {
    use ::tracing::Instrument as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let host = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(host.clone())
        .build();
    let subscriber =
        tracing_subscriber::Registry::default().with(ActivatingLayer(provider.tracer("host")));
    let _subscriber = ::tracing::subscriber::set_default(subscriber);

    let r1 = ::tracing::info_span!("R1");
    let seen = Arc::new(Seen::default());
    let (_, spans, _) = spanning_turn("l17-tracing", seen)
        .instrument(r1.clone())
        .await?;
    drop(r1);
    let _ = provider.force_flush();
    let hosted = host.get_finished_spans().expect("host spans");
    let r1 = hosted
        .iter()
        .find(|span| span.name == "R1")
        .expect("R1 is exported");
    let tool = exported(&spans, "execute_tool");
    for name in ["fetch", "fetch_task"] {
        let span = hosted
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("{name} is exported"));
        assert_eq!(
            span.parent_span_id,
            tool.span_context.span_id(),
            "{name} nests under execute_tool"
        );
        assert_eq!(span.span_context.trace_id(), tool.span_context.trace_id());
        assert_ne!(span.parent_span_id, r1.span_context.span_id());
        assert_ne!(span.parent_span_id, SpanId::INVALID);
    }
    Ok(())
}

fn nesting_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:nesting",
        "nesting",
        "Open an extension span with a point and a direct completion under it",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A tool whose body opens an extension span and, under it, records a
/// point and makes one direct completion; builders count each
/// construction.
struct Nesting(Arc<AtomicUsize>);

#[async_trait]
impl ToolProvider for Nesting {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![nesting_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == "nesting").then(|| Arc::new(nesting_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let built = Arc::clone(&self.0);
        let mut span = call.context.trace().span("outer");
        span.content("prompt", || {
            built.fetch_add(1, Ordering::SeqCst);
            "built".into()
        });
        span.trace().event("note", |_| {
            built.fetch_add(1, Ordering::SeqCst);
        });
        let completion = span
            .trace()
            .instrument(call.context.direct_completions().complete(
                lash_core::facade_support::DirectRequest::text("nested prompt"),
                "nested-direct",
            ))
            .await;
        span.end();
        match completion {
            Ok(completion) => {
                lash_core::ToolOutcome::ok(serde_json::json!({ "text": completion.text })).into()
            }
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

/// Skips the extension kinds, children first, for the `nesting` body.
struct SkipsTheBody;

impl lash::tracing::CaptureSelector for SkipsTheBody {
    fn select(
        &self,
        at: &lash::tracing::SelectorView<'_>,
        narrow: &mut lash::tracing::TraceNarrowing,
    ) {
        if at.kind == lash::tracing::TraceSpanKind::Tool && at.ids.call.is_some() {
            for kind in [
                lash::tracing::TraceSpanKind::ModelAttempt,
                lash::tracing::TraceSpanKind::Model,
                lash::tracing::TraceSpanKind::Point,
                lash::tracing::TraceSpanKind::Extension,
            ] {
                narrow.skip(kind).expect("children first");
            }
        }
    }
}

/// One served turn that calls `nesting`, under `export` and `selector`:
/// every record, the exported spans, and how many builders ran.
async fn nesting_turn(
    session: &str,
    export: lash::tracing::TraceExportPolicy,
    selector: Option<Arc<dyn lash::tracing::CaptureSelector>>,
) -> Result<(Vec<TraceRecord>, Vec<SpanData>, usize)> {
    let built = Arc::new(AtomicUsize::new(0));
    let sink = Arc::new(lash::testing::RecordingTraceSink::new());
    let mut builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        mock_provider(vec![
            tool_call("call-nesting", "nesting", "{}"),
            text_call("nested answer"),
            text_call("done"),
        ])
        .into_handle(),
        mock_llm_profile_spec(),
    )
    .tools(
        lash::tool_provider_key!("tools"),
        Arc::new(Nesting(Arc::clone(&built))),
    )
    .content_policy(lash::tracing::ContentPolicy::captured())
    .trace_sink(sink.clone());
    if let Some(selector) = selector {
        builder = builder.capture_selector(selector);
    }
    let ((spans, _tracer), builder) = exporting(
        builder,
        Sampler::AlwaysOn,
        OtelOptions {
            export,
            ..OtelOptions::standard()
        },
        None,
    );
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let output = created_session(&core, lash::SessionId::parse(session).expect("an id"))
        .await
        .open()
        .await?
        .send(TurnInput::text("nest"))
        .output()
        .await?;
    assert!(
        matches!(output.outcome, lash::TurnOutcome::Finished(_)),
        "{:?}",
        output.outcome
    );
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
    Ok((sink.records(), exported, built.load(Ordering::SeqCst)))
}

fn of_kind<'a>(records: &'a [TraceRecord], kind: &str) -> Vec<&'a TraceRecord> {
    records
        .iter()
        .filter(|record| record.event.kind().as_str() == kind)
        .collect()
}

fn named_extension<'a>(records: &'a [TraceRecord], name: &str) -> Vec<&'a TraceRecord> {
    records
        .iter()
        .filter(
            |record| matches!(&record.event, TraceEvent::ExtensionSpan(span) if span.name == name),
        )
        .collect()
}

fn chats(spans: &[SpanData]) -> Vec<&SpanData> {
    spans
        .iter()
        .filter(|span| span.name == "chat" || span.name.starts_with("chat "))
        .collect()
}

/// Every exported span's parent is exported, but for the producer root.
fn no_orphans(spans: &[SpanData]) {
    for span in spans {
        assert!(
            span.parent_span_id == SpanId::INVALID
                || span.name == "lash.send"
                || spans
                    .iter()
                    .any(|parent| parent.span_context.span_id() == span.parent_span_id),
            "{} is exported under a parent that was",
            span.name
        );
    }
}

/// L21(f): an exporter skipping the extension kinds and model calls
/// exports none of the body's extension span, its point or the `chat`
/// under it, while their sink records are present; a selector that skips
/// the same four, children first, for this body builds none of them, and
/// the turn's own `chat` is exported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extension_skip_takes_its_point_and_model_call_with_it() -> Result<()> {
    use lash::tracing::{TraceExportPolicy, TraceSpanKind as K};

    let (records, spans, built) =
        nesting_turn("l21-standard", TraceExportPolicy::standard(), None).await?;
    assert_eq!(built, 2, "the content and point builders ran");
    let outer = exported(&spans, "outer");
    let note = exported(&spans, "note");
    assert_eq!(note.parent_span_id, outer.span_context.span_id());
    let nested = chats(&spans)
        .into_iter()
        .filter(|chat| chat.parent_span_id == outer.span_context.span_id())
        .count();
    assert_eq!(nested, 1, "the direct completion nests under the span");
    assert_eq!(of_kind(&records, "llm_call_completed").len(), 3);
    no_orphans(&spans);

    let skip = TraceExportPolicy::builder()
        .skip(K::ModelAttempt)
        .skip(K::Model)
        .skip(K::Point)
        .skip(K::Extension)
        .build()
        .expect("children skipped with their parents");
    let (records, spans, built) = nesting_turn("l21-export-skip", skip, None).await?;
    assert_eq!(built, 2, "an export skip still builds");
    assert!(
        spans
            .iter()
            .all(|span| span.name != "outer" && span.name != "note"),
        "no extension span or point is exported"
    );
    assert!(chats(&spans).is_empty(), "no chat span is exported");
    assert_eq!(named_extension(&records, "outer").len(), 1);
    assert_eq!(named_extension(&records, "note").len(), 1);
    assert_eq!(of_kind(&records, "llm_call_completed").len(), 3);
    no_orphans(&spans);

    let (records, spans, built) = nesting_turn(
        "l21-selector-skip",
        TraceExportPolicy::standard(),
        Some(Arc::new(SkipsTheBody)),
    )
    .await?;
    assert_eq!(built, 0, "nothing of the skipped body is built");
    assert!(named_extension(&records, "outer").is_empty());
    assert!(named_extension(&records, "note").is_empty());
    assert_eq!(
        of_kind(&records, "llm_call_completed").len(),
        2,
        "only the turn's own model calls are built"
    );
    assert_eq!(chats(&spans).len(), 2, "the turn's own chats are exported");
    no_orphans(&spans);
    Ok(())
}
