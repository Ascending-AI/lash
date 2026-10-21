//! The extension capability's body laws (FIG-6304): L8 (b)–(f), L21 (d)
//! and the selector half of (f), L22's body clauses with (b), and L23.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll};

use lash_trace::{
    CaptureSelector, ContentPolicy, DurableTraceScope, ExtensionSpanEnd, ExtensionSpanRecord,
    ProducerKind, RecordingTraceSink, SelectorView, SpanRole, SpanSpec, TraceCall, TraceContext,
    TraceDetail, TraceDetailPolicy, TraceEvent, TraceIds, TraceLimits, TraceLinks, TraceNarrowing,
    TraceOmission, TraceOmissionSite, TraceOmissionSiteKind, TraceRecord, TraceScopeId,
    TraceScopeOwner, TraceSpanKind, TraceValue, W3cSpanId,
};

use super::*;
use crate::trace::{JournalFrontier, StepIssue, TraceRuntime};
use lash_vocabulary::providers::PluginCallbackIdentity;

/// A wall clock that moves one millisecond each time it is read, so every
/// end is strictly after what it follows.
#[derive(Debug, Default)]
struct Ticking(AtomicU64);

#[async_trait::async_trait]
impl crate::Clock for Ticking {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let at = 1_700_000_000_000 + i64::try_from(self.0.fetch_add(1, Ordering::SeqCst)).unwrap();
        chrono::DateTime::from_timestamp_millis(at).unwrap()
    }

    async fn sleep(&self, _: std::time::Duration) {}

    async fn sleep_until(&self, _: std::time::Instant) {}
}

/// A runtime recording every record, and an observed turn scope in it.
struct Fixture {
    sink: Arc<RecordingTraceSink>,
    runtime: TraceRuntime,
    scope: DurableTraceScope,
}

impl Fixture {
    fn new(limits: TraceLimits) -> Self {
        Self::with(limits, TraceDetailPolicy::standard(), None)
    }

    fn with(
        limits: TraceLimits,
        detail: TraceDetailPolicy,
        selector: Option<Arc<dyn CaptureSelector>>,
    ) -> Self {
        let sink = Arc::new(RecordingTraceSink::new());
        let mut runtime = TraceRuntime::new(Arc::new(Ticking::default()))
            .with_limits(limits)
            .with_detail(detail)
            .with_content(ContentPolicy::captured())
            .with_trace_sink(sink.clone());
        if let Some(selector) = selector {
            runtime = runtime.with_capture_selector(selector);
        }
        let producer = runtime.open_producer(
            ProducerKind::Send,
            &TraceCall::default(),
            TraceIds::default(),
        );
        let scope = DurableTraceScope::admit(
            TraceScopeId::admission(TraceScopeOwner::Turn {
                session_id: crate::SessionId::from("extension-laws"),
                turn_id: crate::TurnId::from("extension-laws-turn"),
            }),
            producer.parent().as_ref(),
            TraceLinks::new(),
            0,
        );
        Self {
            sink,
            runtime,
            scope,
        }
    }

    /// The live standing of a body under the turn scope.
    fn standing(&self) -> crate::trace::TraceStanding {
        self.runtime.unreplayed(Some(self.scope.clone()))
    }

    /// A tool provider body under the turn scope, bound from `plugin`'s
    /// registration and run for `call`.
    fn body(&self, call: Option<crate::CallAddress>) -> (ExtensionTrace, BodyTrace) {
        self.body_at(self.standing(), call)
    }

    fn body_at(
        &self,
        standing: crate::trace::TraceStanding,
        call: Option<crate::CallAddress>,
    ) -> (ExtensionTrace, BodyTrace) {
        let mut spec = BodySpec::at(standing, TraceContext::default());
        spec.callback = BoundCallback::of(&provider());
        spec.call = call;
        spec.bind()
    }

    fn records(&self) -> Vec<TraceRecord> {
        self.sink.records()
    }

