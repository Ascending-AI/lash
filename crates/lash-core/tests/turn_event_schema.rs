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

use lash_core::llm::types::{LlmTerminalReason, ProviderFailureKind};
use lash_protocol_types::{
    ErrorEnvelope, FailureCode, ReportedFailure, RetryProgress, SessionStreamEvent, TurnFailureKind,
};
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};
use lash_vocabulary::identity::TurnId;
use std::collections::BTreeSet;

use lash_core::CheckpointKind;
use lash_core::LlmCallId;
use lash_core::PluginRuntimeEvent;
use lash_core::ToolCallOutput;
use lash_core::ToolFailure;
use lash_core::ToolFailureClass;
use lash_core::ToolIntentActivityOutcome;
use lash_core::TurnActivity;
use lash_core::TurnActivityId;
use lash_core::TurnEvent;
use lash_core::TurnInputApplication;
use lash_vocabulary::ToolIntentKind;
use serde_json::json;

macro_rules! turn_event_tags {
    ($( $variant:ident => $tag:literal, )*) => {
        /// The `type` tag serde writes for this variant. Exhaustive on purpose: a new
        /// variant fails to compile until it is mapped, forcing the author here.
        fn expected_type_tag(event: &TurnEvent) -> &'static str {
            match event {
                $( TurnEvent::$variant { .. } => $tag, )*
            }
        }

        /// Canonical list of every serialized tag. Paired with [`samples_cover_every_variant`],
        /// this catches a variant that gained a tag mapping but no pinned sample.
        const ALL_TURN_EVENT_TAGS: &[&str] = &[
            $( $tag, )*
        ];
    };
}

turn_event_tags! {
    TurnStarted => "turn_started",
    ModelRequestStarted => "model_request_started",
    CheckpointRecorded => "checkpoint_recorded",
    StreamBlock => "stream_block",
    ModelAttemptReset => "model_attempt_reset",
    ModelCallRecorded => "model_call_recorded",
    CodeBlockStarted => "code_block_started",
    CodeBlockCompleted => "code_block_completed",
    ToolCallStarted => "tool_call_started",
    ToolCallCompleted => "tool_call_completed",
    ToolIntentOutcome => "tool_intent_outcome",
    Finished => "finished",
    RetryStatus => "retry_status",
    PluginRuntime => "plugin_runtime",
    QueuedInputAccepted => "queued_input_accepted",
    Error => "error",
}

fn block_identity(
    id: &str,
    ordinal: u64,
    item_id: Option<&str>,
) -> lash_core::llm::types::StreamBlockIdentity {
    lash_core::llm::types::StreamBlockIdentity::new(id, ordinal)
        .with_item_id(item_id.map(str::to_string))
}

