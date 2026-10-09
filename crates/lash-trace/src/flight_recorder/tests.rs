use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::{TelemetryContent, TraceContext, TraceToolCallOutcome, TraceToolCallOutput};
use serde_json::json;

#[derive(Clone, Default)]
struct InnerSink {
    records: Arc<Mutex<Vec<TraceRecord>>>,
    fail: Arc<AtomicBool>,
    flushes: Arc<AtomicUsize>,
}

impl TraceSink for InnerSink {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.records.lock_recover().push(record.clone());
        self.result()
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        self.result()
    }
}

impl InnerSink {
    fn result(&self) -> Result<(), TraceSinkError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(TraceSinkError::Write {
                path: "<test>".into(),
                source: std::io::Error::other("inner sink failed"),
            })
        } else {
            Ok(())
        }
    }
}

fn recorder(
    settings: FlightRecorderSettings,
) -> (
    FlightRecorderSink<InnerSink>,
    Arc<Mutex<Vec<FlightRecorderSnapshot>>>,
    InnerSink,
) {
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let received = snapshots.clone();
    let inner = InnerSink::default();
    (
        FlightRecorderSink::new(inner.clone(), settings, move |snapshot| {
            received.lock_recover().push(snapshot);
        }),
        snapshots,
        inner,
    )
}

fn record(id: &str, timestamp_ms: i64, event: TraceEvent) -> TraceRecord {
    TraceRecord {
        schema_version: crate::TRACE_SCHEMA_VERSION,
        id: id.into(),
        timestamp: DateTime::from_timestamp_millis(timestamp_ms).unwrap(),
        content: TelemetryContent::Captured,
        context: TraceContext::default()
            .for_session("session")
            .for_turn("turn"),
        event,
    }
}

fn call_id() -> ToolCallId {
    ToolCallId::derive("", lash_sansio::ToolCallRoot::turn("turn").unwrap(), &[])
}

fn tool(outcome: TraceToolCallOutcome, duration_ms: u64) -> TraceEvent {
    TraceEvent::ToolCallCompleted {
        call_id: call_id(),
        provider_call_id: Some("provider-call".into()),
        name: "tool".into(),
        args: json!({"input": "PAYLOAD_SECRET"}),
        output: TraceToolCallOutput {
            outcome,
            control: Some(json!("PAYLOAD_SECRET")),
        },
        duration_ms,
        issuing_node_id: None,
        attempts: None,
    }
}

fn failure() -> TraceEvent {
    TraceEvent::ExecCodeFailed {
        reason: crate::ExecCodeFailureReason::Session,
        error: "PAYLOAD_SECRET".into(),
    }
}

#[test]
fn slow_record_snapshots_bounded_preceding_records_in_order() {
    let (sink, snapshots, _) = recorder(FlightRecorderSettings {
        capacity: NonZeroUsize::new(3).unwrap(),
        minimum_interval: Duration::ZERO,
        ..FlightRecorderSettings::default()
    });
    // The threshold is strict: an operation at five seconds is not slow.
    for id in ["evicted-1", "evicted-2", "preceding-1", "preceding-2"] {
        sink.append(&record(
            id,
            1000,
            tool(TraceToolCallOutcome::Success(json!(null)), 5000),
        ))
        .unwrap();
    }
    sink.append(&record(
        "slow",
        2000,
        tool(TraceToolCallOutcome::Success(json!(null)), 5001),
    ))
    .unwrap();
    {
        let snapshots = snapshots.lock_recover();
        assert_eq!(snapshots.len(), 1); // Duration alone is enough to trigger.
        let snapshot = &snapshots[0];
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|record| record.record_id.as_str())
                .collect::<Vec<_>>(),
            ["preceding-1", "preceding-2", "slow"]
        );
        assert_eq!(snapshot.evicted_records, 2);
        assert!(snapshot.slow && !snapshot.failed);
        let trigger = &snapshot.records[2];
        assert_eq!(trigger.duration, Some(Duration::from_millis(5001)));
        assert_eq!(trigger.tool_call_id, Some(call_id()));
        assert_eq!(trigger.outcome, Some(FlightRecorderOutcome::Completed));
    }
    sink.append(&record(
        "next",
        3000,
        tool(TraceToolCallOutcome::Failure(json!(null)), 5001),
    ))
    .unwrap();
    let snapshots = snapshots.lock_recover();
    assert_eq!(snapshots.len(), 2); // Both triggers still produce one snapshot.
    assert_eq!(snapshots[1].evicted_records, 1);
    assert!(snapshots[1].slow && snapshots[1].failed);
}