    /// Every extension record, in publication order.
    fn extensions(&self) -> Vec<(TraceRecord, ExtensionSpanRecord)> {
        extensions(&self.records())
    }

    fn scope_span(&self) -> W3cSpanId {
        self.scope.span.as_ref().unwrap().span_id()
    }

    fn ledger(&self, site: TraceOmissionSiteKind, reason: TraceOmission) -> u64 {
        self.runtime.omissions().report().get(site, reason)
    }
}

fn provider() -> PluginCallbackIdentity {
    PluginCallbackIdentity::tool_provider("extension-laws", "tools")
}

fn extensions(records: &[TraceRecord]) -> Vec<(TraceRecord, ExtensionSpanRecord)> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::ExtensionSpan(span) => Some((record.clone(), span.clone())),
            _ => None,
        })
        .collect()
}

fn named<'a>(
    extensions: &'a [(TraceRecord, ExtensionSpanRecord)],
    name: &str,
) -> Vec<&'a (TraceRecord, ExtensionSpanRecord)> {
    extensions
        .iter()
        .filter(|(_, span)| span.name == name)
        .collect()
}

fn span_of(record: &TraceRecord) -> W3cSpanId {
    record.position.as_ref().unwrap().span.span_id()
}

fn parent_of(record: &TraceRecord) -> W3cSpanId {
    record
        .position
        .as_ref()
        .unwrap()
        .parent
        .as_ref()
        .unwrap()
        .span_id()
}

fn ended_at(record: &TraceRecord) -> u64 {
    record
        .position
        .as_ref()
        .unwrap()
        .ended_at
        .unwrap()
        .unix_ms()
}

/// No published record names a parent that is neither the turn's span nor
/// a span published before it: P3, over what the sink saw in order.
fn orphans(fixture: &Fixture) -> Vec<String> {
    let mut published = vec![fixture.scope_span()];
    let mut orphans = Vec::new();
    for record in fixture.records() {
        let Some(position) = &record.position else {
            continue;
        };
        if !position.role.publishes() {
            continue;
        }
        let parent = position.parent.as_ref().map(|parent| parent.span_id());
        if let Some(parent) = parent
            && !published.contains(&parent)
        {
            orphans.push(format!(
                "{:?} names unpublished {parent:?}",
                record.event.kind()
            ));
        }
        published.push(position.span.span_id());
    }
    orphans
}

/// Opens a model call under `trace`'s position and closes it, as a direct
/// completion made through it does: its closing record carries `bytes` of
/// content.
fn chat(trace: &ExtensionTrace, bytes: usize) {
    let standing = trace
        .standing()
        .unwrap()
        .selected(TraceSpanKind::Model, &TraceIds::default());
    let open = standing.open(TraceSpanKind::Model, || {
        (TraceContext::default(), compaction_record())
    });
    open.close(|| {
        (
            TraceContext::default(),
            TraceEvent::ContextPressure {
                plugin_id: "chat".to_owned(),
                hook_id: "close".to_owned(),
                task: "x".repeat(bytes),
                frame_node_id: "frame".to_owned(),
            },
        )
    });
}

fn compaction_record() -> TraceEvent {
    TraceEvent::CompactionCompleted { summary_nodes: 1 }
}

fn is_chat_close(record: &TraceRecord) -> bool {
    matches!(record.event, TraceEvent::ContextPressure { .. })
        && record
            .position
            .as_ref()
            .is_some_and(|position| position.role == SpanRole::Closes)
}

fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    let waker = futures_util::task::noop_waker();
    future.poll(&mut Context::from_waker(&waker))
}

