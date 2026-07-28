//! Golden schema pins for the on-disk trace format.
//!
//! Trace records are a durable, cross-tool contract (JSONL files consumed by
//! the trace viewer, exporters, and the OTel bridge). These tests pin the
//! `schema_version` tripwire, the `type` tag for every [`TraceEvent`] variant,
//! the full payload shape of the load-bearing variants, and a JSONL round-trip
//! carrying an `exec_code_completed` diagnostic.

use std::collections::BTreeSet;

use lash_trace::{
    TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceLashlangExecutionEvent,
    TraceLashlangExecutionIdentity, TraceLashlangStatus, TraceLlmRequest, TraceLlmResponse,
    TraceProviderRequestEvent, TraceProviderStreamEvent, TraceRecord, TraceRuntimeScope,
    TraceRuntimeStreamEvent, TraceRuntimeSubject, TraceTokenUsage, TraceToolCallOutcome,
    TraceToolCallOutput,
};
use serde_json::json;

#[test]
fn trace_schema_version_is_pinned_at_2() {
    // Tripwire. This is the current on-disk trace schema version. Every reader
    // (viewer, exporter, OTel bridge) keys off it, so a change here must be a
    // deliberate, documented schema bump — see the crate-level rustdoc and the
    // `TRACE_SCHEMA_VERSION` doc comment for the bump policy. If this fails,
    // read that policy before touching the constant.
    assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 2);
}

#[test]
fn new_records_stamp_the_schema_version() {
    let record = TraceRecord::new(
        TraceContext::default().for_session("root"),
        TraceEvent::SessionStarted {
            metadata: Default::default(),
        },
    );
    assert_eq!(record.schema_version, lash_trace::TRACE_SCHEMA_VERSION);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["schema_version"], 2);
}

fn token_usage_sample() -> TraceTokenUsage {
    TraceTokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 3,
    }
}

fn lashlang_identity() -> TraceLashlangExecutionIdentity {
    TraceLashlangExecutionIdentity {
        scope: TraceRuntimeScope::new("s1"),
        subject: TraceRuntimeSubject::Process {
            process_id: "p1".to_string(),
        },
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
        entry_name: "main".to_string(),
    }
}

/// One representative of every [`TraceEvent`] variant. Paired with
/// [`event_samples_cover_every_variant`], this fails until a new variant is
/// given a sample, and [`TraceEvent::kind`] (exhaustive in the crate) fails to
/// compile until the variant is given a tag.
fn event_samples() -> Vec<TraceEvent> {
    vec![
        TraceEvent::SessionStarted {
            metadata: Default::default(),
        },
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
        TraceEvent::PromptBuilt {
            prompt_hash: "h".to_string(),
            prompt_chars: 12,
            components: Vec::new(),
        },
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                model: "m".to_string(),
                model_variant: Default::default(),
                messages: Vec::new(),
                attachments: Vec::new(),
                tools: Vec::new(),
                tool_choice: "auto".to_string(),
                output_spec: None,
                stream: false,
            },
        },
        TraceEvent::LlmCallCompleted {
            response: TraceLlmResponse {
                text: "hello".to_string(),
                duration_ms: 12,
                terminal_reason: Some("stop".to_string()),
                parts: None,
                generation_disposition: None,
            },
            usage: Some(token_usage_sample()),
            provider_usage: None,
            stream_summary: None,
        },
        TraceEvent::LlmCallFailed {
            error: TraceError {
                message: "boom".to_string(),
                retryable: true,
                terminal_reason: None,
                code: None,
                raw: None,
            },
            stream_summary: None,
        },
        TraceEvent::ProviderRequest {
            event: TraceProviderRequestEvent {
                provider: "test".to_string(),
                sequence: 0,
                elapsed_ms: 0,
                endpoint: "chat/completions".to_string(),
                body_len: 13,
                body_sha256: "abcd".to_string(),
                body_json: Some(json!({ "model": "m" })),
                body_json_omitted_reason: None,
            },
        },
        TraceEvent::EffectEnvelopeDiff {
            event: TraceEffectEnvelopeDiffEvent {
                recorded_envelope_hash: "old".to_string(),
                reconstructed_envelope_hash: "new".to_string(),
                divergent_paths: vec![TraceEffectEnvelopeDiffEntry {
                    path: "command.input.value".to_string(),
                    recorded: TraceEffectEnvelopeDiffValue::Present {
                        json_len: 1,
                        json_sha256: "one".to_string(),
                        value_json: Some(json!(1)),
                        value_json_omitted_reason: None,
                    },
                    reconstructed: TraceEffectEnvelopeDiffValue::Missing,
                }],
            },
        },
        TraceEvent::ProviderStreamEvent {
            event: TraceProviderStreamEvent {
                provider: "test".to_string(),
                sequence: 1,
                elapsed_ms: 0,
                event_name: "delta".to_string(),
                item_id: None,
                output_index: None,
                raw_len: 4,
                raw_sha256: "abcd".to_string(),
                raw_json: None,
            },
        },
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 1,
                elapsed_ms: 0,
                event_name: "delta".to_string(),
                raw_text: None,
                visible_text: None,
                item_id: None,
                output_index: None,
                call_id: None,
                tool_name: None,
                input_json: None,
                usage: None,
            },
        },
        TraceEvent::ToolCallStarted {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            args: json!({ "path": "README.md" }),
        },
        TraceEvent::ToolCallCompleted {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            args: json!({ "path": "README.md" }),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Success(json!("ok")),
                control: None,
            },
            duration_ms: 3,
        },
        TraceEvent::ProtocolStep {
            plugin_id: "custom".to_string(),
            payload: json!({ "code": "print 1" }),
        },
        TraceEvent::TokenUsage {
            usage: token_usage_sample(),
            cumulative: Some(token_usage_sample()),
        },
        TraceEvent::LashlangExecution {
            event: TraceLashlangExecutionEvent::ExecutionFinished {
                event_key: "process:p1:finished".to_string(),
                identity: lashlang_identity(),
                status: TraceLashlangStatus::Completed,
                error: None,
            },
        },
        TraceEvent::TurnCompleted {
            status: "completed".to_string(),
            done_reason: "modelstop".to_string(),
            agent_frame_switch: None,
        },
        TraceEvent::Custom {
            name: "x.event".to_string(),
            payload: json!({ "ok": true }),
        },
    ]
}

