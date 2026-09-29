//! Golden JSON pins for the app-facing [`SessionStreamEvent`] surface and the
//! [`StreamMessageKind`] discriminator its `Message` variant carries.
//!
//! Drift guard: [`expected_type_tag`] and [`expected_message_kind`] are
//! exhaustive `match`es with no wildcard, so a new variant fails to compile
//! here until it is given its wire spelling. [`samples_cover_every_variant`]
//! and [`samples_cover_every_message_kind`] then fail until a representative
//! sample is added to [`sample_events`], so the new shape is pinned rather
//! than silently skipped.

use std::collections::BTreeSet;

use serde_json::json;

use super::{
    AcceptedInjectedTurnInput, ErrorEnvelope, MessageRole, SessionStreamEvent, StreamMessageKind,
    TokenUsage, TurnFailureKind, TurnFinish, TurnOutcome,
};
use crate::llm::types::{StreamBlockIdentity, StreamBlockKind};
use crate::{CheckpointKind, OmittedToolCalls, PluginMessage, PluginRuntimeEvent, ToolCallOutput};

macro_rules! stream_event_tags {
    ($( $variant:ident => $tag:literal, )*) => {
        /// The `type` tag serde writes for this variant. Exhaustive on purpose:
        /// a new variant fails to compile until it is mapped.
        fn expected_type_tag(event: &SessionStreamEvent) -> &'static str {
            match event {
                $( SessionStreamEvent::$variant { .. } => $tag, )*
            }
        }

        const ALL_STREAM_EVENT_TAGS: &[&str] = &[
            $( $tag, )*
        ];
    };
}

stream_event_tags! {
    TextDelta => "text_delta",
    ReasoningDelta => "reasoning_delta",
    StreamBlockStarted => "stream_block_started",
    StreamBlockCompleted => "stream_block_completed",
    ToolCall => "tool_call",
    ToolCallsOmitted => "tool_calls_omitted",
    ToolCallStart => "tool_call_start",
    Message => "message",
    LlmRequest => "llm_request",
    LlmResponse => "llm_response",
    TokenUsage => "token_usage",
    RetryStatus => "retry_status",
    InjectedTurnInputAccepted => "injected_turn_input_accepted",
    InjectedMessagesCommitted => "injected_messages_committed",
    PluginEvent => "plugin_event",
    TurnOutcome => "turn_outcome",
    StoppedPartialAvailable => "stopped_partial_available",
    Done => "done",
    Error => "error",
}

macro_rules! message_kinds {
    ($( $variant:ident => $kind:literal, )*) => {
        /// The `kind` string serde writes for this message kind. Exhaustive on
        /// purpose: a new kind fails to compile until it is mapped.
        fn expected_message_kind(kind: StreamMessageKind) -> &'static str {
            match kind {
                $( StreamMessageKind::$variant => $kind, )*
            }
        }

        const ALL_MESSAGE_KINDS: &[&str] = &[
            $( $kind, )*
        ];
    };
}

message_kinds! {
    TypescriptCode => "typescript_code",
    ChildStreamTruncated => "child_stream_truncated",
}

fn block(id: &str, ordinal: u64, item_id: Option<&str>) -> StreamBlockIdentity {
    StreamBlockIdentity::new(id, ordinal).with_item_id(item_id.map(str::to_string))
}

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

fn envelope_sample() -> ErrorEnvelope {
    ErrorEnvelope {
        kind: TurnFailureKind::LlmProvider,
        code: None,
        terminal_reason: None,
        user_message: "rate limited".to_string(),
        raw: None,
        retryable: None,
        provider_failure_kind: None,
    }
}

fn message_json() -> serde_json::Value {
    json!({ "role": "Assistant", "parts": [{ "id": "", "kind": "Text", "content": "done" }] })
}