/// L8(b): a body that returns while a task it spawned still holds its
/// parent span closes that parent at its exit (`body_exit`), released
/// before its ended child; the task's late `end()` emits nothing and is
/// counted, and no record names an unpublished parent.
#[test]
fn a_body_exit_closes_what_a_task_still_holds_and_its_late_end_is_counted() {
    let fixture = Fixture::new(TraceLimits::standard());
    let (trace, body) = fixture.body(None);
    let parent = trace.span("parent");
    parent.child("child").end();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let task = std::thread::spawn(move || {
        released.recv().unwrap();
        parent.end();
    });
    body.finish();
    release.send(()).unwrap();
    task.join().unwrap();

    let extensions = fixture.extensions();
    let parent = named(&extensions, "parent");
    let child = named(&extensions, "child");
    assert_eq!((parent.len(), child.len()), (1, 1), "{extensions:#?}");
    assert_eq!(parent[0].1.ended_by, Some(ExtensionSpanEnd::BodyExit));
    assert_eq!(parent_of(&child[0].0), span_of(&parent[0].0));
    let order = |name| extensions.iter().position(|(_, span)| span.name == name);
    assert!(order("parent") < order("child"));
    assert_eq!(
        fixture.ledger(TraceOmissionSiteKind::Body, TraceOmission::Late),
        1
    );
    assert_eq!(orphans(&fixture), Vec::<String>::new());
}

/// L8(c): a body future dropped mid-span emits its open spans `cancelled`,
/// and the handles it dropped afterwards count nothing.
#[test]
fn a_body_dropped_mid_span_emits_its_open_spans_cancelled() {
    let fixture = Fixture::new(TraceLimits::standard());
    let (trace, body) = fixture.body(None);
    let mut guarded = Box::pin(body.guard(async move {
        let _outer = trace.span("outer");
        let _inner = _outer.child("inner");
        std::future::pending::<()>().await;
    }));
    assert!(poll_once(guarded.as_mut()).is_pending());
    drop(guarded);

    let extensions = fixture.extensions();
    assert_eq!(extensions.len(), 2, "{extensions:#?}");
    for (_, span) in &extensions {
        assert_eq!(span.ended_by, Some(ExtensionSpanEnd::Cancelled), "{span:?}");
    }
    assert_eq!(
        fixture.ledger(TraceOmissionSiteKind::Body, TraceOmission::Late),
        0
    );
    assert_eq!(orphans(&fixture), Vec::<String>::new());
}

/// L8(d): a body that panics with spans open emits them `panic`: one its
/// panicking poll drops as it unwinds, and one that outlives the unwind
/// (held outside the body), which only the body's exit closes.
#[test]
fn a_body_that_panics_emits_its_open_spans_panic() {
    let fixture = Fixture::new(TraceLimits::standard());
    let (trace, body) = fixture.body(None);
    let kept = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&kept);
    let mut guarded = Box::pin(body.guard(async move {
        *slot.lock().unwrap() = Some(trace.span("held"));
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        let _local = trace.span("local");
        panic!("the body panics");
    }));
    assert!(poll_once(guarded.as_mut()).is_pending());
    let panicked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(guarded.as_mut())));
    assert!(panicked.is_err());
    drop(guarded);
    drop(kept.lock().unwrap().take());

    let extensions = fixture.extensions();
    assert_eq!(extensions.len(), 2, "{extensions:#?}");
    for (_, span) in &extensions {
        assert_eq!(span.ended_by, Some(ExtensionSpanEnd::Panic), "{span:?}");
    }
    assert_eq!(orphans(&fixture), Vec::<String>::new());
}

