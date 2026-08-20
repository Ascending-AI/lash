//! Golden schema pins for the on-disk trace format.
//!
//! Trace records are a durable, cross-tool contract (JSONL files consumed by
//! the trace viewer, exporters, and the OTel bridge). These tests pin the
//! `schema_version` tripwire, the `type` tag for every [`TraceEvent`] variant,
//! the full payload shape of the load-bearing variants, and a JSONL round-trip
//! carrying an `exec_code_completed` diagnostic.

use std::collections::BTreeSet;

use lash_trace::{
    TraceBranchSelection, TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceLanguageChildExecution,
    TraceLanguageExecution, TraceLanguageExecutionIdentity, TraceLanguageExecutionMap,
    TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus, TraceLlmRequest, TraceLlmResponse, TraceProviderReplayDropEvent,
    TraceProviderReplayDropReason, TraceProviderReplayKind, TraceProviderRequestEvent,
    TraceProviderRouteIdentity, TraceProviderStreamEvent, TraceRecord, TraceRuntimeScope,
    TraceRuntimeStreamEvent, TraceRuntimeSubject, TraceTokenUsage, TraceToolCallOutcome,
    TraceToolCallOutput,
};
use serde_json::json;

#[test]
fn trace_schema_version_is_pinned_at_7() {
    // Tripwire. This is the current on-disk trace schema version. Every reader
    // (viewer, exporter, OTel bridge) keys off it, so a change here must be a
    // deliberate, documented schema bump — see the crate-level rustdoc and the
    // `TRACE_SCHEMA_VERSION` doc comment for the bump policy. If this fails,
    // read that policy before touching the constant.
    assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 7);
}

#[test]
fn pre_frame_key_trace_schema_is_rejected_with_literal_versions() {
    assert_eq!(
        lash_trace::ensure_trace_schema_version(3),
        Err(lash_trace::TraceSchemaVersionError {
            actual: 3,
            expected: 7,
        })
    );
}

#[test]
fn documented_trace_record_decode_rejects_schema_3_before_payload_interpretation() {
    let otherwise_current = r#"{"schema_version":3,"id":"legacy-record","timestamp":"2026-05-11T11:42:01.234+00:00","context":{},"type":"session_started"}"#;
    let error = serde_json::from_str::<TraceRecord>(otherwise_current)
        .expect_err("schema-3 trace records must be refused during typed decode");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 3; expected 7"
    );

    let stale_and_malformed = r#"{"schema_version":3,"payload":"not a current event"}"#;
    let error = serde_json::from_str::<TraceRecord>(stale_and_malformed)
        .expect_err("the version refusal must precede current-shape validation");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 3; expected 7"
    );
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
    assert_eq!(json["schema_version"], 7);
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

