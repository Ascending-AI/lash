//! Golden JSON schema pins for the app-facing [`TurnEvent`] surface and its
//! [`TurnActivity`] envelope.
//!
//! `TurnEvent` is a public streaming DTO consumed across process and protocol
//! boundaries, so its serde shape — tag string, field names, and
//! absent-when-`None` optional fields — is a contract. These tests assert the
//! exact `serde_json::Value` for every variant.
//!
//! Drift guard: [`expected_type_tag`] is an exhaustive `match` with no
//! wildcard, so adding a new `TurnEvent` variant fails to compile here until it
//! is given a tag. [`samples_cover_every_variant`] then fails until a
//! representative sample for the new variant is added to [`sample_events`], so
//! the new shape actually gets pinned rather than silently skipped.

use std::collections::BTreeSet;

use lash_core::runtime::QueuedWorkClaimBoundary;
use lash_core::{
    AttemptOutcome, AttemptRecord, CheckpointKind, LlmCallId, LlmCallRecord, MessageOrigin,
    MessageRole, PluginMessage, PluginRuntimeEvent, ProtocolPosition, TokenUsage, ToolCallOutput,
    ToolFailure, ToolFailureClass, ToolIntentExecutionOutcome, ToolIntentIdentity,
    ToolIntentKind, TurnActivity, TurnActivityId, TurnCause, TurnEvent, TurnInputApplication,
};
use serde_json::json;

/// The `type` tag serde writes for this variant. Exhaustive on purpose: a new
/// variant fails to compile until it is mapped, forcing the author here.
fn expected_type_tag(event: &TurnEvent) -> &'static str {
    match event {
        TurnEvent::QueuedWorkStarted { .. } => "queued_work_started",
        TurnEvent::ModelRequestStarted { .. } => "model_request_started",
        TurnEvent::AssistantProseDelta { .. } => "assistant_prose_delta",
        TurnEvent::ReasoningDelta { .. } => "reasoning_delta",
        TurnEvent::ModelAttemptReset { .. } => "model_attempt_reset",
        TurnEvent::ModelCallRecorded { .. } => "model_call_recorded",
        TurnEvent::CodeBlockStarted { .. } => "code_block_started",
        TurnEvent::CodeBlockCompleted { .. } => "code_block_completed",
        TurnEvent::ToolCallStarted { .. } => "tool_call_started",
        TurnEvent::ToolCallCompleted { .. } => "tool_call_completed",
        TurnEvent::ToolIntentOutcome { .. } => "tool_intent_outcome",
        TurnEvent::FinalValue { .. } => "final_value",
        TurnEvent::ToolValue { .. } => "tool_value",
        TurnEvent::Usage { .. } => "usage",
        TurnEvent::ChildUsage { .. } => "child_usage",
        TurnEvent::RetryStatus { .. } => "retry_status",
        TurnEvent::PluginRuntime { .. } => "plugin_runtime",
        TurnEvent::QueuedInputAccepted { .. } => "queued_input_accepted",
        TurnEvent::QueuedMessagesCommitted { .. } => "queued_messages_committed",
        TurnEvent::Error { .. } => "error",
    }
}

/// Canonical list of every serialized tag. Paired with [`samples_cover_every_variant`],
/// this catches a variant that gained a tag mapping but no pinned sample.
const ALL_TURN_EVENT_TAGS: &[&str] = &[
    "queued_work_started",
    "model_request_started",
    "assistant_prose_delta",
    "reasoning_delta",
    "model_attempt_reset",
    "model_call_recorded",
    "code_block_started",
    "code_block_completed",
    "tool_call_started",
    "tool_call_completed",
    "tool_intent_outcome",
    "final_value",
    "tool_value",
    "usage",
    "child_usage",
    "retry_status",
    "plugin_runtime",
    "queued_input_accepted",
    "queued_messages_committed",
    "error",
];

fn token_usage_sample() -> TokenUsage {
    TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 3,
    }
}

fn token_usage_json() -> serde_json::Value {
    json!({
        "input_tokens": 10,
        "output_tokens": 5,
        "cache_read_input_tokens": 1,
        "cache_write_input_tokens": 2,
        "reasoning_output_tokens": 3,
    })
}