/// L8(e): an `end()` racing the body's return is emitted once, by exactly
/// one of them: ended by the plugin, or closed at the exit and the end
/// counted late.
#[test]
fn an_end_racing_the_body_exit_is_emitted_once_or_counted_late() {
    for _ in 0..200 {
        let fixture = Fixture::new(TraceLimits::standard());
        let (trace, body) = fixture.body(None);
        let span = trace.span("raced");
        let start = Arc::new(Barrier::new(2));
        let ender = {
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                span.end();
            })
        };
        start.wait();
        body.finish();
        ender.join().unwrap();

        let extensions = fixture.extensions();
        let raced = named(&extensions, "raced");
        assert_eq!(raced.len(), 1, "{extensions:#?}");
        let late = fixture.ledger(TraceOmissionSiteKind::Body, TraceOmission::Late);
        match raced[0].1.ended_by {
            Some(ExtensionSpanEnd::End) => assert_eq!(late, 0),
            Some(ExtensionSpanEnd::BodyExit) => assert_eq!(late, 1),
            other => panic!("ended by {other:?}"),
        }
    }
}

/// L8(f): a protocol body's span opened by a shift behind its journal
/// frontier is held with the shift's observations: none is published when
/// the journal serves the step, and they are released when a step body
/// crosses the frontier.
#[test]
fn a_shift_behind_its_frontier_holds_its_extension_records_until_it_crosses() {
    for served in [true, false] {
        let fixture = Fixture::new(TraceLimits::standard());
        let frontier = JournalFrontier::new();
        frontier.bind_runtime(fixture.runtime.clone());
        let standing = fixture.runtime.standing(
            Some(fixture.scope.clone()),
            crate::trace::runtime::EmissionRight::Shift {
                frontier: frontier.clone(),
                attempt: None,
            },
        );
        let issue = StepIssue::new(frontier.clone(), None, Some(fixture.scope.clone()));
        let (trace, body) = fixture.body_at(standing, None);
        trace.span("protocol").child("step").end();
        body.finish();
        assert!(
            fixture.extensions().is_empty(),
            "nothing before the frontier"
        );
        if served {
            drop(issue);
            assert!(
                fixture.extensions().is_empty(),
                "a served step publishes none"
            );
        } else {
            let _live = issue.begin(None);
            let extensions = fixture.extensions();
            assert_eq!(
                extensions
                    .iter()
                    .map(|(_, span)| span.name.as_str())
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["protocol", "step"])
            );
            assert_eq!(orphans(&fixture), Vec::<String>::new());
        }
    }
}

/// L21(d): with every extension-related kind off, or with no extension
/// spans allowed and model calls off, a body is not recording and no
/// builder of an extension span, point or model call runs.
#[test]
fn extension_kinds_off_build_nothing() {
    let all_off = TraceDetailPolicy::builder()
        .set(TraceSpanKind::Extension, TraceDetail::Off)
        .set(TraceSpanKind::Point, TraceDetail::Off)
        .set(TraceSpanKind::Model, TraceDetail::Off)
        .set(TraceSpanKind::ModelAttempt, TraceDetail::Off)
        .build()
        .unwrap();
    let models_off = TraceDetailPolicy::builder()
        .set(TraceSpanKind::Model, TraceDetail::Off)
        .set(TraceSpanKind::ModelAttempt, TraceDetail::Off)
        .build()
        .unwrap();
    let no_spans = TraceLimits::builder().extension_spans(0).build().unwrap();
    for (limits, detail) in [(TraceLimits::standard(), all_off), (no_spans, models_off)] {
        let fixture = Fixture::with(limits, detail, None);
        let (trace, body) = fixture.body(None);
        let built = AtomicUsize::new(0);
        let count = || {
            built.fetch_add(1, Ordering::SeqCst);
        };
        assert!(!trace.is_recording());
        let mut span = trace.span("off");
        assert!(!span.is_recording());
        span.content("prompt", || {
            count();
            TraceValue::from("built")
        })
        .event("event", |_| count());
        trace.event("point", |_| count());
        span.trace().event("nested", |_| count());
        let model = trace
            .standing()
            .unwrap()
            .selected(TraceSpanKind::Model, &TraceIds::default())
            .open(TraceSpanKind::Model, || {
                count();
                (TraceContext::default(), compaction_record())
            });
        model.close(|| {
            count();
            (TraceContext::default(), compaction_record())
        });
        drop(span);
        body.finish();
        assert_eq!(built.load(Ordering::SeqCst), 0);
        assert!(fixture.extensions().is_empty());
    }
}