fn lashlang_identity() -> TraceLanguageExecutionIdentity {
    TraceLanguageExecutionIdentity {
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
/// given a sample, and [`expected_event_kind`] is an exhaustive match that
/// fails to compile until the variant is given a tag.
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
        TraceEvent::CompositionChanged {
            fingerprint: "composition-sha".to_string(),
            rendered_system_prompt: "system policy".to_string(),
            tool_schemas: vec![lash_trace::TraceToolSpec {
                name: "search".to_string(),
                description: "Search documents".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "array" }),
            }],
        },
        TraceEvent::RollingHistoryCompactionNeeded {
            context_budget_tokens: 30_000,
            max_context_tokens: 40_000,
            threshold_tokens: 20_000,
        },
        TraceEvent::RollingHistoryPromptPruned {
            context_budget_tokens: 30_000,
            max_context_tokens: 40_000,
            dropped_prefix_messages: 2,
            retained_messages: 1,
        },
        TraceEvent::RollingHistoryCompactionStarted {
            source_messages: 3,
            instructions_present: true,
        },
        TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 1 },
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
            attempts: None,
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
            attempts: None,
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
        TraceEvent::ProviderReplayDropped {
            event: TraceProviderReplayDropEvent {
                replay_kind: TraceProviderReplayKind::Reasoning,
                reason: TraceProviderReplayDropReason::ForeignRoute,
                minting_route: Some(TraceProviderRouteIdentity {
                    provider: "anthropic".to_string(),
                    endpoint: "https://api.anthropic.com".to_string(),
                    model: "claude".to_string(),
                }),
                serving_route: TraceProviderRouteIdentity {
                    provider: "google_oauth".to_string(),
                    endpoint: "https://cloudcode-pa.googleapis.com/v1internal".to_string(),
                    model: "gemini".to_string(),
                },
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
            attempts: None,
        },
        TraceEvent::JournaledEffectStarted {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
        },
        TraceEvent::JournaledEffectSettled {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
            status: "completed".to_string(),
        },
        TraceEvent::DurableWaitParked {
            wait_kind: "await_event".to_string(),
        },
        TraceEvent::DurableWaitResolved {
            wait_kind: "await_event".to_string(),
            resolution: "ok".to_string(),
        },
        TraceEvent::DurableTimerStarted { duration_ms: 250 },
        TraceEvent::DurableTimerResolved {
            duration_ms: 250,
            status: "resolved".to_string(),
        },
        TraceEvent::DurableSegmentBoundary {
            reason: "journal_budget".to_string(),
            effects_executed: 10_000,
            journaled_bytes_estimate: None,
        },
        TraceEvent::StoreErrorObserved {
            operation: "session_restore".to_string(),
            error_class: "StoredDataCorrupt".to_string(),
            message: "stored SessionHeadMeta data is corrupt".to_string(),
        },
        TraceEvent::ProtocolStep {
            plugin_id: "custom".to_string(),
            payload: json!({ "code": "print 1" }),
        },
        TraceEvent::TokenUsage {
            usage: token_usage_sample(),
            cumulative: Some(token_usage_sample()),
        },
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: TraceLanguageExecution {
                event_key: "process:p1:finished".to_string(),
                identity: lashlang_identity(),
                payload: TraceLanguageExecutionPayload::ExecutionFinished {
                    status: TraceLanguageExecutionStatus::Completed,
                    error: None,
                },
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

macro_rules! trace_event_kinds {
    ($( $variant:ident => $kind:literal, )*) => {
        /// The expected event kind string for this variant. Exhaustive on purpose:
        /// a new variant fails to compile here until it is mapped.
        fn expected_event_kind(event: &TraceEvent) -> &'static str {
            match event {
                $( TraceEvent::$variant { .. } => $kind, )*
            }
        }

        /// Canonical list of every trace event kind string. Paired with
        /// [`event_samples_cover_every_variant`], this catches a variant that
        /// gained a kind mapping but no pinned sample.
        const ALL_TRACE_EVENT_KINDS: &[&str] = &[
            $( $kind, )*
        ];
    };
}

trace_event_kinds! {
    SessionStarted => "session_started",
    TurnStarted => "turn_started",
    PromptBuilt => "prompt_built",
    CompositionChanged => "composition_changed",
    RollingHistoryCompactionNeeded => "rolling_history_compaction_needed",
    RollingHistoryCompactionStarted => "rolling_history_compaction_started",
    RollingHistoryCompactionCompleted => "rolling_history_compaction_completed",
    RollingHistoryPromptPruned => "rolling_history_prompt_pruned",
    LlmCallStarted => "llm_call_started",
    LlmCallCompleted => "llm_call_completed",
    LlmCallFailed => "llm_call_failed",
    ProviderRequest => "provider_request",
    ProviderReplayDropped => "provider_replay_dropped",
    EffectEnvelopeDiff => "effect_envelope_diff",
    ProviderStreamEvent => "provider_stream_event",
    RuntimeStreamEvent => "runtime_stream_event",
    ToolCallStarted => "tool_call_started",
    ToolCallCompleted => "tool_call_completed",
    JournaledEffectStarted => "journaled_effect_started",
    JournaledEffectSettled => "journaled_effect_settled",
    DurableWaitParked => "durable_wait_parked",
    DurableWaitResolved => "durable_wait_resolved",
    DurableTimerStarted => "durable_timer_started",
    DurableTimerResolved => "durable_timer_resolved",
    DurableSegmentBoundary => "durable_segment_boundary",
    StoreErrorObserved => "store_error_observed",
    ProtocolStep => "protocol_step",
    TokenUsage => "token_usage",
    LanguageExecution => "language_execution",
    TurnCompleted => "turn_completed",
    Custom => "custom",
}

#[test]
fn every_event_type_tag_matches_kind() {
    for event in event_samples() {
        let kind = event.kind();
        let json = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(
            json["type"], kind,
            "serialized `type` disagrees with TraceEvent::kind() for `{kind}`"
        );
        assert_eq!(
            expected_event_kind(&event),
            kind,
            "expected_event_kind disagrees with TraceEvent::kind() for `{kind}`"
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
fn composition_change_is_a_complete_snapshot_at_schema_version_five() {
    let record = TraceRecord::new(
        TraceContext::default().for_session("composition-session"),
        TraceEvent::CompositionChanged {
            fingerprint: "4c94f3".to_string(),
            rendered_system_prompt: "Follow the stored policy.".to_string(),
            tool_schemas: vec![lash_trace::TraceToolSpec {
                name: "search".to_string(),
                description: "Search documents".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
                output_schema: json!({
                    "type": "object",
                    "properties": { "matches": { "type": "array" } },
                    "required": ["matches"]
                }),
            }],
        },
    );
    let mut json = serde_json::to_value(&record).expect("serialize composition snapshot");

    assert_eq!(json["schema_version"], 7);
    json["schema_version"] = json!(5);
    assert_eq!(json["schema_version"], 5);
    assert_eq!(json["type"], "composition_changed");
    assert_eq!(json["fingerprint"], "4c94f3");
    assert_eq!(json["rendered_system_prompt"], "Follow the stored policy.");
    assert_eq!(json["tool_schemas"][0]["name"], "search");
    assert_eq!(
        json["tool_schemas"][0]["input_schema"]["required"][0],
        "query"
    );
    assert_eq!(
        json["tool_schemas"][0]["output_schema"]["required"][0],
        "matches"
    );

    let object = json.as_object_mut().expect("trace record object");
    assert!(
        object.remove("type").is_some()
            && object.remove("fingerprint").is_some()
            && object.remove("rendered_system_prompt").is_some()
            && object.remove("tool_schemas").is_some(),
        "the field-strip probe removes exactly the additive event payload"
    );
    assert_eq!(
        object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        ["schema_version", "id", "timestamp", "context"]
            .into_iter()
            .collect(),
        "stripping the additive event leaves the pre-existing trace-record envelope"
    );
}

#[derive(Debug, PartialEq, Eq)]
enum HistoricalV4ReadError {
    UnsupportedVersion { actual: u32, expected: u32 },
    Payload(String),
}

fn read_with_historical_v4_reader(input: &str) -> Result<(), HistoricalV4ReadError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| HistoricalV4ReadError::Payload(error.to_string()))?;
    let actual = value["schema_version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| HistoricalV4ReadError::Payload("missing schema_version".to_string()))?;
    if actual != 4 {
        return Err(HistoricalV4ReadError::UnsupportedVersion {
            actual,
            expected: 4,
        });
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum HistoricalV4Event {
        SessionStarted,
    }

    serde_json::from_value::<HistoricalV4Event>(value)
        .map(|_| ())
        .map_err(|error| HistoricalV4ReadError::Payload(error.to_string()))
}

#[test]
fn historical_v4_reader_refuses_v5_before_interpreting_new_closed_enum_variant() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::CompositionChanged {
            fingerprint: "fingerprint".to_string(),
            rendered_system_prompt: "prompt".to_string(),
            tool_schemas: Vec::new(),
        },
    );
    let current_wire = serde_json::to_string(&record).expect("serialize composition event");
    let wire = current_wire.replacen("\"schema_version\":7", "\"schema_version\":5", 1);
    assert_eq!(
        read_with_historical_v4_reader(&wire),
        Err(HistoricalV4ReadError::UnsupportedVersion {
            actual: 5,
            expected: 4,
        }),
        "the old reader's typed version gate must run before its closed enum decoder"
    );

    let forced_v4 = wire.replacen("\"schema_version\":5", "\"schema_version\":4", 1);
    let error = read_with_historical_v4_reader(&forced_v4)
        .expect_err("without the version gate, the new enum variant is unknown");
    assert!(
        matches!(error, HistoricalV4ReadError::Payload(message) if message.contains("unknown variant"))
    );
}

#[derive(Debug, PartialEq, Eq)]
enum HistoricalV5ReadError {
    UnsupportedVersion { actual: u32, expected: u32 },
    Payload(String),
}

fn read_with_historical_v5_reader(input: &str) -> Result<(), HistoricalV5ReadError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| HistoricalV5ReadError::Payload(error.to_string()))?;
    let actual = value["schema_version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| HistoricalV5ReadError::Payload("missing schema_version".to_string()))?;
    if actual != 5 {
        return Err(HistoricalV5ReadError::UnsupportedVersion {
            actual,
            expected: 5,
        });
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum HistoricalV5Event {
        SessionStarted,
        CompositionChanged,
    }

    serde_json::from_value::<HistoricalV5Event>(value)
        .map(|_| ())
        .map_err(|error| HistoricalV5ReadError::Payload(error.to_string()))
}

#[test]
fn historical_v5_reader_refuses_v6_provider_replay_dropped_before_interpreting_variant() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::ProviderReplayDropped {
            event: TraceProviderReplayDropEvent {
                replay_kind: TraceProviderReplayKind::Reasoning,
                reason: TraceProviderReplayDropReason::ForeignRoute,
                minting_route: Some(TraceProviderRouteIdentity {
                    provider: "anthropic".to_string(),
                    endpoint: "https://api.anthropic.com".to_string(),
                    model: "claude".to_string(),
                }),
                serving_route: TraceProviderRouteIdentity {
                    provider: "google_oauth".to_string(),
                    endpoint: "https://cloudcode-pa.googleapis.com/v1internal".to_string(),
                    model: "gemini".to_string(),
                },
            },
        },
    );
    let current_wire = serde_json::to_string(&record).expect("serialize replay-drop event");
    let wire = current_wire.replacen("\"schema_version\":7", "\"schema_version\":6", 1);
    assert_eq!(
        read_with_historical_v5_reader(&wire),
        Err(HistoricalV5ReadError::UnsupportedVersion {
            actual: 6,
            expected: 5,
        }),
        "the v5 reader's typed version gate must run before its closed enum decoder"
    );

    let forced_v5 = wire.replacen("\"schema_version\":6", "\"schema_version\":5", 1);
    let error = read_with_historical_v5_reader(&forced_v5)
        .expect_err("without the version gate, the new enum variant is unknown");
    assert!(
        matches!(error, HistoricalV5ReadError::Payload(message) if message.contains("unknown variant"))
    );
}