const ALL_TRACE_EVENT_KINDS: &[&str] = &[
    "session_started",
    "turn_started",
    "prompt_built",
    "llm_call_started",
    "llm_call_completed",
    "llm_call_failed",
    "provider_request",
    "effect_envelope_diff",
    "provider_stream_event",
    "runtime_stream_event",
    "tool_call_started",
    "tool_call_completed",
    "protocol_step",
    "token_usage",
    "lashlang_execution",
    "turn_completed",
    "custom",
];

#[test]
fn every_event_type_tag_matches_kind() {
    for event in event_samples() {
        let kind = event.kind();
        let json = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(
            json["type"], kind,
            "serialized `type` disagrees with TraceEvent::kind() for `{kind}`"
        );
    }
}

#[test]
fn event_samples_cover_every_variant() {
    let sampled: BTreeSet<&str> = event_samples().iter().map(TraceEvent::kind).collect();
    let canonical: BTreeSet<&str> = ALL_TRACE_EVENT_KINDS.iter().copied().collect();
    assert_eq!(
        sampled, canonical,
        "event_samples must pin exactly one representative per TraceEvent variant"
    );
}

#[test]
fn tool_call_started_full_shape() {
    let event = TraceEvent::ToolCallStarted {
        call_id: Some("call-1".to_string()),
        name: "read_file".to_string(),
        args: json!({ "path": "README.md" }),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "tool_call_started",
            "call_id": "call-1",
            "name": "read_file",
            "args": { "path": "README.md" },
        })
    );
}

#[test]
fn tool_call_completed_pins_outcome_vocabulary() {
    let cases = [
        (TraceToolCallOutcome::Success(json!("ok")), "success"),
        (
            TraceToolCallOutcome::Failure(json!({ "code": "boom" })),
            "failure",
        ),
        (TraceToolCallOutcome::Cancelled(json!(null)), "cancelled"),
    ];
    for (outcome, status) in cases {
        let payload = outcome.clone();
        let event = TraceEvent::ToolCallCompleted {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            args: json!({ "path": "x" }),
            output: TraceToolCallOutput {
                outcome,
                control: None,
            },
            duration_ms: 3,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call_completed");
        assert_eq!(
            json["output"]["outcome"]["status"], status,
            "outcome status vocabulary drifted for {payload:?}"
        );
        // The `content = "payload"` tagging keeps the value under `payload`.
        assert!(json["output"]["outcome"].get("payload").is_some());
    }
}

#[test]
fn llm_call_completed_full_shape() {
    let event = TraceEvent::LlmCallCompleted {
        response: TraceLlmResponse {
            text: "hello".to_string(),
            duration_ms: 12,
            terminal_reason: Some("stop".to_string()),
            parts: None,
            generation_disposition: None,
        },
        usage: Some(token_usage_sample()),
        provider_usage: None,
        stream_summary: None,
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "llm_call_completed",
            "response": {
                "text": "hello",
                "duration_ms": 12,
                "terminal_reason": "stop",
            },
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 1,
                "cache_write_input_tokens": 2,
                "reasoning_output_tokens": 3,
            },
        })
    );
}

