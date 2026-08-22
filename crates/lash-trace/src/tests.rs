//! Unit tests for the trace record and event carriers.
//!
//! These live in their own file so `lib.rs` carries only the durable schema
//! types it defines.

use super::*;

/// A sink that fails every call, standing in for a closed stderr.
struct FailingSink;

impl TraceSink for FailingSink {
    fn append(&self, _record: &TraceRecord) -> Result<(), TraceSinkError> {
        Err(TraceSinkError::Write {
            path: PathBuf::from("<failing>"),
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        })
    }

    fn flush(&self) -> Result<(), TraceSinkError> {
        Err(TraceSinkError::Write {
            path: PathBuf::from("<failing>"),
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        })
    }
}

#[test]
fn a_failing_sink_does_not_rob_later_sinks_in_a_tee() {
    // The bot tees stderr first and its durable JSONL file second, so a
    // supervisor closing stderr must not cost the run its trace file.
    let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trace.jsonl");
    let tee = TeeTraceSink::new([
        Arc::new(FailingSink) as Arc<dyn TraceSink>,
        Arc::new(JsonlTraceSink::new(&path)),
    ]);

    let append = tee.append(&TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::Custom {
            name: "test.event".to_string(),
            payload: serde_json::json!({"ok": true}),
        },
    ));

    assert!(
        append.is_err(),
        "the first sink's failure is still reported"
    );
    assert!(tee.flush().is_err(), "a failing flush is reported too");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"type\":\"custom\""),
        "the later sink still received the record"
    );
}

#[test]
fn jsonl_sink_writes_record() {
    let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trace.jsonl");
    let sink = JsonlTraceSink::new(&path);
    sink.append(&TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::Custom {
            name: "test.event".to_string(),
            payload: serde_json::json!({"ok": true}),
        },
    ))
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"type\":\"custom\""));
    assert!(text.contains("\"session_id\":\"root\""));
}

#[test]
fn tool_start_and_frame_switch_records_are_jsonl_shaped() {
    let started = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::ToolCallStarted {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "README.md"}),
        },
    );
    let completed = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::AgentFrameSwitch {
                frame_switch: TraceAgentFrameSwitch {
                    frame_key: "frame-key/v1/example".to_string(),
                },
            },
        },
    );

    let started_json = serde_json::to_value(started).unwrap();
    assert_eq!(started_json["type"], "tool_call_started");
    assert_eq!(started_json["call_id"], "call-1");

    let completed_json = serde_json::to_value(completed).unwrap();
    assert_eq!(completed_json["type"], "turn_completed");
    assert_eq!(completed_json["outcome"]["status"], "agent_frame_switch");
    assert_eq!(
        completed_json["outcome"]["frame_switch"]["frame_key"],
        "frame-key/v1/example"
    );
}

#[test]
fn language_execution_records_are_jsonl_shaped() {
    let identity = TraceLanguageExecutionIdentity {
        scope: TraceRuntimeScope::new("s1"),
        subject: TraceRuntimeSubject::Process {
            process_id: "p1".to_string(),
        },
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
        entry_name: "main".to_string(),
    };
    let event = TraceLanguageExecution {
        event_key: "process:p1:node:n1:1:started".to_string(),
        identity,
        payload: TraceLanguageExecutionPayload::NodeStarted {
            node_id: "n1".to_string(),
            node_kind: "resource_operation".to_string(),
            label: "read_file".to_string(),
            occurrence: 1,
        },
    };
    let record = TraceRecord::new(
        TraceContext::default().for_session("s1"),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event,
        },
    );

    let json = serde_json::to_value(&record).expect("serialize language execution");
    assert_eq!(json["type"], "language_execution");
    assert_eq!(json["language"], "lashlang");
    assert_eq!(json["event"]["kind"], "node_started");
    assert_eq!(json["event"]["event_key"], "process:p1:node:n1:1:started");

    let round_trip =
        serde_json::from_value::<TraceRecord>(json).expect("deserialize language execution");
    assert!(matches!(
        round_trip.event,
        TraceEvent::LanguageExecution {
            language,
            event: TraceLanguageExecution {
                payload: TraceLanguageExecutionPayload::NodeStarted { .. },
                ..
            }
        } if language == "lashlang"
    ));
}