#[derive(Debug, PartialEq, Eq)]
enum HistoricalV6ReadError {
    UnsupportedVersion { actual: u32, expected: u32 },
    Payload(String),
}

fn read_with_historical_v6_reader(input: &str) -> Result<(), HistoricalV6ReadError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| HistoricalV6ReadError::Payload(error.to_string()))?;
    let actual = value["schema_version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| HistoricalV6ReadError::Payload("missing schema_version".to_string()))?;
    if actual != 6 {
        return Err(HistoricalV6ReadError::UnsupportedVersion {
            actual,
            expected: 6,
        });
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum HistoricalV6Event {
        SessionStarted,
        CompositionChanged,
        ProviderReplayDropped,
        // v6 spelled the execution stream after the one language that could
        // produce it. v7 renames the tag, which is why a v6 reader must not
        // reach this decoder for a v7 record.
        LashlangExecution,
    }

    serde_json::from_value::<HistoricalV6Event>(value)
        .map(|_| ())
        .map_err(|error| HistoricalV6ReadError::Payload(error.to_string()))
}

#[test]
fn historical_v6_reader_refuses_v7_language_execution_before_interpreting_variant() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::LanguageExecution {
            language: "lashlang".to_string(),
            event: TraceLanguageExecution {
                event_key: "execution:finished".to_string(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::new("s1"),
                    subject: TraceRuntimeSubject::Process {
                        process_id: "p1".to_string(),
                    },
                    module_ref: "module:v1".to_string(),
                    entry_kind: "program".to_string(),
                    entry_ref: None,
                    entry_name: "main".to_string(),
                },
                payload: TraceLanguageExecutionPayload::ExecutionFinished {
                    status: TraceLanguageExecutionStatus::Completed,
                    error: None,
                },
            },
        },
    );
    let wire = serde_json::to_string(&record).expect("serialize v7 language-execution event");
    assert_eq!(
        read_with_historical_v6_reader(&wire),
        Err(HistoricalV6ReadError::UnsupportedVersion {
            actual: 7,
            expected: 6,
        }),
        "the v6 reader's typed version gate must run before its closed enum decoder"
    );

    let forced_v6 = wire.replacen("\"schema_version\":7", "\"schema_version\":6", 1);
    let error = read_with_historical_v6_reader(&forced_v6)
        .expect_err("without the version gate, the renamed variant is unknown");
    assert!(
        matches!(error, HistoricalV6ReadError::Payload(message) if message.contains("unknown variant"))
    );
}