/// Skips the extension kinds, children first, for the tool span of one
/// call only.
struct SkipsCall(crate::ToolCallId);

impl CaptureSelector for SkipsCall {
    fn select(&self, at: &SelectorView<'_>, narrow: &mut TraceNarrowing) {
        if at.kind == TraceSpanKind::Tool && at.ids.call.as_ref() == Some(&self.0) {
            for kind in [
                TraceSpanKind::ModelAttempt,
                TraceSpanKind::Model,
                TraceSpanKind::Point,
                TraceSpanKind::Extension,
            ] {
                narrow.skip(kind).unwrap();
            }
        }
    }
}

/// L21(f), the selector half: a selector that skips the extension kinds,
/// children first, for one body builds none of its extension span, point or
/// model call, while the turn's own model call is built.
#[test]
fn a_selector_skip_for_one_body_builds_nothing_of_it() {
    let call = crate::ToolCallId::fixture("skipped");
    let fixture = Fixture::with(
        TraceLimits::standard(),
        TraceDetailPolicy::standard(),
        Some(Arc::new(SkipsCall(call.clone()))),
    );
    let ids = TraceIds {
        call: Some(call),
        ..TraceIds::default()
    };
    let standing = fixture.standing().selected(TraceSpanKind::Tool, &ids);
    let (trace, body) = fixture.body_at(standing, None);
    let built = AtomicUsize::new(0);
    let mut span = trace.span("skipped");
    span.content("prompt", || {
        built.fetch_add(1, Ordering::SeqCst);
        TraceValue::from("built")
    });
    span.trace().event("point", |_| {
        built.fetch_add(1, Ordering::SeqCst);
    });
    span.trace()
        .standing()
        .unwrap()
        .selected(TraceSpanKind::Model, &TraceIds::default())
        .open(TraceSpanKind::Model, || {
            built.fetch_add(1, Ordering::SeqCst);
            (TraceContext::default(), compaction_record())
        })
        .close(|| {
            built.fetch_add(1, Ordering::SeqCst);
            (TraceContext::default(), compaction_record())
        });
    span.end();
    body.finish();
    assert_eq!(built.load(Ordering::SeqCst), 0);
    assert!(fixture.extensions().is_empty());

    // The turn's own model call is outside the skipped body.
    fixture
        .standing()
        .selected(TraceSpanKind::Model, &TraceIds::default())
        .open(TraceSpanKind::Model, || {
            (TraceContext::default(), compaction_record())
        })
        .close(|| (TraceContext::default(), compaction_record()));
    assert!(
        fixture
            .records()
            .iter()
            .any(|record| record.position.as_ref().is_some_and(|position| {
                position.kind == TraceSpanKind::Model && position.role == SpanRole::Closes
            }))
    );
}