fn sample_events() -> Vec<(&'static str, TurnEvent, serde_json::Value)> {
    vec![
        (
            "turn_started",
            TurnEvent::TurnStarted {
                run_id: lash_vocabulary::identity::RunId::fixture("run-1"),
                turn_id: TurnId::from("turn-1"),
            },
            json!({
                "type": "turn_started",
                "run_id": "run-1",
                "turn_id": "turn-1",
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
            "checkpoint_recorded",
            TurnEvent::CheckpointRecorded {
                protocol_iteration: 2,
            },
            json!({ "type": "checkpoint_recorded", "protocol_iteration": 2 }),
        ),
        (
            "stream_block",
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                block: block_identity("rs-1:summary:0", 1, Some("rs-1")),
                text: "thinking".into(),
            }),
            json!({
                "type": "stream_block",
                "phase": "delta",
                "kind": "reasoning",
                "block": { "id": "rs-1:summary:0", "ordinal": 1, "item_id": "rs-1" },
                "text": "thinking",
            }),
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
                call_id: LlmCallId("llm-call-1".to_string()),
            },
            json!({ "type": "model_call_recorded", "call_id": "llm-call-1" }),
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
            "code_block_completed (failed + graph_key present)",
            TurnEvent::CodeBlockCompleted {
                language: "python".to_string(),
                prints: Some(vec!["1".to_string().into()]),
                prints_retained: None,
                result: lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Program,
                    "boom",
                )),
                duration_ms: Some(5),
                tool_call_ids: Some(vec![lash_core::ToolCallId::fixture("call-1")]),
                graph_key: Some("effect:s:e".to_string()),
            },
            json!({
                "type": "code_block_completed",
                "language": "python",
                "prints": [{
                    "text": "1",
                    "value": "1",
                    "projection": {
                        "truncated": false,
                        "original_chars": 1,
                        "projected_chars": 1,
                        "limit_chars": 1,
                    },
                }],
                "result": {
                    "kind": "failed",
                    "value": { "kind": "program", "message": "boom" },
                },
                "duration_ms": 5,
                "tool_call_ids": [lash_core::ToolCallId::fixture("call-1").as_str()],
                "graph_key": "effect:s:e",
            }),
        ),
        (
            "code_block_completed (finished + graph_key absent)",
            TurnEvent::CodeBlockCompleted {
                language: "python".to_string(),
                prints: Some(Vec::new()),
                prints_retained: None,
                result: lash_core::CellOutcome::finished_by(
                    "finish",
                    lash_core::ToolCallId::fixture("call-1"),
                    json!(null),
                ),
                duration_ms: Some(5),
                tool_call_ids: Some(vec![]),
                graph_key: None,
            },
            json!({
                "type": "code_block_completed",
                "language": "python",
                "prints": [],
                "result": { "kind": "controlled", "value": {
                    "tool_name": "finish",
                    "call_id": lash_core::ToolCallId::fixture("call-1"),
                    "control": { "type": "finish", "value": { "inline": null } },
                } },
                "duration_ms": 5,
                "tool_call_ids": [],
            }),
        ),
        (
            "tool_call_started (all options present)",
            TurnEvent::ToolCallStarted {
                call_id: lash_core::ToolCallId::fixture("call-1"),
                provider_call_id: Some("provider-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                graph_key: Some("effect:s:e".to_string()),
            },
            json!({
                "type": "tool_call_started",
                "call_id": lash_core::ToolCallId::fixture("call-1").as_str(),
                "provider_call_id": "provider-1",
                "name": "read_file",
                "args": { "path": "x" },
                "graph_key": "effect:s:e",
            }),
        ),
        (
            "tool_call_started (all options absent)",
            TurnEvent::ToolCallStarted {
                call_id: lash_core::ToolCallId::fixture("call-2"),
                provider_call_id: None,
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                graph_key: None,
            },
            json!({
                "type": "tool_call_started",
                "call_id": lash_core::ToolCallId::fixture("call-2").as_str(),
                "name": "read_file",
                "args": { "path": "x" },
            }),
        ),
        (
            "tool_call_completed (all options present, success)",
            TurnEvent::ToolCallCompleted {
                call_id: lash_core::ToolCallId::fixture("call-1"),
                provider_call_id: Some("provider-1".to_string()),
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                output: ToolCallOutput::success("ok"),
                duration_ms: Some(7),
                graph_key: Some("effect:s:e".to_string()),
            },
            json!({
                "type": "tool_call_completed",
                "call_id": lash_core::ToolCallId::fixture("call-1").as_str(),
                "provider_call_id": "provider-1",
                "name": "read_file",
                "args": { "path": "x" },
                "output": {
                    "outcome": {
                        "status": "success",
                        "payload": {
                            "$lash_tool_value": "untrusted_json",
                            "value": "ok",
                        },
                    },
                },
                "duration_ms": 7,
                "graph_key": "effect:s:e",
            }),
        ),
        (
            "tool_call_completed (all options absent, failure vocabulary)",
            TurnEvent::ToolCallCompleted {
                call_id: lash_core::ToolCallId::fixture("call-2"),
                provider_call_id: None,
                name: "read_file".to_string(),
                args: json!({ "path": "x" }),
                output: ToolCallOutput::failure(ToolFailure::tool(
                    ToolFailureClass::Execution,
                    "boom",
                    "kaboom",
                )),
                duration_ms: None,
                graph_key: None,
            },
            json!({
                "type": "tool_call_completed",
                "call_id": lash_core::ToolCallId::fixture("call-2").as_str(),
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
                        },
                    },
                },
            }),
        ),
        (
            "tool_intent_outcome",
            TurnEvent::ToolIntentOutcome {
                call_id: lash_core::ToolCallId::fixture("call-1"),
                outcome: ToolIntentActivityOutcome::Executed {
                    intent_index: 0,
                    kind: ToolIntentKind::StartProcess,
                    realized: lash_core::ToolIntentRealizedRef::Process(
                        lash_core::ProcessIdMint::sequential_id_for_testing(1),
                    ),
                },
            },
            json!({
                "type": "tool_intent_outcome",
                "call_id": lash_core::ToolCallId::fixture("call-1").as_str(),
                "outcome": {
                    "status": "executed",
                    "intent_index": 0,
                    "kind": "start_process",
                    "realized": {
                        "kind": "process",
                        "result": lash_core::ProcessIdMint::sequential_id_for_testing(1),
                    },
                },
            }),
        ),
        (
            "tool_intent_refused",
            TurnEvent::ToolIntentOutcome {
                call_id: lash_core::ToolCallId::fixture("call-1"),
                outcome: ToolIntentActivityOutcome::Refused {
                    intent_index: 2,
                    kind: ToolIntentKind::CancelProcess,
                    refusal: lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::MintingRunCancelled,
                },
            },
            json!({"type":"tool_intent_outcome", "call_id":lash_core::ToolCallId::fixture("call-1"),
                "outcome":{"status":"refused","intent_index":2,"kind":"cancel_process","refusal":{"reason":"minting_run_cancelled"}}}),
        ),
        (
            "tool_intent_batch_refused",
            TurnEvent::ToolIntentOutcome {
                call_id: lash_core::ToolCallId::fixture("call-1"),
                outcome: ToolIntentActivityOutcome::BatchRefused {
                    refusal: lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason::UnsupportedProtocolVersion {
                        recorded: 0,
                    },
                },
            },
            json!({"type":"tool_intent_outcome", "call_id":lash_core::ToolCallId::fixture("call-1"),
                "outcome":{"status":"batch_refused","refusal":{"reason":"unsupported_protocol_version","recorded":0}}}),
        ),
        (
            "unavailable_cell_evidence",
            TurnEvent::CodeBlockCompleted {
                language: "python".into(),
                prints: None,
                prints_retained: None,
                result: lash_core::CellOutcome::Failed(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    "invocation failed",
                )),
                duration_ms: None,
                tool_call_ids: None,
                graph_key: None,
            },
            json!({"type":"code_block_completed","language":"python","result":{"kind":"failed","value":{"kind":"host","message":"invocation failed"}}}),
        ),
        (
            "finished",
            TurnEvent::Finished {
                tool_name: "calc".to_string(),
                value: json!(3),
            },
            json!({ "type": "finished", "tool_name": "calc", "value": 3 }),
        ),
        (
            "retry_status",
            TurnEvent::RetryStatus(RetryProgress {
                wait_seconds: 3,
                attempt: 1,
                max_attempts: 5,
                reason: "rate_limited".to_string(),
                envelope: None,
            }),
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
                    input_id: "input-1".into(),
                    source_key: Some("host:source-1".to_string()),
                    turn_id: lash_vocabulary::identity::TurnId::from("turn-1"),
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
            "error",
            TurnEvent::Error(ReportedFailure {
                message: "boom".to_string(),
                envelope: Some(provider_failure_envelope()),
            }),
            json!({
                "type": "error",
                "message": "boom",
                "envelope": provider_failure_envelope_json(),
            }),
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
        event: TurnEvent::Error(ReportedFailure {
            message: "boom".to_string(),
            envelope: None,
        }),
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

fn provider_failure_envelope() -> ErrorEnvelope {
    ErrorEnvelope {
        kind: TurnFailureKind::LlmProvider,
        code: Some(FailureCode::provider("insufficient_quota")),
        terminal_reason: Some(LlmTerminalReason::ProviderError),
        user_message: "provider call failed".to_string(),
        raw: None,
        retryable: Some(false),
        provider_failure_kind: Some(ProviderFailureKind::Quota),
    }
}

fn provider_failure_envelope_json() -> serde_json::Value {
    json!({
        "kind": "llm_provider",
        "code": "provider:insufficient_quota",
        "terminal_reason": "provider_error",
        "user_message": "provider call failed",
        "retryable": false,
        "provider_failure_kind": "quota",
    })
}

/// One fact, one payload: every step of a block's lifecycle, a reported
/// failure and a retry serialize to the same fields on the turn-activity lane
/// and on the session stream, so one host decoder reads both.
#[test]
fn session_stream_and_turn_activity_carry_one_payload_per_fact() {
    let block = block_identity("rs-1:summary:1", 2, Some("rs-1"));
    let lifecycle = [
        (
            StreamBlockEvent::started(StreamBlockKind::Reasoning, block.clone()),
            json!({
                "type": "stream_block",
                "phase": "started",
                "kind": "reasoning",
                "block": { "id": "rs-1:summary:1", "ordinal": 2, "item_id": "rs-1" },
            }),
        ),
        (
            StreamBlockEvent::delta(StreamBlockKind::Reasoning, block.clone(), "th"),
            json!({
                "type": "stream_block",
                "phase": "delta",
                "kind": "reasoning",
                "block": { "id": "rs-1:summary:1", "ordinal": 2, "item_id": "rs-1" },
                "text": "th",
            }),
        ),
        (
            StreamBlockEvent::completed(StreamBlockKind::Reasoning, block, "thought"),
            json!({
                "type": "stream_block",
                "phase": "completed",
                "kind": "reasoning",
                "block": { "id": "rs-1:summary:1", "ordinal": 2, "item_id": "rs-1" },
                "text": "thought",
            }),
        ),
    ];
    for (event, expected) in lifecycle {
        let activity = serde_json::to_value(TurnEvent::StreamBlock(event.clone())).unwrap();
        let session = serde_json::to_value(SessionStreamEvent::StreamBlock(event.clone())).unwrap();
        assert_eq!(activity, expected);
        assert_eq!(session, expected);
        let TurnEvent::StreamBlock(decoded) = serde_json::from_value(activity).unwrap() else {
            panic!("a stream block decodes as a stream block");
        };
        assert_eq!(decoded, event);
    }

    let failure = ReportedFailure {
        message: "LLM error: quota".to_string(),
        envelope: Some(provider_failure_envelope()),
    };
    let activity = serde_json::to_value(TurnEvent::Error(failure.clone())).unwrap();
    assert_eq!(
        activity,
        serde_json::to_value(SessionStreamEvent::Error(failure)).unwrap()
    );
    assert_eq!(activity["envelope"]["kind"], "llm_provider");
    assert_eq!(activity["envelope"]["code"], "provider:insufficient_quota");
    assert_eq!(activity["envelope"]["retryable"], false);

    let retry = RetryProgress {
        wait_seconds: 3,
        attempt: 1,
        max_attempts: 5,
        reason: "rate_limited".to_string(),
        envelope: Some(provider_failure_envelope()),
    };
    assert_eq!(
        serde_json::to_value(TurnEvent::RetryStatus(retry.clone())).unwrap(),
        serde_json::to_value(SessionStreamEvent::RetryStatus(retry)).unwrap()
    );
}