#[test]
fn rolling_history_events_pin_decision_payloads() {
    let events = [
        TraceEvent::RollingHistoryCompactionNeeded {
            context_budget_tokens: 30_000,
            max_context_tokens: 40_000,
            threshold_tokens: 20_000,
        },
        TraceEvent::RollingHistoryPromptPruned {
            context_budget_tokens: 30_000,
            max_context_tokens: 40_000,
            dropped_prefix_messages: 2,
            retained_messages: 1,
        },
        TraceEvent::RollingHistoryCompactionStarted {
            source_messages: 3,
            instructions_present: true,
        },
        TraceEvent::RollingHistoryCompactionCompleted { summary_nodes: 1 },
    ];

    assert_eq!(
        events.map(|event| serde_json::to_value(event).expect("serialize event")),
        [
            json!({
                "type": "rolling_history_compaction_needed",
                "context_budget_tokens": 30_000,
                "max_context_tokens": 40_000,
                "threshold_tokens": 20_000,
            }),
            json!({
                "type": "rolling_history_prompt_pruned",
                "context_budget_tokens": 30_000,
                "max_context_tokens": 40_000,
                "dropped_prefix_messages": 2,
                "retained_messages": 1,
            }),
            json!({
                "type": "rolling_history_compaction_started",
                "source_messages": 3,
                "instructions_present": true,
            }),
            json!({
                "type": "rolling_history_compaction_completed",
                "summary_nodes": 1,
            }),
        ]
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
            attempts: None,
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
        attempts: None,
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
fn retry_attempts_are_optional_additive_event_fields() {
    let attempts = Some(vec![
        lash_trace::TraceRetryAttempt {
            ordinal: 1,
            outcome: "failed".to_string(),
            duration_ms: 10,
            reason: Some("http_429".to_string()),
            delay_ms: Some(250),
            execution_evidence: Some(lash_trace::TraceExecutionEvidence {
                served_model: Some("served-model".to_string()),
                provider_response_id: Some("provider-response-1".to_string()),
                reasoning_output_tokens: Some(0),
                provider_finish_reason: Some("stop".to_string()),
                ..Default::default()
            }),
        },
        lash_trace::TraceRetryAttempt {
            ordinal: 2,
            outcome: "completed".to_string(),
            duration_ms: 20,
            reason: None,
            delay_ms: None,
            execution_evidence: None,
        },
    ]);
    let event = TraceEvent::ToolCallCompleted {
        call_id: Some("call-1".to_string()),
        name: "retry_probe".to_string(),
        args: json!({}),
        output: TraceToolCallOutput {
            outcome: TraceToolCallOutcome::Success(json!("ok")),
            control: None,
        },
        duration_ms: 280,
        attempts,
    };
    let json = serde_json::to_value(event).expect("serialize retry ladder");
    assert_eq!(json["type"], "tool_call_completed");
    assert_eq!(json["attempts"].as_array().map(Vec::len), Some(2));
    assert_eq!(json["attempts"][0]["reason"], "http_429");
    assert_eq!(json["attempts"][0]["delay_ms"], 250);
    assert_eq!(
        json["attempts"][0]["execution_evidence"]["served_model"],
        "served-model"
    );
    assert_eq!(
        json["attempts"][0]["execution_evidence"]["reasoning_output_tokens"],
        0
    );
    assert!(json["attempts"][1].get("reason").is_none());
    assert!(json["attempts"][1].get("delay_ms").is_none());
    assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 7);
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
fn language_execution_full_shape() {
    let event = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "process:p1:finished".to_string(),
            identity: lashlang_identity(),
            payload: TraceLanguageExecutionPayload::ExecutionFinished {
                status: TraceLanguageExecutionStatus::Completed,
                error: None,
            },
        },
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "language_execution",
            "language": "lashlang",
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
fn language_execution_all_seven_payload_variants_round_trip() {
    let variants = vec![
        TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: TraceLanguageExecutionMap {
                module_ref: "module".to_string(),
                entry_kind: "process".to_string(),
                entry_ref: Some("component:0".to_string()),
                entry_name: "main".to_string(),
                nodes: vec![TraceLanguageExecutionMapNode {
                    id: "n1".to_string(),
                    kind: "resource_operation".to_string(),
                    label: "read_file".to_string(),
                    label_metadata: None,
                }],
                edges: vec![TraceLanguageExecutionMapEdge {
                    id: "e1".to_string(),
                    from: "n1".to_string(),
                    to: "n2".to_string(),
                    label: "next".to_string(),
                }],
            },
        },
        TraceLanguageExecutionPayload::ExecutionFinished {
            status: TraceLanguageExecutionStatus::Completed,
            error: Some("test error".to_string()),
        },
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "n1".to_string(),
            node_kind: "resource_operation".to_string(),
            label: "read_file".to_string(),
            occurrence: 1,
        },
        TraceLanguageExecutionPayload::NodeCompleted {
            node_id: "n1".to_string(),
            node_kind: "resource_operation".to_string(),
            label: "read_file".to_string(),
            occurrence: 1,
        },
        TraceLanguageExecutionPayload::NodeFailed {
            node_id: "n1".to_string(),
            node_kind: "resource_operation".to_string(),
            label: "read_file".to_string(),
            occurrence: 1,
            error: "failed to read file".to_string(),
        },
        TraceLanguageExecutionPayload::BranchSelected {
            node_id: "b1".to_string(),
            occurrence: 1,
            edge_id: "e1".to_string(),
            selected: TraceBranchSelection::Then,
        },
        TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id: "p_node".to_string(),
            occurrence: 1,
            child: TraceLanguageChildExecution {
                scope: TraceRuntimeScope::new("s1"),
                subject: TraceRuntimeSubject::Process {
                    process_id: "p2".to_string(),
                },
                module_ref: Some("child_mod".to_string()),
                entry_ref: Some("component:1".to_string()),
                entry_name: Some("child_main".to_string()),
            },
        },
    ];

    assert_eq!(variants.len(), 7, "must test all 7 payload variants");

    for payload in variants {
        let record = TraceRecord::new(
            TraceContext::default().for_session("s1"),
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event: TraceLanguageExecution {
                    event_key: "k1".to_string(),
                    identity: lashlang_identity(),
                    payload: payload.clone(),
                },
            },
        );

        let json = serde_json::to_value(&record).expect("serialize record");
        assert_eq!(json["type"], "language_execution");
        assert_eq!(json["event"]["event_key"], "k1");
        assert!(
            json["event"].get("kind").is_some(),
            "kind must be flattened into event envelope"
        );

        let round_tripped: TraceRecord =
            serde_json::from_value(json).expect("deserialize round trip");
        assert_eq!(record, round_tripped);
    }
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
        assert_eq!(record.schema_version, 7);
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