/// L22(a)'s body clauses: every refusal of one bounded writer is its exact
/// typed omission, and the body's charged bytes never pass
/// `extension_bytes`.
#[test]
fn a_body_contribution_is_refused_with_its_exact_omission() {
    let limits = TraceLimits::standard();
    let fixture = Fixture::new(limits);
    let annotations = Arc::new(std::sync::Mutex::new(
        lash_trace::ToolSpanAnnotations::default(),
    ));
    let mut spec = BodySpec::at(fixture.standing(), TraceContext::default());
    spec.callback = BoundCallback::of(&provider());
    spec.tool_span = Some(annotations.clone());
    let (trace, body) = spec.bind();
    let charged = |trace: &ExtensionTrace| limits.extension_bytes() - trace.remaining().bytes;

    let mut span = trace.span("flooded");
    for index in 0..10_000 {
        span.attr(format!("body.k{index:05}"), "v");
        assert!(charged(&trace) <= limits.extension_bytes());
    }
    assert!(span.omissions().get(TraceOmission::Attributes) > 0);
    let before = span.omissions();
    let reasons = [
        (TraceOmission::ValueTooLarge, {
            let mut span = trace.span("json");
            span.attr("body.json", serde_json::json!({ "v": "x".repeat(1 << 20) }));
            span.omissions()
        }),
        (TraceOmission::ValueTruncated, {
            let mut span = trace.span("string");
            span.attr("body.string", "é".repeat(1 << 19));
            span.omissions()
        }),
        (TraceOmission::NonFinite, {
            let mut span = trace.span("nan");
            span.attr("body.nan", f64::NAN);
            span.omissions()
        }),
        (TraceOmission::ReservedKey, {
            let mut span = trace.span("reserved");
            span.attr("lash.owned", "v");
            span.omissions()
        }),
        (TraceOmission::Key, {
            let mut span = trace.span("key");
            span.attr("k".repeat(2048), "v");
            span.omissions()
        }),
    ];
    for (reason, omissions) in reasons {
        assert_eq!(omissions.get(reason), 1, "{reason}: {omissions:?}");
        assert_eq!(omissions.total(), 1, "{reason}: {omissions:?}");
    }
    let long_name = trace.span("n".repeat(20 * 1024));
    assert!(!long_name.is_recording());
    drop(long_name);
    assert_eq!(
        trace.omissions().counts.get(TraceOmission::Name),
        1,
        "{:?}",
        trace.omissions()
    );
    let tool_span = trace.tool_span();
    assert_eq!(
        tool_span.set("foo.bar", "v"),
        Err(TraceOmission::DisallowedKey)
    );
    for index in 0..10 {
        assert_eq!(tool_span.set(format!("mcp.k{index}"), "v"), Ok(()));
    }
    assert_eq!(tool_span.set("error.type", "tool_error"), Ok(()));
    assert_eq!(span.omissions(), before);
    drop(span);
    body.finish();
    assert!(charged(&trace) <= limits.extension_bytes());

    let extensions = fixture.extensions();
    let flooded = named(&extensions, "flooded");
    let attributes = &flooded[0].1.attributes;
    let bytes: u64 = attributes
        .iter()
        .map(|attribute| attribute.key.len() as u64 + attribute.value.encoded_len())
        .sum();
    assert!(bytes <= limits.extension_bytes());
    assert!(attributes.len() as u32 <= limits.contribution_attributes(TraceSpanKind::Extension));
    let string = named(&extensions, "string");
    let TraceValue::Str(cut) = &string[0].1.attributes[0].value else {
        panic!("the string is kept");
    };
    assert!(cut.len() as u64 <= limits.attribute_value_bytes());
    assert!(named(&extensions, "json")[0].1.attributes.is_empty());
    let annotations = annotations.lock().unwrap().clone();
    assert_eq!(annotations.0.len(), 11);
    assert!(annotations.get("foo.bar").is_none());
}

