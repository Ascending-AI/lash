//! Golden schema pins for the on-disk trace format.
//!
//! Trace records are a durable, cross-tool contract (JSONL files consumed by
//! trace sinks, exporters, and the OTel bridge). These tests pin the
//! `schema_version` tripwire, the `type` tag for every [`TraceEvent`] variant,
//! the full payload shape of the load-bearing variants, and a JSONL round-trip
//! carrying an `exec_code_completed` diagnostic.

use std::collections::BTreeSet;

use lash_trace::{
    TraceBranchSelection, TraceContext, TraceDurableTimerStatus, TraceDurableWaitResolution,
    TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue,
    TraceError, TraceEvent, TraceExecToolCall, TraceJournaledEffectStatus,
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLlmRequest, TraceLlmResponse,
    TraceProviderReplayDropEvent, TraceProviderReplayDropReason, TraceProviderReplayKind,
    TraceProviderRequestEvent, TraceProviderRouteIdentity, TraceProviderStreamEvent, TraceRecord,
    TraceRetryAttemptOutcome, TraceRuntimeScope, TraceRuntimeStreamEvent, TraceRuntimeSubject,
    TraceTokenUsage, TraceToolCallOutcome, TraceToolCallOutput, TraceToolCallStatus,
    TraceTurnCancellationEvidence, TraceTurnCompletionReason, TraceTurnFailureReason,
    TraceTurnOutcome,
};
use serde_json::json;

#[test]
fn trace_schema_version_is_pinned_at_14() {
    // Tripwire. This is the current on-disk trace schema version. Every reader
    // (viewer, exporter, OTel bridge) keys off it, so a change here must be a
    // deliberate, documented schema bump — see the crate-level rustdoc and the
    // `TRACE_SCHEMA_VERSION` doc comment for the bump policy. If this fails,
    // read that policy before touching the constant.
    assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 14);
}

#[test]
fn pre_frame_key_trace_schema_is_rejected_with_literal_versions() {
    assert_eq!(
        lash_trace::ensure_trace_schema_version(3),
        Err(lash_trace::TraceSchemaVersionError {
            actual: 3,
            expected: 14,
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
        "unsupported trace schema version 3; expected 14"
    );

    let stale_and_malformed = r#"{"schema_version":3,"payload":"not a current event"}"#;
    let error = serde_json::from_str::<TraceRecord>(stale_and_malformed)
        .expect_err("the version refusal must precede current-shape validation");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 3; expected 14"
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
    assert_eq!(json["schema_version"], 14);
}

#[test]
fn schema_8_exec_protocol_record_is_refused_before_old_envelope_interpretation() {
    let stored_v8 = r#"{"schema_version":8,"id":"v8-exec","timestamp":"2026-08-24T09:00:00+00:00","context":{},"type":"protocol_step","plugin_id":"runtime","payload":{"diagnostic":{"phase":"exec_code_completed","payload":{"tool_call_count":0,"tool_calls":[],"terminal_finish":null,"terminal_finish_present":false}}}}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v8)
        .expect_err("schema-8 trace records must be refused before decoding the old envelope");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 8; expected 14"
    );
}

#[test]
fn schema_9_exec_completion_record_is_refused_before_old_projection_interpretation() {
    let stored_v9 = r#"{"schema_version":9,"id":"v9-exec","timestamp":"2026-08-26T09:00:00+00:00","context":{},"type":"exec_code_completed","duration_ms":12,"output":"hello","output_chars":5,"observation_count":1,"observation_truncation":[],"error":null,"terminal_finish":null,"tool_calls":[]}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v9).expect_err(
        "schema-9 trace records must be refused before decoding the old projection field",
    );
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 9; expected 14"
    );
}

#[test]
fn schema_10_retry_attempt_record_is_refused_before_charge_safety_interpretation() {
    let stored_v10 = r#"{"schema_version":10,"id":"v10-retry","timestamp":"2026-08-29T09:00:00+00:00","context":{},"type":"llm_call_failed","error":{"message":"rate limited","retryable":true},"attempts":[{"ordinal":1,"outcome":"failed","duration_ms":10}]}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v10)
        .expect_err("schema-10 trace records must be refused before decoding charge safety");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 10; expected 14"
    );
}