#[test]
fn durable_step_events_round_trip_at_schema_version_six() {
    let events = vec![
        TraceEvent::JournaledEffectStarted {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
        },
        TraceEvent::JournaledEffectSettled {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
            status: "completed".to_string(),
        },
        TraceEvent::DurableWaitParked {
            wait_kind: "await_event".to_string(),
        },
        TraceEvent::DurableWaitResolved {
            wait_kind: "await_event".to_string(),
            resolution: "ok".to_string(),
        },
        TraceEvent::DurableTimerStarted { duration_ms: 250 },
        TraceEvent::DurableTimerResolved {
            duration_ms: 250,
            status: "resolved".to_string(),
        },
        TraceEvent::DurableSegmentBoundary {
            reason: "journal_budget".to_string(),
            effects_executed: 10_000,
            journaled_bytes_estimate: None,
        },
        TraceEvent::StoreErrorObserved {
            operation: "session_restore".to_string(),
            error_class: "StoredDataCorrupt".to_string(),
            message: "stored SessionHeadMeta data is corrupt".to_string(),
        },
    ];

    for event in events {
        let expected_kind = event.kind();
        let record = TraceRecord::new(TraceContext::default().for_session("s1"), event);
        let json = serde_json::to_value(&record).expect("serialize durable trace event");
        assert_eq!(json["schema_version"], lash_trace::TRACE_SCHEMA_VERSION);
        assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 7);
        assert_eq!(json["type"], expected_kind);
        let decoded: TraceRecord = serde_json::from_value(json).expect("round trip event");
        assert_eq!(decoded.event.kind(), expected_kind);
    }
}