/// Representative construction of every variant paired with its exact
/// expected JSON, with one `Message` per [`StreamMessageKind`]. Optional
/// fields appear as both present and absent where meaningful.
fn sample_events() -> Vec<(&'static str, SessionStreamEvent, serde_json::Value)> {
    vec![
        (
            "text_delta",
            SessionStreamEvent::TextDelta {
                content: "hi".to_string(),
                block: block("msg-1:0", 0, Some("msg-1")),
            },
            json!({
                "type": "text_delta",
                "content": "hi",
                "block": { "id": "msg-1:0", "ordinal": 0, "item_id": "msg-1" },
            }),
        ),
        (
            "reasoning_delta",
            SessionStreamEvent::ReasoningDelta {
                content: "thinking".to_string(),
                block: block("rs-1:summary:0", 1, None),
            },
            json!({
                "type": "reasoning_delta",
                "content": "thinking",
                "block": { "id": "rs-1:summary:0", "ordinal": 1 },
            }),
        ),
        (
            "stream_block_started",
            SessionStreamEvent::StreamBlockStarted {
                kind: StreamBlockKind::Reasoning,
                block: block("rs-1:summary:1", 2, Some("rs-1")),
            },
            json!({
                "type": "stream_block_started",
                "kind": "reasoning",
                "block": { "id": "rs-1:summary:1", "ordinal": 2, "item_id": "rs-1" },
            }),
        ),
        (
            "stream_block_completed",
            SessionStreamEvent::StreamBlockCompleted {
                kind: StreamBlockKind::AssistantText,
                block: block("msg-1:0", 3, None),
                content: "done".to_string(),
            },
            json!({
                "type": "stream_block_completed",
                "kind": "assistant_text",
                "block": { "id": "msg-1:0", "ordinal": 3 },
                "content": "done",
            }),
        ),
        (
            "tool_call (provider correlation present)",
            SessionStreamEvent::ToolCall {
                call_id: crate::ToolCallId::fixture("call-1"),
                provider_call_id: Some("call-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                output: ToolCallOutput::success("ok"),
            },
            json!({
                "type": "tool_call",
                "call_id": crate::ToolCallId::fixture("call-1").as_str(),
                "provider_call_id": "call-1",
                "name": "read_file",
                "args": { "path": "x" },
                "output": {
                    "outcome": {
                        "status": "success",
                        "payload": { "$lash_tool_value": "untrusted_json", "value": "ok" },
                    },
                },
            }),
        ),
        (
            "tool_calls_omitted",
            SessionStreamEvent::ToolCallsOmitted {
                summary: OmittedToolCalls {
                    count: 3,
                    failures: 1,
                    attachments: Vec::new(),
                },
            },
            json!({
                "type": "tool_calls_omitted",
                "summary": { "count": 3, "failures": 1, "attachments": [] },
            }),
        ),
        (
            "tool_call_start (provider correlation absent)",
            SessionStreamEvent::ToolCallStart {
                call_id: crate::ToolCallId::fixture("start"),
                provider_call_id: None,
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
            },
            json!({
                "type": "tool_call_start",
                "call_id": crate::ToolCallId::fixture("start").as_str(),
                "name": "read_file",
                "args": { "path": "x" },
            }),
        ),
        (
            "message (typescript_code)",
            SessionStreamEvent::Message {
                text: "finish(1);".to_string(),
                kind: StreamMessageKind::TypescriptCode,
            },
            json!({ "type": "message", "text": "finish(1);", "kind": "typescript_code" }),
        ),
        (
            "message (child_stream_truncated)",
            SessionStreamEvent::Message {
                text: "dropped".to_string(),
                kind: StreamMessageKind::ChildStreamTruncated,
            },
            json!({ "type": "message", "text": "dropped", "kind": "child_stream_truncated" }),
        ),
        (
            "llm_request",
            SessionStreamEvent::LlmRequest {
                protocol_iteration: 1,
                message_count: 4,
                tool_list: "read_file".to_string(),
            },
            json!({
                "type": "llm_request",
                "protocol_iteration": 1,
                "message_count": 4,
                "tool_list": "read_file",
            }),
        ),
        (
            "llm_response",
            SessionStreamEvent::LlmResponse {
                protocol_iteration: 1,
                content: "answer".to_string(),
            },
            json!({ "type": "llm_response", "protocol_iteration": 1, "content": "answer" }),
        ),
        (
            "token_usage",
            SessionStreamEvent::TokenUsage {
                protocol_iteration: 1,
                usage: token_usage_sample(),
                cumulative: token_usage_sample(),
            },
            json!({
                "type": "token_usage",
                "protocol_iteration": 1,
                "usage": token_usage_json(),
                "cumulative": token_usage_json(),
            }),
        ),
        (
            "retry_status (envelope present)",
            SessionStreamEvent::RetryStatus {
                wait_seconds: 3,
                attempt: 1,
                max_attempts: 5,
                reason: "rate_limited".to_string(),
                envelope: Some(envelope_sample()),
            },
            json!({
                "type": "retry_status",
                "wait_seconds": 3,
                "attempt": 1,
                "max_attempts": 5,
                "reason": "rate_limited",
                "envelope": { "kind": "llm_provider", "user_message": "rate limited" },
            }),
        ),
        (
            "injected_turn_input_accepted",
            SessionStreamEvent::InjectedTurnInputAccepted {
                inputs: vec![AcceptedInjectedTurnInput {
                    id: Some("input-1".to_string()),
                    message: PluginMessage::text(MessageRole::Assistant, "done"),
                }],
                checkpoint: CheckpointKind::AfterWork,
            },
            json!({
                "type": "injected_turn_input_accepted",
                "inputs": [{ "id": "input-1", "message": message_json() }],
                "checkpoint": "after_work",
            }),
        ),
        (
            "injected_messages_committed",
            SessionStreamEvent::InjectedMessagesCommitted {
                messages: vec![PluginMessage::text(MessageRole::Assistant, "done")],
                checkpoint: CheckpointKind::BeforeCompletion,
            },
            json!({
                "type": "injected_messages_committed",
                "messages": [message_json()],
                "checkpoint": "before_completion",
            }),
        ),
        (
            "plugin_event",
            SessionStreamEvent::PluginEvent {
                plugin_id: "todo".to_string(),
                event: PluginRuntimeEvent::Status {
                    key: "k".to_string(),
                    label: "Working".to_string(),
                    detail: None,
                },
            },
            json!({
                "type": "plugin_event",
                "plugin_id": "todo",
                "event": { "kind": "status", "key": "k", "label": "Working" },
            }),
        ),
        (
            "turn_outcome",
            SessionStreamEvent::TurnOutcome {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "hi".to_string(),
                }),
            },
            json!({
                "type": "turn_outcome",
                "outcome": { "finished": { "assistant_message": { "text": "hi" } } },
            }),
        ),
        (
            "stopped_partial_available",
            SessionStreamEvent::StoppedPartialAvailable {
                summary: crate::StoppedPartialSummary {
                    id: crate::StoppedPartialId {
                        session_id: crate::SessionId::from("session-1"),
                        root: crate::TurnId::from("root-1"),
                        turn_id: crate::TurnId::from("turn-1"),
                        base: crate::CaptureBase(1),
                        sealed_through: 7,
                    },
                    digest: crate::StoppedPartialDigest([0; 32]),
                    reason: crate::StopReason::UserCancel,
                    recovered_after_process_loss: false,
                    coverage: crate::CaptureCoverage::Complete,
                    eligibility: crate::ResubmissionEligibility::Empty,
                    cut_mid_tool_call: false,
                    tool_outcome_unknown: false,
                    item_count: 0,
                },
            },
            json!({
                "type": "stopped_partial_available",
                "summary": {
                    "id": {
                        "session_id": "session-1",
                        "root": "root-1",
                        "turn_id": "turn-1",
                        "base": 1,
                        "sealed_through": 7,
                    },
                    "digest": vec![0u8; 32],
                    "reason": { "kind": "user_cancel" },
                    "recovered_after_process_loss": false,
                    "coverage": "complete",
                    "eligibility": { "eligibility": "empty" },
                    "cut_mid_tool_call": false,
                    "tool_outcome_unknown": false,
                    "item_count": 0,
                },
            }),
        ),
        ("done", SessionStreamEvent::Done, json!({ "type": "done" })),
        (
            "error (envelope absent)",
            SessionStreamEvent::Error {
                message: "boom".to_string(),
                envelope: None,
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
        assert_eq!(
            expected["type"],
            expected_type_tag(&event),
            "tag disagrees with expected_type_tag for `{label}`"
        );

        let round_trip: SessionStreamEvent = serde_json::from_value(expected.clone())
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
    let canonical: BTreeSet<&str> = ALL_STREAM_EVENT_TAGS.iter().copied().collect();
    assert_eq!(
        sampled, canonical,
        "sample_events must pin a representative of every SessionStreamEvent variant"
    );
}

#[test]
fn samples_cover_every_message_kind() {
    let sampled: BTreeSet<&str> = sample_events()
        .iter()
        .filter_map(|(label, event, expected)| match event {
            SessionStreamEvent::Message { kind, .. } => {
                let spelling = expected_message_kind(*kind);
                assert_eq!(expected["kind"], spelling, "kind drifted for `{label}`");
                assert_eq!(kind.as_str(), spelling, "as_str drifted for `{label}`");
                Some(spelling)
            }
            _ => None,
        })
        .collect();
    let canonical: BTreeSet<&str> = ALL_MESSAGE_KINDS.iter().copied().collect();
    assert_eq!(
        sampled, canonical,
        "sample_events must pin a Message for every StreamMessageKind"
    );
}

#[test]
fn an_unknown_message_kind_is_refused() {
    let err = serde_json::from_value::<SessionStreamEvent>(json!({
        "type": "message",
        "text": "x",
        "kind": "final",
    }))
    .expect_err("the message kind set is closed");
    assert!(err.to_string().contains("final"), "{err}");
}