/// Representative construction of every variant paired with its exact expected
/// JSON. Optional fields appear as both `Some` and `None` where meaningful, so
/// the pins cover both the present and absent-when-`None` encodings.
fn sample_events() -> Vec<(&'static str, TurnEvent, serde_json::Value)> {
    vec![
        (
            "queued_work_started",
            TurnEvent::QueuedWorkStarted {
                boundary: QueuedWorkClaimBoundary::Idle,
                batch_ids: vec!["batch-1".to_string()],
                causes: vec![TurnCause {
                    id: "cause-1".to_string(),
                    event_type: "process_wake".to_string(),
                    origin: MessageOrigin::Plugin {
                        plugin_id: "p".to_string(),
                        transient: false,
                    },
                    text: "hello".to_string(),
                }],
            },
            json!({
                "type": "queued_work_started",
                "boundary": "idle",
                "batch_ids": ["batch-1"],
                "causes": [{
                    "id": "cause-1",
                    "event_type": "process_wake",
                    "origin": { "kind": "plugin", "plugin_id": "p" },
                    "text": "hello",
                }],
            }),
        ),
        (
            "model_request_started",
            TurnEvent::ModelRequestStarted {
                protocol_iteration: 2,
            },
            json!({ "type": "model_request_started", "protocol_iteration": 2 }),
        ),
        (
            "assistant_prose_delta",
            TurnEvent::AssistantProseDelta { text: "hi".into() },
            json!({ "type": "assistant_prose_delta", "text": "hi" }),
        ),
        (
            "reasoning_delta",
            TurnEvent::ReasoningDelta {
                text: "thinking".into(),
            },
            json!({ "type": "reasoning_delta", "text": "thinking" }),
        ),
        (
            "model_attempt_reset",
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids: vec![TurnActivityId::new("prose-1")],
                reasoning_correlation_ids: vec![TurnActivityId::new("reasoning-1")],
            },
            json!({
                "type": "model_attempt_reset",
                "assistant_prose_correlation_ids": ["prose-1"],
                "reasoning_correlation_ids": ["reasoning-1"],
            }),
        ),
        (
            "model_call_recorded",
            TurnEvent::ModelCallRecorded {
                record: LlmCallRecord {
                    call_id: LlmCallId("llm-call-1".to_string()),
                    label: Some("primary".to_string()),
                    attempts: vec![AttemptRecord {
                        ordinal: 1,
                        started_at: 7,
                        duration: std::time::Duration::from_millis(3),
                        outcome: AttemptOutcome::Completed,
                        protocol_position: ProtocolPosition::TerminalObserved,
                        retry_budget_consumed: true,
                        retry_decision: None,
                        error: None,
                        evidence: None,
                        generation_disposition: None,
                        usage: None,
                    }],
                },
            },
            json!({
                "type": "model_call_recorded",
                "record": {
                    "call_id": "llm-call-1",
                    "label": "primary",
                    "attempts": [{
                        "ordinal": 1,
                        "started_at": 7,
                        "duration": { "secs": 0, "nanos": 3000000 },
                        "outcome": "completed",
                        "protocol_position": "terminal_observed",
                        "retry_budget_consumed": true,
                    }],
                },
            }),
        ),
        (
            "code_block_started (graph_key present)",
            TurnEvent::CodeBlockStarted {
                language: "python".to_string(),
                code: "print(1)".to_string(),
                graph_key: Some("effect:s:e".to_string()),
            },
            json!({
                "type": "code_block_started",
                "language": "python",
                "code": "print(1)",
                "graph_key": "effect:s:e",
            }),
        ),
        (
            "code_block_started (graph_key absent)",
            TurnEvent::CodeBlockStarted {
                language: "python".to_string(),
                code: "print(1)".to_string(),
                graph_key: None,
            },
            json!({
                "type": "code_block_started",
                "language": "python",
                "code": "print(1)",
            }),
        ),
        (
            "code_block_completed (error + graph_key present)",
            TurnEvent::CodeBlockCompleted {
                language: "python".to_string(),
                output: "1".to_string(),
                error: Some("boom".to_string()),
                success: false,
                duration_ms: 5,
                tool_call_ids: vec!["call-1".to_string()],
                graph_key: Some("effect:s:e".to_string()),
            },
            json!({
                "type": "code_block_completed",
                "language": "python",
                "output": "1",
                "error": "boom",
                "success": false,
                "duration_ms": 5,
                "tool_call_ids": ["call-1"],
                "graph_key": "effect:s:e",
            }),
        ),
        (
            "code_block_completed (error + graph_key absent)",
            TurnEvent::CodeBlockCompleted {
                language: "python".to_string(),
                output: "1".to_string(),
                error: None,
                success: true,
                duration_ms: 5,
                tool_call_ids: vec![],
                graph_key: None,
            },
            json!({
                "type": "code_block_completed",
                "language": "python",
                "output": "1",
                "success": true,
                "duration_ms": 5,
                "tool_call_ids": [],
            }),
        ),
        (
            "tool_call_started (all options present)",
            TurnEvent::ToolCallStarted {
                call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                graph_key: Some("effect:s:e".to_string()),
                parent_call_id: Some("parent-1".to_string()),
            },
            json!({
                "type": "tool_call_started",
                "call_id": "call-1",
                "name": "read_file",
                "args": { "path": "x" },
                "graph_key": "effect:s:e",
                "parent_call_id": "parent-1",
            }),
        ),
        (
            "tool_call_started (all options absent)",
            TurnEvent::ToolCallStarted {
                call_id: None,
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                graph_key: None,
                parent_call_id: None,
            },
            json!({
                "type": "tool_call_started",
                "name": "read_file",
                "args": { "path": "x" },
            }),
        ),
        (
            "tool_call_completed (all options present, success)",
            TurnEvent::ToolCallCompleted {
                call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                output: ToolCallOutput::success("ok"),
                duration_ms: 7,
                graph_key: Some("effect:s:e".to_string()),
                parent_call_id: Some("parent-1".to_string()),
            },
            json!({
                "type": "tool_call_completed",
                "call_id": "call-1",
                "name": "read_file",
                "args": { "path": "x" },
                "output": { "outcome": { "status": "success", "payload": "ok" } },
                "duration_ms": 7,
                "graph_key": "effect:s:e",
                "parent_call_id": "parent-1",
            }),
        ),
        (
            "tool_call_completed (all options absent, failure vocabulary)",
            TurnEvent::ToolCallCompleted {
                call_id: None,
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                output: ToolCallOutput::failure(ToolFailure::tool(
                    ToolFailureClass::Execution,
                    "boom",
                    "kaboom",
                )),
                duration_ms: 7,
                graph_key: None,
                parent_call_id: None,
            },
            json!({
                "type": "tool_call_completed",
                "name": "read_file",
                "args": { "path": "x" },
                "output": {
                    "outcome": {
                        "status": "failure",
                        "payload": {
                            "class": "execution",
                            "code": "boom",
                            "message": "kaboom",
                            "source": "tool",
                            "retry": { "type": "never" },
                        },
                    },
                },
                "duration_ms": 7,
            }),
        ),
        (
            "tool_intent_outcome",
            TurnEvent::ToolIntentOutcome {
                call_id: "call-1".to_string(),
                outcome: ToolIntentExecutionOutcome::Executed {
                    identity: ToolIntentIdentity {
                        session_id: "session-1".to_string(),
                        turn_id: "turn-1".to_string(),
                        tool_call_id: "call-1".to_string(),
                        intent_index: 0,
                        replay_key: "tool-intent:v1:sha256:literal".to_string(),
                    },
                    kind: ToolIntentKind::StartProcess,
                    result: json!({"process_id": "p-1"}),
                },
            },
            json!({
                "type": "tool_intent_outcome",
                "call_id": "call-1",
                "outcome": {
                    "status": "executed",
                    "identity": {
                        "session_id": "session-1",
                        "turn_id": "turn-1",
                        "tool_call_id": "call-1",
                        "intent_index": 0,
                        "replay_key": "tool-intent:v1:sha256:literal",
                    },
                    "kind": "start_process",
                    "result": {"process_id": "p-1"},
                },
            }),
        ),
        (
            "final_value",
            TurnEvent::FinalValue {
                value: json!({ "answer": 42 }),
            },
            json!({ "type": "final_value", "value": { "answer": 42 } }),
        ),
        (
            "tool_value",
            TurnEvent::ToolValue {
                tool_name: "calc".to_string(),
                value: json!(3),
            },
            json!({ "type": "tool_value", "tool_name": "calc", "value": 3 }),
        ),
        (
            "usage",
            TurnEvent::Usage {
                protocol_iteration: 1,
                usage: token_usage_sample(),
                cumulative: token_usage_sample(),
            },
            json!({
                "type": "usage",
                "protocol_iteration": 1,
                "usage": token_usage_json(),
                "cumulative": token_usage_json(),
            }),
        ),
        (
            "child_usage",
            TurnEvent::ChildUsage {
                session_id: "child".to_string(),
                source: "delegate".to_string(),
                model: "m".to_string(),
                protocol_iteration: 1,
                usage: token_usage_sample(),
                cumulative: token_usage_sample(),
            },
            json!({
                "type": "child_usage",
                "session_id": "child",
                "source": "delegate",
                "model": "m",
                "protocol_iteration": 1,
                "usage": token_usage_json(),
                "cumulative": token_usage_json(),
            }),
        ),
        (
            "retry_status",
            TurnEvent::RetryStatus {
                wait_seconds: 3,
                attempt: 1,
                max_attempts: 5,
                reason: "rate_limited".to_string(),
            },
            json!({
                "type": "retry_status",
                "wait_seconds": 3,
                "attempt": 1,
                "max_attempts": 5,
                "reason": "rate_limited",
            }),
        ),
        (
            "plugin_runtime",
            TurnEvent::PluginRuntime {
                plugin_id: "todo".to_string(),
                event: PluginRuntimeEvent::Status {
                    key: "k".to_string(),
                    label: "Working".to_string(),
                    detail: Some("d".to_string()),
                },
            },
            json!({
                "type": "plugin_runtime",
                "plugin_id": "todo",
                "event": {
                    "kind": "status",
                    "key": "k",
                    "label": "Working",
                    "detail": "d",
                },
            }),
        ),
        (
            "queued_input_accepted",
            TurnEvent::QueuedInputAccepted {
                applications: vec![TurnInputApplication {
                    input_id: "input-1".to_string(),
                    source_key: Some("host:source-1".to_string()),
                    turn_id: lash_core::TurnId::from("turn-1"),
                    committed_message_id: "message-1".to_string(),
                    checkpoint: Some(CheckpointKind::AfterWork),
                }],
            },
            json!({
                "type": "queued_input_accepted",
                "applications": [{
                    "input_id": "input-1",
                    "source_key": "host:source-1",
                    "turn_id": "turn-1",
                    "committed_message_id": "message-1",
                    "checkpoint": "after_work",
                }],
            }),
        ),
        (
            "queued_messages_committed",
            TurnEvent::QueuedMessagesCommitted {
                messages: vec![PluginMessage::text(MessageRole::Assistant, "done")],
                checkpoint: CheckpointKind::BeforeCompletion,
            },
            json!({
                "type": "queued_messages_committed",
                "messages": [{ "role": "Assistant", "content": "done" }],
                "checkpoint": "before_completion",
            }),
        ),
        (
            "error",
            TurnEvent::Error {
                message: "boom".to_string(),
            },
            json!({ "type": "error", "message": "boom" }),
        ),
    ]
}