#[test]
fn tool_completion_serializes_typed_failure_output() {
    let record = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::ToolCallCompleted {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "missing"}),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Failure(serde_json::json!({
                    "class": "invalid_request",
                    "code": "invalid_tool_args",
                    "message": "bad args",
                    "source": "runtime",
                    "retry": { "type": "never" },
                    "raw": { "path": "missing" }
                })),
                control: None,
            },
            duration_ms: 3,
            attempts: None,
        },
    );

    let json = serde_json::to_value(record).unwrap();
    assert_eq!(json["type"], "tool_call_completed");
    assert_eq!(json["output"]["outcome"]["status"], "failure");
    assert_eq!(
        json["output"]["outcome"]["payload"]["code"],
        "invalid_tool_args"
    );
    assert_eq!(
        json["output"]["outcome"]["payload"]["raw"]["path"],
        "missing"
    );
}

#[test]
fn event_kind_matches_serialized_type_tag() {
    let events = [
        TraceEvent::SessionStarted {
            metadata: Default::default(),
        },
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
        TraceEvent::ToolCallStarted {
            call_id: None,
            name: "read_file".to_string(),
            args: Value::Null,
        },
        TraceEvent::Custom {
            name: "x".to_string(),
            payload: Value::Null,
        },
    ];
    for event in events {
        let kind = event.kind();
        let json = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(json["type"], kind, "kind() disagrees with serde tag");
    }
}

#[test]
fn event_is_failed_identifies_all_failure_outcomes() {
    let ok_turn = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
        },
    );
    let failed_turn = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::Failed {
                done_reason: TraceTurnFailureReason::RuntimeError,
            },
        },
    );
    assert!(!ok_turn.event.is_failed());
    assert!(failed_turn.event.is_failed());

    let lang_started = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "key1".to_string(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p1".to_string(),
                },
                module_ref: "m".to_string(),
                entry_kind: "p".to_string(),
                entry_ref: None,
                entry_name: "main".to_string(),
            },
            payload: TraceLanguageExecutionPayload::NodeStarted {
                node_id: "n1".to_string(),
                node_kind: "op".to_string(),
                label: "lbl".to_string(),
                occurrence: 1,
            },
        },
    };
    let lang_node_failed = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "key2".to_string(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p1".to_string(),
                },
                module_ref: "m".to_string(),
                entry_kind: "p".to_string(),
                entry_ref: None,
                entry_name: "main".to_string(),
            },
            payload: TraceLanguageExecutionPayload::NodeFailed {
                node_id: "n1".to_string(),
                node_kind: "op".to_string(),
                label: "lbl".to_string(),
                occurrence: 1,
                error: "err".to_string(),
            },
        },
    };
    let lang_exec_failed = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "key3".to_string(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p1".to_string(),
                },
                module_ref: "m".to_string(),
                entry_kind: "p".to_string(),
                entry_ref: None,
                entry_name: "main".to_string(),
            },
            payload: TraceLanguageExecutionPayload::ExecutionFinished {
                status: TraceLanguageExecutionStatus::Failed,
                error: Some("err".to_string()),
            },
        },
    };
    let lang_exec_ok = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "key4".to_string(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p1".to_string(),
                },
                module_ref: "m".to_string(),
                entry_kind: "p".to_string(),
                entry_ref: None,
                entry_name: "main".to_string(),
            },
            payload: TraceLanguageExecutionPayload::ExecutionFinished {
                status: TraceLanguageExecutionStatus::Completed,
                error: None,
            },
        },
    };
    assert!(!lang_started.is_failed());
    assert!(lang_node_failed.is_failed());
    assert!(lang_exec_failed.is_failed());
    assert!(!lang_exec_ok.is_failed());
}

#[test]
fn jsonl_sink_creates_parent_directories() {
    let dir = std::env::temp_dir().join(format!("lash-trace-{}", uuid::Uuid::new_v4()));
    let path = dir.join("nested").join("trace.jsonl");
    let sink = JsonlTraceSink::new(&path);
    sink.append(&TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 1,
                elapsed_ms: 0,
                event_name: "delta".to_string(),
                raw_text: Some("hello".to_string()),
                visible_text: Some("hello".to_string()),
                item_id: None,
                output_index: None,
                call_id: None,
                tool_name: None,
                input_json: None,
                usage: None,
            },
        },
    ))
    .unwrap();
    assert!(path.exists());
    let _ = std::fs::remove_dir_all(dir);
}