#[test]
fn schema_11_retry_attempt_record_is_refused_before_attempt_evidence_interpretation() {
    let stored_v11 = r#"{"schema_version":11,"id":"v11-retry","timestamp":"2026-08-30T09:00:00+00:00","context":{},"type":"llm_call_failed","error":{"message":"rate limited","retryable":true},"attempts":[{"ordinal":1,"outcome":"failed","duration_ms":10,"charge_safety":{"outcome":"authorized","tokens_at_stake":42,"attempt_number":1}}]}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v11)
        .expect_err("schema-11 trace records must be refused before decoding version 13 evidence");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 11; expected 14"
    );
}

#[test]
fn schema_12_record_is_refused_before_attachment_event_interpretation() {
    let stored_v12 = r#"{"schema_version":12,"id":"v12","timestamp":"2026-08-30T09:00:00+00:00","context":{},"type":"session_started"}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v12)
        .expect_err("schema-12 records must be refused before decoding version 13 events");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 12; expected 14"
    );
}

#[test]
fn schema_12_llm_completion_record_is_refused_before_response_interpretation() {
    let stored_v12 = r#"{"schema_version":12,"id":"v12-completion","timestamp":"2026-08-31T09:00:00+00:00","context":{},"type":"llm_call_completed","response":{"text":"hello","duration_ms":12},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_output_tokens":0}}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v12)
        .expect_err("schema-12 trace records must be refused before decoding the response");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 12; expected 14"
    );
}

#[test]
fn schema_13_llm_completion_without_request_model_is_refused_before_response_interpretation() {
    let stored_v13 = r#"{"schema_version":13,"id":"v13-completion","timestamp":"2026-08-31T09:00:00+00:00","context":{},"type":"llm_call_completed","response":{"text":"hello","duration_ms":12},"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_output_tokens":0}}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v13)
        .expect_err("schema-13 trace records must be refused before decoding the response");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 13; expected 14"
    );
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
        TraceEvent::AttachmentDegraded {
            attachment_id: Some("attachment-id".to_string()),
            label: Some("artifact.bin".to_string()),
            media_type: Some("application/octet-stream".to_string()),
            source: lash_sansio::AttachmentMaterializationSource::Stored,
            reason: lash_sansio::AttachmentMaterializationReason::NoProviderAcceptsMimeAndSource,
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
                request_model: "request-model".to_string(),
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
        TraceEvent::ExecCodeStarted {
            code: "print(1)".to_string(),
            code_chars: 8,
        },
        exec_code_completed_event(),
        TraceEvent::ExecCodeFailed {
            error: "boom".to_string(),
        },
        TraceEvent::ObservationProjection {
            projections: Vec::new(),
        },
        TraceEvent::JournaledEffectStarted {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
        },
        TraceEvent::JournaledEffectSettled {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
            status: TraceJournaledEffectStatus::Completed,
        },
        TraceEvent::DurableWaitParked {
            wait_kind: "await_event".to_string(),
        },
        TraceEvent::DurableWaitResolved {
            wait_kind: "await_event".to_string(),
            resolution: TraceDurableWaitResolution::Ok,
        },
        TraceEvent::DurableTimerStarted { duration_ms: 250 },
        TraceEvent::DurableTimerResolved {
            duration_ms: 250,
            status: TraceDurableTimerStatus::Resolved,
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
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
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
    AttachmentDegraded => "attachment_degraded",
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
    ExecCodeStarted => "exec_code_started",
    ExecCodeCompleted => "exec_code_completed",
    ExecCodeFailed => "exec_code_failed",
    ObservationProjection => "observation_projection",
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

    assert_eq!(json["schema_version"], 14);
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
    let wire = current_wire.replacen("\"schema_version\":14", "\"schema_version\":5", 1);
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
    let wire = current_wire.replacen("\"schema_version\":14", "\"schema_version\":6", 1);
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
    let current_wire =
        serde_json::to_string(&record).expect("serialize current language-execution event");
    let wire = current_wire.replacen("\"schema_version\":14", "\"schema_version\":7", 1);
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

#[derive(Debug, PartialEq, Eq)]
enum HistoricalV7ReadError {
    UnsupportedVersion { actual: u32, expected: u32 },
    Payload(String),
}

fn read_with_historical_v7_reader(input: &str) -> Result<(), HistoricalV7ReadError> {
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| HistoricalV7ReadError::Payload(error.to_string()))?;
    let actual = value["schema_version"]
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| HistoricalV7ReadError::Payload("missing schema_version".to_string()))?;
    if actual != 7 {
        return Err(HistoricalV7ReadError::UnsupportedVersion {
            actual,
            expected: 7,
        });
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum HistoricalV7Event {
        SessionStarted,
        // v7 spread one turn outcome over three free-form fields. v8 replaces
        // them with a single closed `outcome`, which is why a v7 reader must
        // not reach this decoder for a v8 record.
        TurnCompleted {
            #[allow(dead_code)]
            status: String,
            #[allow(dead_code)]
            done_reason: String,
        },
    }

    serde_json::from_value::<HistoricalV7Event>(value)
        .map(|_| ())
        .map_err(|error| HistoricalV7ReadError::Payload(error.to_string()))
}

/// FIG-1758: the v8 bump retypes `turn_completed`. A v7 reader must refuse a
/// v8 record on its version gate, before its decoder can trip over the
/// reshaped payload.
#[test]
fn historical_v7_reader_refuses_v8_turn_outcome_before_interpreting_payload() {
    let record = TraceRecord::new(
        TraceContext::default(),
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::Cancelled {
                evidence: TraceTurnCancellationEvidence {
                    request_id: "cancel-1".to_string(),
                    origin: None,
                    reason: None,
                },
            },
        },
    );
    let current_wire = serde_json::to_string(&record).expect("serialize current turn_completed");
    let wire = current_wire.replacen("\"schema_version\":14", "\"schema_version\":8", 1);
    assert_eq!(
        read_with_historical_v7_reader(&wire),
        Err(HistoricalV7ReadError::UnsupportedVersion {
            actual: 8,
            expected: 7,
        }),
        "the v7 reader's typed version gate must run before its payload decoder"
    );

    let forced_v7 = wire.replacen("\"schema_version\":8", "\"schema_version\":7", 1);
    let error = read_with_historical_v7_reader(&forced_v7)
        .expect_err("without the version gate, the retyped payload is undecodable");
    assert!(
        matches!(&error, HistoricalV7ReadError::Payload(message) if message.contains("status")),
        "expected the missing `status` field to break a v7 decoder, got: {error:?}"
    );
}