#[test]
fn every_variant_serializes_to_pinned_json() {
    for (label, event, expected) in sample_events() {
        let actual =
            serde_json::to_value(&event).unwrap_or_else(|err| panic!("serialize {label}: {err}"));
        assert_eq!(actual, expected, "serialized shape drifted for `{label}`");

        // The pinned `type` tag is the single source of truth shared with the
        // exhaustive drift guard.
        assert_eq!(
            expected["type"],
            expected_type_tag(&event),
            "tag disagrees with expected_type_tag for `{label}`"
        );

        // The pinned JSON decodes back to the same event (confirms optional
        // fields default to `None` when absent), and re-serializes identically.
        let round_trip: TurnEvent = serde_json::from_value(expected.clone())
            .unwrap_or_else(|err| panic!("deserialize {label}: {err}"));
        assert_eq!(
            serde_json::to_value(&round_trip).unwrap(),
            expected,
            "round-trip drifted for `{label}`"
        );
    }
}

#[test]
fn samples_cover_every_variant() {
    let sampled: BTreeSet<&str> = sample_events()
        .iter()
        .map(|(_, event, _)| expected_type_tag(event))
        .collect();
    let canonical: BTreeSet<&str> = ALL_TURN_EVENT_TAGS.iter().copied().collect();
    assert_eq!(
        sampled, canonical,
        "sample_events must pin exactly one representative per TurnEvent variant"
    );
}