#[test]
fn failed_operation_triggers_a_snapshot() {
    let (sink, snapshots, _) = recorder(FlightRecorderSettings {
        minimum_interval: Duration::ZERO,
        ..FlightRecorderSettings::default()
    });
    let failed_events = [
        failure(),
        tool(TraceToolCallOutcome::Failure(json!("PAYLOAD_SECRET")), 0),
        TraceEvent::ProgramStep {
            step_index: 1,
            outcome: TraceProgramStepOutcome::Failure {
                diagnostic: "PAYLOAD_SECRET".into(),
            },
        },
        TraceEvent::DomainCompleted {
            completion: crate::TraceDomainCompletion::new(
                crate::TraceDomainOperation::Process,
                1000,
                TraceDomainStatus::Failed,
            ),
        },
    ];
    for (index, event) in failed_events.into_iter().enumerate() {
        sink.append(&record(&index.to_string(), 1000 + index as i64, event))
            .unwrap();
    }
    let snapshots = snapshots.lock_recover();
    assert_eq!(snapshots.len(), 4);
    for snapshot in snapshots.iter() {
        assert!(snapshot.failed);
        assert!(!snapshot.slow);
        assert_eq!(
            snapshot.records.last().unwrap().outcome,
            Some(FlightRecorderOutcome::Failed)
        );
    }
}

#[test]
fn minimum_interval_suppresses_triggers_without_losing_recent_context() {
    let (sink, snapshots, _) = recorder(FlightRecorderSettings {
        capacity: NonZeroUsize::new(2).unwrap(),
        minimum_interval: Duration::from_secs(1),
        ..FlightRecorderSettings::default()
    });
    for (id, timestamp) in [("first", 1000), ("suppressed", 1999), ("older", 999)] {
        sink.append(&record(id, timestamp, failure())).unwrap();
    }
    assert_eq!(snapshots.lock_recover().len(), 1);
    sink.append(&record("boundary", 2000, failure())).unwrap();
    let snapshots = snapshots.lock_recover();
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[1].evicted_records, 2);
    assert_eq!(snapshots[1].records[0].record_id, "older");
    assert_eq!(snapshots[1].records[1].record_id, "boundary");
}

#[test]
fn snapshots_never_contain_payload_even_with_full_content() {
    let (sink, snapshots, _) = recorder(FlightRecorderSettings::default());
    let events = [
        TraceEvent::Custom {
            name: "PAYLOAD_SECRET".into(),
            payload: json!("PAYLOAD_SECRET"),
        },
        TraceEvent::ProtocolStep {
            plugin_id: "plugin".into(),
            payload: json!("PAYLOAD_SECRET"),
        },
        TraceEvent::CompositionChanged {
            fingerprint: "fingerprint".into(),
            rendered_system_prompt: "PAYLOAD_SECRET".into(),
            tool_schemas: vec![],
        },
        tool(TraceToolCallOutcome::Failure(json!("PAYLOAD_SECRET")), 100),
    ];
    for (index, event) in events.into_iter().enumerate() {
        let mut record = record(&index.to_string(), 1000, event);
        record
            .context
            .metadata
            .insert("secret".into(), json!("PAYLOAD_SECRET"));
        assert!(format!("{record:?}").contains("PAYLOAD_SECRET"));
        sink.append(&record).unwrap();
    }
    let snapshots = snapshots.lock_recover();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].records.len(), 4);
    assert!(!format!("{:?}", snapshots[0]).contains("PAYLOAD_SECRET"));
    for metadata in &snapshots[0].records {
        assert_eq!(metadata.session_id, Some("session".into()));
        assert_eq!(metadata.turn_id, Some("turn".into()));
    }
}

#[test]
fn inner_sink_receives_every_record_unchanged_and_keeps_its_errors() {
    let (sink, snapshots, inner) = recorder(FlightRecorderSettings::default());
    let records = [
        record(
            "success",
            1000,
            tool(TraceToolCallOutcome::Success(json!("PAYLOAD_SECRET")), 10),
        ),
        record("failure", 1001, failure()),
        record(
            "cancelled",
            1002,
            tool(TraceToolCallOutcome::Cancelled(json!("PAYLOAD_SECRET")), 10),
        ),
    ];
    sink.append(&records[0]).unwrap();
    inner.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        sink.append(&records[1]),
        Err(TraceSinkError::Write { .. })
    ));
    assert_eq!(snapshots.lock_recover().len(), 1);
    inner.fail.store(false, Ordering::SeqCst);
    sink.append(&records[2]).unwrap();
    assert_eq!(*inner.records.lock_recover(), records);
    sink.flush().unwrap();
    inner.fail.store(true, Ordering::SeqCst);
    assert!(matches!(sink.flush(), Err(TraceSinkError::Write { .. })));
    assert_eq!(inner.flushes.load(Ordering::SeqCst), 2);
}