/// FIG-1758: the other direction of the same gate — the current decoder must
/// refuse a stored v7 record on its version, before the old flat
/// `status`/`done_reason` payload reaches the typed event decoder.
#[test]
fn current_reader_refuses_a_stored_v7_turn_completed_record() {
    let stored_v7 = r#"{"schema_version":7,"id":"v7-turn","timestamp":"2026-08-19T09:12:33.101+00:00","context":{},"type":"turn_completed","status":"failed","done_reason":"cancelled"}"#;
    let error = serde_json::from_str::<TraceRecord>(stored_v7)
        .expect_err("v7 trace records must be refused by the current decoder");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 7; expected 14",
        "the version refusal must precede payload interpretation"
    );

    // The same bytes at the current version are undecodable for a second,
    // independent reason: the current schema has no flat `status`/`done_reason` pair. The
    // version gate is what keeps that shape error from ever being the message a
    // stale reader sees.
    let forced_v14 = stored_v7.replacen("\"schema_version\":7", "\"schema_version\":14", 1);
    let error = serde_json::from_str::<TraceRecord>(&forced_v14)
        .expect_err("without the version gate, the v7 payload shape is undecodable");
    assert!(
        error.to_string().contains("outcome"),
        "expected the missing `outcome` field to break the current decoder, got: {error}"
    );
}

