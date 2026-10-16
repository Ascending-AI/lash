use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};

struct FailingSink(AtomicBool);
impl TraceSink for FailingSink {
    fn append(&self, _: &TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        if self.0.load(Ordering::SeqCst) {
            Err(lash_trace::TraceSinkError::Write {
                path: "injected-sink".into(),
                source: std::io::Error::other("injected append failure"),
            })
        } else {
            Ok(())
        }
    }
}

/// FIG-5531: a sink failure episode has one warning and a counted recovery;
/// another sink and cloned runtime keep their own delivery contracts.
#[test]
fn repeated_sink_failures_report_transitions_and_count() {
    let failed = Arc::new(FailingSink(AtomicBool::new(true)));
    let healthy = Arc::new(FailingSink(AtomicBool::new(false)));
    let runtime = TraceRuntime::default().with_trace_sink(failed.clone());
    let other = runtime.clone().with_trace_sink(healthy);
    let record = TraceRecord {
        schema_version: lash_trace::TRACE_SCHEMA_VERSION,
        id: "sink-law".into(),
        timestamp: TraceTimestamp::from_unix_ms(0),
        content: ContentPolicy::captured().into(),
        context: TraceContext::default(),
        position: None,
        omissions: Default::default(),
        event: TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    };
    let (_, capture) = crate::testing::trace_capture::capturing_sync(|| {
        runtime.emitter().append(&record);
        other.emitter().append(&record);
        runtime.emitter().append(&record);
        failed.0.store(false, Ordering::SeqCst);
        other.emitter().append(&record);
    });
    assert_eq!(capture.exactly_one("trace_sink.degraded").level, "WARN");
    assert_eq!(
        capture
            .exactly_one("trace_sink.recovered")
            .field("failure_count"),
        "3"
    );
}

struct Counting(Arc<AtomicUsize>);
impl TraceSink for Counting {
    fn append(&self, _: &TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Panicking;
impl TraceSink for Panicking {
    fn append(&self, _: &TraceRecord) -> Result<(), lash_trace::TraceSinkError> {
        panic!("the sink panics")
    }
}

/// L1(g): sink acceptance is per sink. A sink that panics, and a flight
/// recorder whose snapshot callback panics, each cost only their own
/// delivery: the sink after them sees every record, and each panic
/// counts against its own sink alone.
#[test]
fn a_panicking_sink_or_snapshot_callback_costs_only_its_own_delivery() {
    let forwarded = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let recorder = lash_trace::FlightRecorderSink::new(
        Counting(Arc::clone(&forwarded)),
        lash_trace::FlightRecorderSettings::new(
            256,
            Some(std::time::Duration::from_millis(1)),
            std::time::Duration::ZERO,
            true,
        )
        .expect("valid recorder settings"),
        |_snapshot| panic!("the snapshot callback panics"),
    );
    let runtime = TraceRuntime::default()
        .with_trace_sink(Arc::new(Panicking))
        .with_trace_sink(Arc::new(recorder))
        .with_trace_sink(Arc::new(Counting(Arc::clone(&after))));
    let record = TraceRecord {
        schema_version: lash_trace::TRACE_SCHEMA_VERSION,
        id: "containment-law".into(),
        timestamp: TraceTimestamp::from_unix_ms(0),
        content: ContentPolicy::captured().into(),
        context: TraceContext::default(),
        position: None,
        omissions: Default::default(),
        event: TraceEvent::ToolCallCompleted {
            call_id: crate::ToolCallId::fixture("call"),
            provider_call_id: None,
            name: "slow".into(),
            args: serde_json::Value::Null.into(),
            output: lash_trace::TraceToolCallOutput {
                outcome: lash_trace::TraceToolCallOutcome::Success(serde_json::Value::Null),
                control: None,
            },
            duration_ms: 10,
            issuing_node_id: None,
            attempts: None,
            span_attributes: Default::default(),
        },
    };
    for _ in 0..3 {
        runtime.emitter().append(&record);
    }
    assert_eq!(
        after.load(Ordering::SeqCst),
        3,
        "the last sink sees every record"
    );
    assert_eq!(
        forwarded.load(Ordering::SeqCst),
        3,
        "the recorder forwards each"
    );
    assert_eq!(runtime.emitter().sink_failures(), vec![3, 3, 0]);
}

/// L24: selection happens before builders, is per session and fails closed.
#[test]
fn capture_selector_narrows_before_building_and_panics_omit_once_per_record() {
    struct Select;
    impl lash_trace::CaptureSelector for Select {
        fn select(
            &self,
            at: &lash_trace::SelectorView<'_>,
            narrow: &mut lash_trace::TraceNarrowing,
        ) {
            match at.ids.session.as_ref().map(|s| s.as_str()) {
                Some("S1") => narrow.omit(lash_trace::ContentCategory::InputMessages),
                Some("S2") => narrow.skip(TraceSpanKind::ModelAttempt).unwrap(),
                Some("S3") => panic!("selector failed"),
                _ => {}
            }
        }
    }
    let old = crate::panic_containment::set_loud(false);
    let sink = Arc::new(lash_trace::RecordingTraceSink::default());
    let runtime = TraceRuntime::default()
        .with_content(ContentPolicy::captured())
        .with_capture_selector(Arc::new(Select))
        .with_trace_sink(sink.clone());
    let mut built = Vec::new();
    for name in ["S1", "S2", "S3", "S4"] {
        let context = TraceContext::default().for_session(name);
        let kind = if name == "S2" {
            TraceSpanKind::ModelAttempt
        } else {
            TraceSpanKind::Model
        };
        let standing = runtime.unreplayed(None).selected(kind, &context.ids());
        let content = standing.content();
        standing.observe(|| {
            (
                context,
                TraceEvent::ProtocolStep {
                    plugin_id: "fixture".into(),
                    payload: content.capture(lash_trace::ContentCategory::InputMessages, || {
                        built.push(name);
                        serde_json::json!({"text":name})
                    }),
                },
            )
        });
    }
    crate::panic_containment::set_loud(old);
    assert_eq!(built, vec!["S4"]);
    let records = sink.records();
    assert_eq!(records.len(), 3);
    let failed = records
        .iter()
        .find(|r| {
            r.context
                .session_id
                .as_ref()
                .is_some_and(|s| s.as_str() == "S3")
        })
        .unwrap();
    assert!(failed.content.captured.is_empty());
    assert_eq!(
        failed
            .omissions
            .get(lash_trace::TraceOmission::SelectorFailed),
        1
    );
    assert_eq!(
        runtime.omissions().report().get(
            lash_trace::TraceOmissionSiteKind::Record,
            lash_trace::TraceOmission::SelectorFailed
        ),
        1
    );
}