#[test]
fn turn_activity_envelope_flattens_event() {
    let activity = TurnActivity {
        id: TurnActivityId::new("act-1"),
        correlation_id: TurnActivityId::new("corr-1"),
        event: TurnEvent::Error {
            message: "boom".to_string(),
        },
    };

    let json = serde_json::to_value(&activity).expect("serialize activity");
    // `id` and `correlation_id` sit alongside the flattened event fields: the
    // event is not nested under its own key.
    assert_eq!(
        json,
        json!({
            "id": "act-1",
            "correlation_id": "corr-1",
            "type": "error",
            "message": "boom",
        })
    );
    assert!(
        json.get("event").is_none(),
        "the event payload must be flattened into the envelope, not nested"
    );

    let round_trip: TurnActivity = serde_json::from_value(json.clone()).expect("deserialize");
    assert_eq!(serde_json::to_value(&round_trip).unwrap(), json);
}

#[test]
fn arc_prose_and_activity_id_preserve_wire_bytes() {
    let activity = TurnActivity {
        id: TurnActivityId::new("activity-1"),
        correlation_id: TurnActivityId::new("prose-1"),
        event: TurnEvent::AssistantProseDelta {
            text: "hello".into(),
        },
    };

    assert_eq!(
        serde_json::to_vec(&activity).expect("serialize pinned prose activity"),
        br#"{"id":"activity-1","correlation_id":"prose-1","type":"assistant_prose_delta","text":"hello"}"#,
    );
}