/// L22(b): two concurrent calls of one registered tool keep their own
/// diagnostics: each body's omissions and its `body_omissions` record name
/// its own call, attempt and position, and the ledger holds their sum.
#[test]
fn two_bodies_of_one_callback_keep_their_own_omission_sites() {
    let fixture = Fixture::new(TraceLimits::standard());
    let owner = crate::ExecutionOwner::SessionFrame {
        session_id: crate::SessionId::from("extension-laws"),
        agent_frame_id: crate::FrameNodeId::new("frame").unwrap(),
    };
    let address = |call: &str| crate::CallAddress {
        owner: owner.clone(),
        call_id: crate::ToolCallId::fixture(call),
        attempt: 1,
    };
    let oversized = serde_json::json!({ "v": "x".repeat(64 * 1024) });
    let bodies = [("call-three", 3), ("call-five", 5)].map(|(call, refusals)| {
        let (trace, body) = fixture.body(Some(address(call)));
        let mut span = trace.span("oversized");
        for index in 0..refusals {
            span.attr(format!("body.json{index}"), oversized.clone());
        }
        span.end();
        (call, refusals, trace, body)
    });
    let mut sites = BTreeSet::new();
    for (call, refusals, trace, body) in bodies {
        let diagnostic = trace.omissions();
        assert_eq!(
            diagnostic.counts.get(TraceOmission::ValueTooLarge),
            refusals
        );
        let TraceOmissionSite::Body {
            call: Some(named),
            attempt: Some(1),
            at: Some(_),
            ..
        } = &diagnostic.site
        else {
            panic!("{diagnostic:?}");
        };
        assert_eq!(named, &crate::ToolCallId::fixture(call));
        sites.insert(format!("{:?}", diagnostic.site));
        body.finish();
    }
    assert_eq!(sites.len(), 2);
    let reported = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record.event {
            TraceEvent::BodyOmissions(diagnostic) => Some(diagnostic),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reported.len(), 2);
    assert_eq!(
        reported
            .iter()
            .map(|diagnostic| diagnostic.counts.get(TraceOmission::ValueTooLarge))
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([3, 5])
    );
    assert_eq!(
        fixture.ledger(TraceOmissionSiteKind::Body, TraceOmission::ValueTooLarge),
        8
    );
}

fn held_four(bytes: u64) -> TraceLimits {
    TraceLimits::builder()
        .publication_hold(4, bytes)
        .build()
        .unwrap()
}

/// L23(a): ten model calls under one extension span, past a hold of four:
/// every close is published naming the span, the span is published early
/// (`hold_pressure`) before the first close it could not hold, and the
/// body's later `end()` is counted late. Again with the byte bound at its
/// minimum and large responses.
#[test]
fn hold_pressure_finalizes_the_parent_early_and_drops_no_close() {
    for (limits, response) in [
        (
            held_four(TraceLimits::standard().publication_hold().bytes()),
            16,
        ),
        (held_four(128 * 1024), 40 * 1024),
    ] {
        let fixture = Fixture::new(limits);
        let (trace, body) = fixture.body(None);
        let span = trace.span("calls");
        for _ in 0..10 {
            chat(&span.trace(), response);
            assert!(fixture_account(&trace).records() <= 4);
        }
        span.end();
        body.finish();

        let records = fixture.records();
        let extensions = extensions(&records);
        let calls = named(&extensions, "calls");
        assert_eq!(calls.len(), 1);
        let (parent, record) = calls[0];
        assert_eq!(record.ended_by, Some(ExtensionSpanEnd::HoldPressure));
        let closes = records
            .iter()
            .filter(|record| is_chat_close(record))
            .collect::<Vec<_>>();
        assert_eq!(closes.len(), 10);
        let at = |target: &TraceRecord| records.iter().position(|record| record == target);
        let fifth = closes[4];
        assert!(at(parent) < at(fifth));
        for close in &closes {
            assert_eq!(parent_of(close), span_of(parent));
        }
        assert!(ended_at(parent) < ended_at(fifth));
        assert_eq!(
            fixture.ledger(TraceOmissionSiteKind::Body, TraceOmission::Late),
            1
        );
        assert_eq!(orphans(&fixture), Vec::<String>::new());
    }
}

/// The body's hold account now.
fn fixture_account(trace: &ExtensionTrace) -> HoldAccount {
    trace.inner.as_ref().unwrap().body.account()
}