/// FIG-1758: `turn_completed` carries exactly one closed outcome. Each variant
/// owns only the data its state can hold, so no combination can state an
/// agent-frame switch without a frame key or a cancellation without evidence.
#[test]
fn turn_completed_outcome_pins_the_closed_variant_vocabulary() {
    let cases = [
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
            json!({ "status": "completed", "done_reason": "assistant_message" }),
        ),
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::FinalValue,
            },
            json!({ "status": "completed", "done_reason": "final_value" }),
        ),
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::ToolValue,
            },
            json!({ "status": "completed", "done_reason": "tool_value" }),
        ),
        (
            TraceTurnOutcome::AgentFrameSwitch {
                frame_switch: lash_trace::TraceAgentFrameSwitch {
                    frame_key: "frame-key/v2/example".to_string(),
                },
            },
            json!({
                "status": "agent_frame_switch",
                "frame_switch": { "frame_key": "frame-key/v2/example" },
            }),
        ),
        (
            TraceTurnOutcome::Cancelled {
                evidence: TraceTurnCancellationEvidence {
                    request_id: "cancel-1".to_string(),
                    origin: Some("host-console".to_string()),
                    reason: Some("operator stopped the turn".to_string()),
                },
            },
            json!({
                "status": "cancelled",
                "evidence": {
                    "request_id": "cancel-1",
                    "origin": "host-console",
                    "reason": "operator stopped the turn",
                },
            }),
        ),
        (
            TraceTurnOutcome::Failed {
                done_reason: TraceTurnFailureReason::ProviderError,
            },
            json!({ "status": "failed", "done_reason": "provider_error" }),
        ),
    ];

    for (outcome, expected) in cases {
        let event = TraceEvent::TurnCompleted {
            outcome: outcome.clone(),
        };
        let wire = serde_json::to_value(&event).expect("serialize turn_completed");
        assert_eq!(wire["type"], "turn_completed");
        assert_eq!(wire["outcome"], expected, "wire shape for {outcome:?}");
        let decoded: TraceEvent = serde_json::from_value(wire).expect("round trip turn_completed");
        assert_eq!(decoded, event);
    }

    // Every failure reason keeps a distinct snake_case tag.
    let failure_tags = [
        TraceTurnFailureReason::Incomplete,
        TraceTurnFailureReason::InvalidInput,
        TraceTurnFailureReason::MaxTurns,
        TraceTurnFailureReason::ToolFailure,
        TraceTurnFailureReason::ProviderError,
        TraceTurnFailureReason::PluginAbort,
        TraceTurnFailureReason::RuntimeError,
        TraceTurnFailureReason::SubmittedError,
        TraceTurnFailureReason::ToolError,
    ]
    .map(|reason| {
        let wire = serde_json::to_value(reason).expect("serialize failure reason");
        assert_eq!(wire, json!(reason.wire_tag()));
        reason.wire_tag()
    });
    assert_eq!(
        failure_tags.iter().collect::<BTreeSet<_>>().len(),
        failure_tags.len(),
        "failure reasons must not collide"
    );
}

/// FIG-1758: `done_reason_tag()` reproduces exactly the spelling the pre-v8
/// flat `done_reason` string carried, so a host that read it as text keeps a
/// supported way to do so after the retyping.
#[test]
fn done_reason_tag_reproduces_the_pre_v8_reason_spellings() {
    let cases = [
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
            "assistant_message",
        ),
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::FinalValue,
            },
            "final_value",
        ),
        (
            TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::ToolValue,
            },
            "tool_value",
        ),
        (
            // v7 reported a frame switch as done_reason="agent_frame_switch".
            TraceTurnOutcome::AgentFrameSwitch {
                frame_switch: lash_trace::TraceAgentFrameSwitch {
                    frame_key: "frame-key/v2/example".to_string(),
                },
            },
            "agent_frame_switch",
        ),
        (
            // v7 reported a cancellation as done_reason="cancelled".
            TraceTurnOutcome::Cancelled {
                evidence: TraceTurnCancellationEvidence {
                    request_id: "cancel-1".to_string(),
                    origin: None,
                    reason: None,
                },
            },
            "cancelled",
        ),
        (
            TraceTurnOutcome::Failed {
                done_reason: TraceTurnFailureReason::ProviderError,
            },
            "provider_error",
        ),
    ];

    for (outcome, expected) in cases {
        assert_eq!(
            outcome.done_reason_tag(),
            expected,
            "done_reason tag for {outcome:?}"
        );
    }
}

/// FIG-1758: only a `failed` outcome satisfies the shared failure predicate.
/// A cancelled turn is a deliberate stop and must not be counted as a failure.
#[test]
fn cancelled_turn_outcome_is_not_a_failure() {
    let cancelled = TraceEvent::TurnCompleted {
        outcome: TraceTurnOutcome::Cancelled {
            evidence: TraceTurnCancellationEvidence {
                request_id: "cancel-1".to_string(),
                origin: None,
                reason: None,
            },
        },
    };
    assert!(!cancelled.is_failed());
    assert!(
        !TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::AgentFrameSwitch {
                frame_switch: lash_trace::TraceAgentFrameSwitch {
                    frame_key: "frame-key/v2/example".to_string(),
                },
            },
        }
        .is_failed()
    );
    assert!(
        TraceEvent::TurnCompleted {
            outcome: TraceTurnOutcome::Failed {
                done_reason: TraceTurnFailureReason::RuntimeError,
            },
        }
        .is_failed()
    );
}