#[test]
fn protocol_step_exec_diagnostic_full_shape() {
    // Mirrors the runtime's `exec_code_completed` diagnostic: a `runtime`
    // ProtocolStep whose payload nests `diagnostic.{phase,payload}` with the
    // per-call `tool_calls` array.
    let event = exec_code_completed_protocol_step();
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "protocol_step");
    assert_eq!(json["plugin_id"], "runtime");
    assert_eq!(
        json["payload"]["diagnostic"]["phase"],
        "exec_code_completed"
    );

    let payload = &json["payload"]["diagnostic"]["payload"];
    assert_eq!(payload["tool_call_count"], 1);
    assert_eq!(
        payload["tool_calls"],
        json!([{
            "call_id": "call-1",
            "name": "read_file",
            "duration_ms": 5,
            "status": "success",
        }])
    );
}

#[test]
fn lashlang_execution_full_shape() {
    let event = TraceEvent::LashlangExecution {
        event: TraceLashlangExecutionEvent::ExecutionFinished {
            event_key: "process:p1:finished".to_string(),
            identity: lashlang_identity(),
            status: TraceLashlangStatus::Completed,
            error: None,
        },
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "lashlang_execution",
            "event": {
                "kind": "execution_finished",
                "event_key": "process:p1:finished",
                "identity": {
                    "scope": { "session_id": "s1" },
                    "subject": { "type": "process", "process_id": "p1" },
                    "module_ref": "module",
                    "entry_kind": "process",
                    "entry_ref": "component:0",
                    "entry_name": "main",
                },
                "status": "completed",
            },
        })
    );
}

#[test]
fn custom_full_shape() {
    let event = TraceEvent::Custom {
        name: "x.event".to_string(),
        payload: json!({ "ok": true }),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({ "type": "custom", "name": "x.event", "payload": { "ok": true } })
    );
}

fn exec_code_completed_protocol_step() -> TraceEvent {
    TraceEvent::ProtocolStep {
        plugin_id: "runtime".to_string(),
        payload: json!({
            "diagnostic": {
                "phase": "exec_code_completed",
                "payload": {
                    "duration_ms": 12,
                    "output": "hello\nworld",
                    "output_chars": 11,
                    "observation_count": 2,
                    "observation_truncation": [],
                    "error": null,
                    "terminal_finish": null,
                    "terminal_finish_present": false,
                    "tool_call_count": 1,
                    "tool_calls": [{
                        "call_id": "call-1",
                        "name": "read_file",
                        "duration_ms": 5,
                        "status": "success",
                    }],
                },
            },
        }),
    }
}

#[test]
fn jsonl_round_trip_preserves_records() {
    let records = vec![
        TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::SessionStarted {
                metadata: Default::default(),
            },
        ),
        TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::ToolCallStarted {
                call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "README.md" }),
            },
        ),
        TraceRecord::new(
            TraceContext::default().for_session("root"),
            exec_code_completed_protocol_step(),
        ),
        TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::TurnCompleted {
                status: "completed".to_string(),
                done_reason: "modelstop".to_string(),
                agent_frame_switch: None,
            },
        ),
    ];

    // Serialize to JSONL exactly as a sink would (one compact record per line).
    let jsonl = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");

    let parsed: Vec<TraceRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse trace record line"))
        .collect();

    assert_eq!(parsed, records, "JSONL round-trip must preserve records");
    for record in &parsed {
        assert_eq!(record.schema_version, 2);
    }

    // Pin the diagnostic's `tool_calls` entry fields explicitly on the parsed
    // line, independent of the Rust construction above.
    let diagnostic_line = jsonl
        .lines()
        .find(|line| line.contains("exec_code_completed"))
        .expect("exec diagnostic line present");
    let value: serde_json::Value =
        serde_json::from_str(diagnostic_line).expect("parse diagnostic line");
    let tool_call = &value["payload"]["diagnostic"]["payload"]["tool_calls"][0];
    assert_eq!(tool_call["call_id"], "call-1");
    assert_eq!(tool_call["name"], "read_file");
    assert_eq!(tool_call["duration_ms"], 5);
    assert_eq!(tool_call["status"], "success");
}