/// L23(b): one account per body across overlapping parents: it never holds
/// more than four, though no parent holds more than two; the close that
/// overflows it finalizes its parent with the ancestor gating it, in
/// order, and the other parent publishes its held children when it ends.
#[test]
fn one_account_per_body_finalizes_through_ancestor_gates() {
    let fixture = Fixture::new(held_four(
        TraceLimits::standard().publication_hold().bytes(),
    ));
    let (trace, body) = fixture.body(None);
    let a = trace.span("a");
    let b = trace.span("b");
    let a1 = a.child("a1");
    let probe = || assert!(fixture_account(&trace).records() <= 4);
    chat(&a1.trace(), 16);
    probe();
    b.child("b1").end();
    probe();
    chat(&a1.trace(), 16);
    probe();
    b.child("b2").end();
    probe();
    chat(&a1.trace(), 16);
    probe();

    let order = |records: &[TraceRecord]| {
        records
            .iter()
            .filter_map(|record| match &record.event {
                TraceEvent::ExtensionSpan(span) => Some(span.name.clone()),
                TraceEvent::ContextPressure { .. } if is_chat_close(record) => {
                    Some("close".to_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        order(&fixture.records()),
        ["a", "a1", "close", "close", "close"]
    );
    b.end();
    assert_eq!(
        order(&fixture.records()),
        ["a", "a1", "close", "close", "close", "b", "b1", "b2"]
    );
    drop((a, a1));
    body.finish();
    assert_eq!(orphans(&fixture), Vec::<String>::new());
}

/// L23(c): a cell whose model response is still held keeps at most four
/// records in its account; the extension spans past it are shed whole,
/// each with its held child, and counted at the cell; once the response
/// commits, what was kept publishes in order.
#[test]
fn an_unanchored_cell_sheds_whole_extension_spans_past_its_hold() {
    let fixture = Fixture::new(held_four(
        TraceLimits::standard().publication_hold().bytes(),
    ));
    let held = crate::HeldResponse::holding(
        lash_durable::ActorTx::opened(lash_durable::OpenedActor {
            actor: lash_durable::ActorKey::session("s").unwrap(),
            epoch: lash_durable::Epoch(1),
            revision: lash_durable::StateRevision(0),
            acked: lash_durable::MailSeq(0),
            seen: lash_durable::MailSeq(0),
            mail: Vec::new(),
            at: lash_durable::DurableInstant(0),
            turn_cancel: None,
        }),
        |_| false,
        |_| false,
    );
    let mut spec = BodySpec::at(fixture.standing(), TraceContext::default());
    spec.callback = BoundCallback::of(&PluginCallbackIdentity {
        owner: "extension-laws".to_owned(),
        key: "code_executor".to_owned(),
    });
    spec.cell = Some((held.clone(), "cell-1".to_owned()));
    let (trace, body) = spec.bind();
    let account = || {
        held.trace_hold(TraceLimits::standard().publication_hold())
            .snapshot()
    };
    for index in 0..6 {
        let span = trace.span(format!("span-{index}"));
        span.child(format!("child-{index}")).end();
        assert!(account().records() <= 4);
        span.end();
        assert!(account().records() <= 4);
    }
    body.finish();
    assert!(
        fixture.extensions().is_empty(),
        "held until the response commits"
    );
    assert_eq!(
        fixture.ledger(TraceOmissionSiteKind::Cell, TraceOmission::PublicationHold),
        8
    );

    held.committed(lash_durable::CommitLabel::MODEL_DONE);
    let names = fixture
        .extensions()
        .into_iter()
        .map(|(_, span)| span.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["span-0", "child-0", "span-1", "child-1"]);
    assert_eq!(account().records(), 0);
    assert_eq!(orphans(&fixture), Vec::<String>::new());
}

/// A span spec's kind and operation reach the record.
#[test]
fn a_span_spec_reaches_the_record() {
    let fixture = Fixture::new(TraceLimits::standard());
    let (trace, body) = fixture.body(None);
    trace
        .span_with(SpanSpec::embeddings("openai", "text-embedding-3-small"))
        .end();
    body.finish();
    let extensions = fixture.extensions();
    assert_eq!(extensions[0].1.kind, lash_trace::ExtensionSpanKind::Client);
    assert!(extensions[0].1.operation.is_some());
}