/// FIG-1758: the three durable outcome payloads are closed enums, and the
/// shared failure predicate matches on the variants rather than on a `"failed"`
/// string.
#[test]
fn durable_outcome_statuses_are_closed_enums() {
    for (status, tag, failed) in [
        (TraceJournaledEffectStatus::Completed, "completed", false),
        (TraceJournaledEffectStatus::Failed, "failed", true),
    ] {
        let event = TraceEvent::JournaledEffectSettled {
            effect_name: "lash:turn:llm:1".to_string(),
            effect_kind: "llm_call".to_string(),
            status,
        };
        let wire = serde_json::to_value(&event).expect("serialize settled effect");
        assert_eq!(wire["status"], tag);
        assert_eq!(event.is_failed(), failed);
    }

    for (resolution, tag, failed) in [
        (TraceDurableWaitResolution::Ok, "ok", false),
        (TraceDurableWaitResolution::Error, "error", false),
        (TraceDurableWaitResolution::Timeout, "timeout", false),
        (TraceDurableWaitResolution::Cancelled, "cancelled", false),
        (TraceDurableWaitResolution::Resolved, "resolved", false),
        (
            TraceDurableWaitResolution::TurnCancelled,
            "turn_cancelled",
            false,
        ),
        (
            TraceDurableWaitResolution::SessionRevoked,
            "session_revoked",
            false,
        ),
        (TraceDurableWaitResolution::Failed, "failed", true),
    ] {
        let event = TraceEvent::DurableWaitResolved {
            wait_kind: "await_event".to_string(),
            resolution,
        };
        let wire = serde_json::to_value(&event).expect("serialize resolved wait");
        assert_eq!(wire["resolution"], tag);
        assert_eq!(event.is_failed(), failed);
    }

    for (status, tag, failed) in [
        (TraceDurableTimerStatus::Resolved, "resolved", false),
        (TraceDurableTimerStatus::Cancelled, "cancelled", false),
        (
            TraceDurableTimerStatus::SessionRevoked,
            "session_revoked",
            false,
        ),
        (TraceDurableTimerStatus::Failed, "failed", true),
    ] {
        let event = TraceEvent::DurableTimerResolved {
            duration_ms: 250,
            status,
        };
        let wire = serde_json::to_value(&event).expect("serialize resolved timer");
        assert_eq!(wire["status"], tag);
        assert_eq!(event.is_failed(), failed);
    }
}

/// FIG-1635: the execution map no longer restates the identity fields. After
/// the removal, the graph's identity is readable only from `identity`, so the
/// two copies can never diverge in a durable record.
#[test]
fn execution_started_map_carries_no_identity_copy() {
    let event = TraceEvent::LanguageExecution {
        language: "lashlang".to_string(),
        event: TraceLanguageExecution {
            event_key: "process:p1:started".to_string(),
            identity: lashlang_identity(),
            payload: TraceLanguageExecutionPayload::ExecutionStarted {
                execution_map: TraceLanguageExecutionMap {
                    nodes: vec![TraceLanguageExecutionMapNode {
                        id: "n1".to_string(),
                        kind: "resource_operation".to_string(),
                        label: "read_file".to_string(),
                        label_metadata: None,
                    }],
                    edges: Vec::new(),
                },
            },
        },
    };
    let wire = serde_json::to_value(&event).expect("serialize execution_started");
    assert_eq!(
        wire,
        json!({
            "type": "language_execution",
            "language": "lashlang",
            "event": {
                "kind": "execution_started",
                "event_key": "process:p1:started",
                "identity": {
                    "scope": { "session_id": "s1" },
                    "subject": { "type": "process", "process_id": "p1" },
                    "module_ref": "module",
                    "entry_kind": "process",
                    "entry_ref": "component:0",
                    "entry_name": "main",
                },
                "execution_map": {
                    "nodes": [{
                        "id": "n1",
                        "kind": "resource_operation",
                        "label": "read_file",
                    }],
                    "edges": [],
                },
            },
        })
    );

    let map = &wire["event"]["execution_map"];
    for field in ["module_ref", "entry_kind", "entry_ref", "entry_name"] {
        assert!(
            map.get(field).is_none(),
            "the execution map must not restate `{field}`; identity owns it"
        );
        assert!(
            wire["event"]["identity"].get(field).is_some(),
            "identity must remain the single home of `{field}`"
        );
    }
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
            request_model: "request-model".to_string(),
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
                "request_model": "request-model",
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
            outcome: TraceRetryAttemptOutcome::Failed,
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
            charge_safety: Some(lash_trace::TraceChargeSafetyDecision::Authorized {
                tokens_at_stake: 42,
                attempt_number: 1,
            }),
            generation_disposition: Some(lash_trace::GenerationReceipt {
                output_token_cap: lash_sansio::llm::types::GenerationOptionOutcome::Applied,
                ..Default::default()
            }),
            usage: Some(token_usage_sample()),
        },
        lash_trace::TraceRetryAttempt {
            ordinal: 2,
            outcome: TraceRetryAttemptOutcome::Completed,
            duration_ms: 20,
            reason: None,
            delay_ms: None,
            execution_evidence: None,
            charge_safety: None,
            generation_disposition: None,
            usage: None,
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
        json["attempts"][0]["charge_safety"]["outcome"],
        "authorized"
    );
    assert_eq!(json["attempts"][0]["charge_safety"]["tokens_at_stake"], 42);
    assert_eq!(json["attempts"][0]["charge_safety"]["attempt_number"], 1);
    assert_eq!(
        json["attempts"][0]["generation_disposition"]["output_token_cap"],
        "applied"
    );
    assert_eq!(json["attempts"][0]["usage"], json!(token_usage_sample()));
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
    assert!(json["attempts"][1].get("generation_disposition").is_none());
    assert!(json["attempts"][1].get("usage").is_none());
    assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 14);
}

#[test]
fn typed_exec_code_completed_full_shape() {
    let event = exec_code_completed_event();
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["type"], "exec_code_completed");
    assert_eq!(
        json["tool_calls"],
        json!([{
            "call_id": "call-1",
            "name": "read_file",
            "duration_ms": 5,
            "status": "success",
        }])
    );
    assert!(json.get("tool_call_count").is_none());
    assert!(json.get("terminal_finish_present").is_none());
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
    // This pin compares parsed `serde_json::Value`s and is key-order-insensitive.
    // The envelope hoist moved the `kind` key from first to third while keeping
    // identical keys and values; this `json!` literal is not a byte-level pin.
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

fn exec_code_completed_event() -> TraceEvent {
    TraceEvent::ExecCodeCompleted {
        duration_ms: 12,
        output: "hello\nworld".to_string(),
        output_chars: 11,
        observation_count: 2,
        observation_projections: Vec::new(),
        error: None,
        terminal_finish: None,
        tool_calls: vec![TraceExecToolCall {
            call_id: Some("call-1".to_string()),
            name: "read_file".to_string(),
            duration_ms: 5,
            status: TraceToolCallStatus::Success,
        }],
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
            exec_code_completed_event(),
        ),
        TraceRecord::new(
            TraceContext::default().for_session("root"),
            TraceEvent::TurnCompleted {
                outcome: TraceTurnOutcome::Completed {
                    done_reason: TraceTurnCompletionReason::AssistantMessage,
                },
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
        assert_eq!(record.schema_version, 14);
    }

    // Pin the diagnostic's `tool_calls` entry fields explicitly on the parsed
    // line, independent of the Rust construction above.
    let diagnostic_line = jsonl
        .lines()
        .find(|line| line.contains("exec_code_completed"))
        .expect("exec diagnostic line present");
    let value: serde_json::Value =
        serde_json::from_str(diagnostic_line).expect("parse diagnostic line");
    let tool_call = &value["tool_calls"][0];
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
            status: TraceJournaledEffectStatus::Completed,
        },
        TraceEvent::DurableWaitParked {
            wait_kind: "await_event".to_string(),
        },
        TraceEvent::DurableWaitResolved {
            wait_kind: "await_event".to_string(),
            resolution: TraceDurableWaitResolution::Ok,
        },
        TraceEvent::DurableTimerStarted { duration_ms: 250 },
        TraceEvent::DurableTimerResolved {
            duration_ms: 250,
            status: TraceDurableTimerStatus::Resolved,
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
        assert_eq!(lash_trace::TRACE_SCHEMA_VERSION, 14);
        assert_eq!(json["type"], expected_kind);
        let decoded: TraceRecord = serde_json::from_value(json).expect("round trip event");
        assert_eq!(decoded.event.kind(), expected_kind);
    }
}
