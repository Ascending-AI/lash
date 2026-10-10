use lash_sansio::llm::types::{
    LlmProviderTraceDirection, LlmUsage, StreamBlockEvent, StreamBlockIdentity, StreamBlockKind,
};
use lash_trace::{
    TraceBranchSelection, TraceContext, TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent,
    TraceEffectEnvelopeDiffValue, TraceError, TraceEvent, TraceEventKind, TraceExecToolCall,
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLlmRequest, TraceLlmResponse,
    TraceProviderEvent, TraceProviderReplayDropEvent, TraceProviderReplayDropReason,
    TraceProviderReplayKind, TraceProviderRouteIdentity, TraceRecord, TraceRuntimeScope,
    TraceRuntimeStreamEvent, TraceRuntimeStreamPayload, TraceRuntimeSubject, TraceToolCallOutcome,
    TraceToolCallOutput, TraceToolCallStatus, TraceTurnCompletionReason, TraceTurnOutcome,
};
use serde_json::json;

#[test]
fn trace_error_refuses_unknown_failure_and_terminal_classes() {
    for field in ["failure_kind", "terminal_reason"] {
        let mut bytes = json!({
            "retryable": false,
            "terminal_reason": "provider_error",
            "failure_kind": "http"
        });
        bytes[field] = json!("rate_limited");
        assert!(
            serde_json::from_value::<TraceError>(bytes).is_err(),
            "{field}"
        );
    }
}

#[test]
fn trace_retry_attempt_refuses_the_shared_llm_tool_shape() {
    let bytes = json!({
        "ordinal": 1,
        "outcome": "cancelled",
        "usage_disposition": "unreported_after_failure"
    });
    assert!(serde_json::from_value::<lash_trace::TraceRetryAttempt>(bytes).is_err());
}

#[test]
fn node_failure_requires_typed_provenance() {
    let legacy = json!({
        "kind": "node",
        "at": {
            "task": "main",
            "site": { "unit": { "function": "node-1" }, "path": [] },
            "occurrence": 1
        },
        "fact": { "kind": "failed", "error": "permission denied" }
    });
    let refusal = serde_json::from_value::<TraceLanguageExecutionPayload>(legacy)
        .expect_err("the former string-only node failure must be refused");
    assert!(refusal.to_string().contains("failure"), "{refusal}");

    let payload = TraceLanguageExecutionPayload::Node {
        at: lash_sansio::effect_identity_fixture("node-1", 1),
        fact: lash_trace::TraceNodeFact::Failed {
            call_id: Some(lash_sansio::ToolCallId::fixture("effect-1")),
            failure: lash_trace::TraceLanguageExecutionFailure::Effect {
                class: lash_sansio::ToolFailureClass::PermissionDenied,
                code: "approval_denied".to_owned(),
                message: "permission denied".to_owned(),
                replay_key: "effect-1".to_owned(),
                source: lash_sansio::ToolFailureSource::Policy,
                suggested_delay_ms: None,
            },
        },
    };
    let wire = serde_json::to_value(&payload).expect("encode typed failure");
    assert_eq!(
        wire["fact"]["failure"],
        json!({
            "kind": "effect",
            "class": "permission_denied",
            "code": "approval_denied",
            "message": "permission denied",
            "replay_key": "effect-1",
            "source": "policy",
            "suggested_delay_ms": null
        })
    );
    assert!(wire.get("error").is_none());
    let mut unknown_kind = wire.clone();
    unknown_kind["fact"]["failure"]["kind"] = json!("unknown");
    assert!(serde_json::from_value::<TraceLanguageExecutionPayload>(unknown_kind).is_err());
    let mut unknown_class = wire.clone();
    unknown_class["fact"]["failure"]["class"] = json!("unknown");
    assert!(serde_json::from_value::<TraceLanguageExecutionPayload>(unknown_class).is_err());
    assert_eq!(
        serde_json::from_value::<TraceLanguageExecutionPayload>(wire).expect("decode failure"),
        payload
    );
}

#[test]
fn documented_trace_record_decode_rejects_schema_3_before_payload_interpretation() {
    let otherwise_current = r#"{"schema_version":3,"id":"legacy-record","timestamp":"2026-05-11T11:42:01.234+00:00","context":{},"type":"turn_started"}"#;
    let error = serde_json::from_str::<TraceRecord>(otherwise_current)
        .expect_err("schema-3 trace records must be refused during typed decode");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 3; expected 36"
    );

    let stale_and_malformed = r#"{"schema_version":3,"payload":"not a current event"}"#;
    let error = serde_json::from_str::<TraceRecord>(stale_and_malformed)
        .expect_err("the version refusal must precede current-shape validation");
    assert_eq!(
        error.to_string(),
        "unsupported trace schema version 3; expected 36"
    );
}

fn token_usage_sample() -> LlmUsage {
    LlmUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 3,
    }
}

fn lash_vm_identity() -> TraceLanguageExecutionIdentity {
    TraceLanguageExecutionIdentity {
        scope: TraceRuntimeScope::new("s1"),
        subject: TraceRuntimeSubject::Process {
            process_id: lash_sansio::ProcessId::fixture("p1"),
        },
        document: lash_trace::WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([1; 32]),
            entry: lash_trace::WorkflowDocumentEntry::Entry {
                function: lash_kernel_doc::Name::new("worker"),
            },
        },
        entry_name: "main".to_string(),
        engine_execution_id: None,
        generation: None,
    }
}

#[test]
fn known_trace_event_tolerates_unknown_field() {
    let record = fixture_record(
        TraceContext::default(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    );
    let mut value = serde_json::to_value(&record).expect("encode record");
    value["future_field"] = json!(true);
    assert_eq!(
        serde_json::from_value::<TraceRecord>(value).expect("additive record field"),
        record
    );
}

fn event_samples() -> Vec<TraceEvent> {
    vec![
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
        TraceEvent::PromptBuilt {
            prompt_hash: "h".to_string(),
            prompt_chars: 12,
            components: Vec::new(),
        },
        TraceEvent::PromptCompositionFailed {
            plan: Some(json!({ "purpose": "turn", "sections": [] })),
            limits: json!({ "render_budget_ms": 2000 }),
            error: json!({ "kind": "total_too_large", "bytes": 300_000, "limit": 262_144 }),
            elapsed_ms: 3,
        },
        TraceEvent::AttachmentDegraded {
            attachment_id: Some("attachment-id".to_string()),
            label: Some("artifact.bin".to_string()),
            media_type: Some("application/octet-stream".to_string()),
            position: lash_sansio::llm::attachment_delivery::AttachmentPosition::Message,
            reason: lash_sansio::AttachmentMaterializationReason::NoProviderAcceptsMimeAndPosition,
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
        TraceEvent::CompactionNeeded {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            threshold_tokens: 20_000,
        },
        TraceEvent::PromptViewAttachmentsPruned {
            used_tokens: 30_000,
            max_context_tokens: 40_000,
            pruned_attachments: 9,
        },
        TraceEvent::CompactionStarted {
            source_messages: 3,
            instructions_present: true,
        },
        TraceEvent::CompactionCompleted { summary_nodes: 1 },
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                model: "m".to_string(),
                model_variant: Default::default(),
                messages: Vec::new(),
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
                terminal_reason: Some(lash_trace::TraceLlmTerminalReason::Stop),
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
                retryable: true,
                terminal_reason: lash_trace::TraceLlmTerminalReason::Unknown,
                failure_kind: lash_trace::TraceProviderFailureKind::Unknown,
                code: None,
            },
            stream_summary: None,
            attempts: None,
        },
        TraceEvent::LlmAttemptCompleted {
            attempt: lash_sansio::llm::types::AttemptRecord {
                ordinal: 1,
                outcome: lash_sansio::llm::types::AttemptOutcome::Completed,
                protocol_position: lash_sansio::llm::types::ProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
            },
            observation: lash_trace::TraceAttemptObservation {
                provider: Some("test".to_string()),
                request_model: "m".to_string(),
                started_at_ms: Some(1),
                ended_at_ms: Some(2),
            },
        },
        TraceEvent::DomainCompleted {
            completion: lash_trace::TraceDomainCompletion::new(
                lash_trace::TraceDomainSubject::Process {},
                1,
                lash_trace::TraceDomainStatus::Completed,
            ),
        },
        TraceEvent::ProviderEvent {
            event: TraceProviderEvent {
                provider: "test".to_string(),
                sequence: 0,
                elapsed_ms: 0,
                direction: LlmProviderTraceDirection::Request {
                    endpoint: "chat/completions".to_string(),
                },
                item_id: None,
                output_index: None,
                raw_len: 13,
                raw_sha256: "abcd".to_string(),
                raw_json: Some(json!({ "model": "m" })),
                raw_json_omitted_reason: None,
            },
        },
        TraceEvent::ProviderEvent {
            event: TraceProviderEvent {
                provider: "test".to_string(),
                sequence: 1,
                elapsed_ms: 0,
                direction: LlmProviderTraceDirection::Response {
                    event_name: "delta".to_string(),
                },
                item_id: None,
                output_index: None,
                raw_len: 4,
                raw_sha256: "abcd".to_string(),
                raw_json: None,
                raw_json_omitted_reason: Some(lash_trace::TraceProviderBodyOmission::InvalidJson),
            },
        },
        TraceEvent::StepBodyStarted {
            step: step_body_started(),
        },
        TraceEvent::ToolCheckConflict {
            plugin_id: "alpha".to_string(),
            conflict: lash_sansio::ToolCheckConflict {
                phase: lash_sansio::ToolCheckPhase::ToolArgsCheck,
                winner: lash_sansio::ToolCheckReply {
                    plugin_id: "alpha".to_string(),
                    callback: "tool_args_check:check".to_string(),
                    verdict: lash_sansio::ToolCheckVerdictKind::Deny,
                },
                displaced: Vec::new(),
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
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 1,
                elapsed_ms: 0,
                payload: TraceRuntimeStreamPayload::Block {
                    event: StreamBlockEvent::delta(
                        StreamBlockKind::Reasoning,
                        StreamBlockIdentity::new("rs_1:summary:1", 1)
                            .with_item_id(Some("rs_1".to_string())),
                        "thinking",
                    ),
                    raw_text: None,
                },
            },
        },
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 2,
                elapsed_ms: 1,
                payload: TraceRuntimeStreamPayload::ToolCallPart {
                    call_id: "call-1".to_string(),
                    tool_name: "search".to_string(),
                    input_json: json!({ "q": "x" }),
                    item_id: None,
                },
            },
        },
        TraceEvent::ToolCallStarted {
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            name: "read_file".to_string(),
            args: json!({ "path": "README.md" }),
            issuing_node_id: None,
        },
        TraceEvent::ToolCallCompleted {
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            name: "read_file".to_string(),
            args: json!({ "path": "README.md" }),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Success(json!("ok")),
                control: None,
            },
            duration_ms: 3,
            issuing_node_id: None,
            attempts: None,
        },
        TraceEvent::ExecCodeStarted {
            code: "print(1)".to_string(),
            code_chars: 8,
        },
        exec_code_completed_event(),
        TraceEvent::ExecCodeFailed {
            reason: lash_trace::ExecCodeFailureReason::RuntimeStopped,
            error: "boom".to_string(),
        },
        TraceEvent::ObservationProjection {
            projections: Vec::new(),
        },
        TraceEvent::StoreErrorObserved {
            operation: "session_restore".to_string(),
            error_class: lash_trace::TraceStoreErrorClass::StoredDataCorrupt,
            message: "stored SessionHeadMeta data is corrupt".to_string(),
        },
        TraceEvent::ProgramStep {
            step_index: 1,
            outcome: lash_trace::TraceProgramStepOutcome::Ok,
        },
        TraceEvent::ProtocolStep {
            plugin_id: "custom".to_string(),
            payload: json!({ "code": "print 1" }),
        },
        TraceEvent::LanguageExecution {
            language: Some("lashvm".to_string()),
            event: TraceLanguageExecution {
                event_key: "process:p1:finished".to_string(),
                identity: lash_vm_identity(),
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

/// Core tracing is seam-neutral: no event names the plugin, durable
/// substrate, store backend or provider that produced it. Those
/// implementations report through the shared vocabulary.
#[test]
fn trace_event_vocabulary_names_no_seam_implementation() {
    const IMPLEMENTATIONS: &[&str] = &[
        "rlm",
        "lashvm",
        "temporal",
        "sqlite",
        "postgres",
        "anthropic",
        "openai",
    ];
    for event in event_samples() {
        let json = serde_json::to_value(&event).expect("serialize event");
        let tag = json["type"].as_str().expect("event type tag").to_owned();
        let variant = format!("{event:?}")
            .split([' ', '{', '('])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        for name in IMPLEMENTATIONS {
            assert!(
                !tag.contains(name) && !variant.contains(name),
                "trace event `{tag}` names the seam implementation `{name}`"
            );
        }
    }
}

/// FIG-2362: `exec_code_failed` carries a closed `reason` code beside the
/// human `error` text so offline analysis never string-matches prose.
#[test]
fn exec_code_failed_carries_a_closed_reason_code() {
    let event = TraceEvent::ExecCodeFailed {
        reason: lash_trace::ExecCodeFailureReason::ExecutorUnavailable,
        error: "code execution is not available in this session".to_string(),
    };

    let json = serde_json::to_value(&event).expect("serialize exec failure");
    assert_eq!(json["type"], "exec_code_failed");
    assert_eq!(json["reason"], "executor_unavailable");
    assert_eq!(
        json["error"],
        "code execution is not available in this session"
    );

    let decoded: TraceEvent = serde_json::from_value(json).expect("round-trip the typed reason");
    assert_eq!(decoded, event);

    let reasonless = serde_json::json!({
        "type": "exec_code_failed",
        "error": "boom",
    });
    serde_json::from_value::<TraceEvent>(reasonless)
        .expect_err("a missing reason is refused, not defaulted");

    let unknown_reason = serde_json::json!({
        "type": "exec_code_failed",
        "reason": "executor_went_to_lunch",
        "error": "boom",
    });
    serde_json::from_value::<TraceEvent>(unknown_reason)
        .expect_err("a reason outside the closed set is refused");
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
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            name: "read_file".to_string(),
            status: TraceToolCallStatus::Success,
        }],
    }
}

fn published_schema(document: &str) -> Result<jsonschema::Validator, String> {
    let schema: serde_json::Value = serde_json::from_str(document)
        .map_err(|error| format!("published trace schema does not parse: {error}"))?;
    jsonschema::validator_for(&schema)
        .map_err(|error| format!("published trace schema does not compile: {error}"))
}

fn assert_schema_accepts(validator: &jsonschema::Validator, value: &serde_json::Value, what: &str) {
    if !validator.is_valid(value) {
        let errors = validator.iter_errors(value);
        panic!(
            "published schema rejected {what}:\n{}",
            errors
                .map(|error| format!("{} at {}", error, error.instance_path()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// One language-execution payload of every kind, in the order an execution
/// reports them: its start, then the observed sites, then its finish.
fn language_execution_payload_samples() -> Vec<TraceLanguageExecutionPayload> {
    vec![
        TraceLanguageExecutionPayload::ExecutionStarted,
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::Started { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::BranchSelected {
                selected: TraceBranchSelection::Then,
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::Completed { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Started {
                call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Waiting {
                awaited: lash_trace::TraceNodeAwaited::Signal {
                    name: "approved".to_string(),
                    key: "approved:1".to_string(),
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Resumed {
                resolution: lash_trace::TraceNodeWaitResolution::Resumed,
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::ChildStarted {
                child: TraceLanguageChildExecution {
                    scope: TraceRuntimeScope::new("s1"),
                    process_id: lash_sansio::ProcessId::fixture("child-1"),
                    attempt: Some(1),
                    document: None,
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Failed {
                call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
                failure: lash_trace::TraceLanguageExecutionFailure::Runtime {
                    code: "boom".to_string(),
                    message: "notify failed".to_string(),
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 2),
            fact: lash_trace::TraceNodeFact::Started { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("then", 2),
            fact: lash_trace::TraceNodeFact::Cancelled,
        },
        TraceLanguageExecutionPayload::ExecutionFinished {
            status: TraceLanguageExecutionStatus::Failed,
            error: Some("notify failed".to_string()),
        },
    ]
}

fn language_execution_records() -> Vec<TraceRecord> {
    language_execution_payload_samples()
        .into_iter()
        .enumerate()
        .map(|(index, payload)| {
            fixture_record(
                TraceContext::default().for_session("s1"),
                TraceEvent::LanguageExecution {
                    language: (index % 2 == 0).then(|| "typescript".to_string()),
                    event: TraceLanguageExecution {
                        event_key: format!("process:p1:{index}"),
                        identity: lash_vm_identity(),
                        payload,
                    },
                },
            )
        })
        .collect()
}

/// Every emitted serde tag must be recognised by the reader's derived vocabulary.
#[test]
fn every_trace_event_serde_tag_round_trips_through_its_kind() {
    use std::collections::HashSet;
    use strum::VariantArray;

    let mut sampled_kinds = HashSet::new();
    for event in event_samples() {
        let wire = serde_json::to_value(&event).expect("encode event");
        let tag = wire["type"].as_str().expect("event type tag");
        let kind = tag
            .parse::<TraceEventKind>()
            .expect("recognise emitted tag");
        assert_eq!(kind, event.kind());
        assert_eq!(kind.as_str(), tag);
        sampled_kinds.insert(kind);
    }
    assert_eq!(
        sampled_kinds,
        TraceEventKind::VARIANTS.iter().copied().collect(),
        "every event variant needs a serde round-trip sample"
    );
}

#[test]
fn published_trace_record_schema_accepts_every_event_and_payload_sample() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/trace-record/v36.schema.json"
    ))
    .expect("published trace schema");
    let context = TraceContext {
        run_id: Some("run".to_string()),
        graph_node_id: Some("turn:s1:t1".to_string()),
        turn_index: Some(0),
        metadata: [("k".to_string(), json!(1))].into_iter().collect(),
        ..TraceContext::default()
            .for_session("s1")
            .for_turn("t1")
            .for_llm_call("llm-1")
    };
    for event in event_samples() {
        let kind = event.kind();
        let record = fixture_record(context.clone(), event);
        let value = serde_json::to_value(&record).expect("encode record");
        assert_schema_accepts(&validator, &value, kind.as_str());
    }
    for record in language_execution_records() {
        let value = serde_json::to_value(&record).expect("encode record");
        assert_schema_accepts(&validator, &value, "a language execution record");
    }
}

/// The published record schema enforces the matrix row: an additive field on a
/// known record is tolerated, while an unknown event tag or a predecessor
/// version is refused.
#[test]
fn published_trace_record_schema_tolerates_additive_fields_and_refuses_unknown_variants() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/trace-record/v36.schema.json"
    ))
    .expect("published trace schema");
    let record = fixture_record(
        TraceContext::default(),
        TraceEvent::TurnStarted {
            metadata: Default::default(),
        },
    );
    let mut value = serde_json::to_value(&record).expect("encode record");
    value["future_field"] = json!(true);
    assert_schema_accepts(&validator, &value, "an additive record field");

    let mut unknown = value.clone();
    unknown["type"] = json!("future_event");
    assert!(
        !validator.is_valid(&unknown),
        "unknown event tag must be refused"
    );

    let mut predecessor = value;
    predecessor["schema_version"] = json!(lash_trace::TRACE_SCHEMA_VERSION - 1);
    assert!(
        !validator.is_valid(&predecessor),
        "predecessor schema version must be refused"
    );
}

#[test]
fn published_overlay_schema_accepts_a_folded_overlay_and_enforces_its_row() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/workflow-execution-overlay/v36.schema.json"
    ))
    .expect("published overlay schema");
    let mut records = language_execution_records();
    records.push(fixture_record(
        TraceContext::default().for_session("s1"),
        TraceEvent::StepBodyStarted {
            step: step_body_started(),
        },
    ));
    // One site of the two the execution touched is the document's; the
    // other is reported as a mismatch.
    let document = lash_trace::WorkflowOverlayDocument::new(
        lash_trace::WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([2; 32]),
            entry: lash_trace::WorkflowDocumentEntry::Main,
        },
        [lash_sansio::effect_identity_fixture("then", 1).site],
    );
    let overlay = lash_trace::fold_workflow_overlay(None, Some(&document), &records, 1)
        .expect("fold every payload kind");
    assert!(!overlay.sites.is_empty() && !overlay.history.is_empty());
    assert!(!overlay.retention.is_empty() && !overlay.children.is_empty());
    assert_eq!(overlay.mismatches.len(), 2);
    assert!(overlay.sites[0].state.call.is_some());
    let mut value = serde_json::to_value(&overlay).expect("encode overlay");
    assert_schema_accepts(&validator, &value, "a folded overlay");

    value["future_field"] = json!(true);
    assert_schema_accepts(&validator, &value, "an additive overlay field");
    for (field, pointer) in [
        ("execution status", "/status"),
        ("site status", "/sites/0/status"),
        ("mismatch kind", "/mismatches/0/kind"),
        ("history fact", "/history/0/fact"),
        ("history identity", "/history/0/identity/of"),
        ("document binding", "/document/state"),
    ] {
        let mut changed = value.clone();
        *changed.pointer_mut(pointer).expect(field) = json!("future_variant");
        assert!(
            !validator.is_valid(&changed),
            "unknown {field} variant must be refused"
        );
    }
}

fn step_body_started() -> lash_trace::StepBodyStarted {
    lash_trace::StepBodyStarted {
        process_id: lash_sansio::ProcessId::fixture("p1"),
        at: lash_sansio::effect_identity_fixture("then", 2),
        call_id: lash_sansio::ToolCallId::fixture("call-2"),
        attempt: 1,
    }
}

#[cfg(test)]
fn fixture_record(
    context: lash_trace::TraceContext,
    event: lash_trace::TraceEvent,
) -> lash_trace::TraceRecord {
    lash_trace::TraceRecord {
        schema_version: lash_trace::TRACE_SCHEMA_VERSION,
        id: "fixture-record".into(),
        timestamp: Default::default(),
        content: lash_trace::TelemetryContent::Captured,
        context,
        event,
    }
}

/// The text every content field of [`content_bearing_events`] holds.
const CONTENT: &str = "CONTENT-MARKER";

/// One event of every kind that can carry content, with [`CONTENT`] in each
/// content field and nowhere else.
fn content_bearing_events() -> Vec<TraceEvent> {
    let tool_spec = || lash_trace::TraceToolSpec {
        name: "search".to_string(),
        description: CONTENT.to_string(),
        input_schema: json!({ "description": CONTENT }),
        output_schema: json!({ "description": CONTENT }),
    };
    let failed_tool_attempt = || {
        Some(vec![lash_trace::TraceRetryAttempt {
            ordinal: 1,
            delay_ms: None,
            detail: lash_trace::TraceRetryAttemptDetail::Tool {
                outcome: lash_trace::TraceToolAttemptOutcome::Failed {
                    class: lash_sansio::ToolFailureClass::Execution,
                    code: "tool_failed".to_string(),
                    message: CONTENT.to_string(),
                    source: lash_sansio::ToolFailureSource::Tool,
                    suggested_delay_ms: None,
                },
            },
        }])
    };
    let language = |payload| TraceEvent::LanguageExecution {
        language: Some("lashvm".to_string()),
        event: TraceLanguageExecution {
            event_key: "process:p1:event".to_string(),
            identity: lash_vm_identity(),
            payload,
        },
    };
    vec![
        TraceEvent::AttachmentDegraded {
            attachment_id: Some("attachment-id".to_string()),
            label: Some(CONTENT.to_string()),
            media_type: Some("application/octet-stream".to_string()),
            position: lash_sansio::llm::attachment_delivery::AttachmentPosition::Message,
            reason: lash_sansio::AttachmentMaterializationReason::NoProviderAcceptsMimeAndPosition,
        },
        TraceEvent::CompositionChanged {
            fingerprint: "composition-sha".to_string(),
            rendered_system_prompt: CONTENT.to_string(),
            tool_schemas: vec![tool_spec()],
        },
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                model: "m".to_string(),
                model_variant: None,
                messages: vec![lash_trace::TraceLlmMessage {
                    role: "user".to_string(),
                    blocks: vec![lash_trace::TraceContentBlock::Text {
                        text: CONTENT.to_string(),
                        cache_breakpoint: false,
                    }],
                }],
                tools: vec![tool_spec()],
                tool_choice: "auto".to_string(),
                output_spec: Some(json!({ "type": "json_schema", "name": CONTENT })),
                stream: false,
            },
        },
        TraceEvent::LlmCallCompleted {
            response: TraceLlmResponse {
                text: CONTENT.to_string(),
                duration_ms: 12,
                request_model: "request-model".to_string(),
                terminal_reason: Some(lash_trace::TraceLlmTerminalReason::Stop),
                parts: Some(vec![lash_sansio::llm::types::LlmOutputPart::Text {
                    text: CONTENT.to_string(),
                    response_meta: None,
                }]),
                generation_disposition: None,
            },
            usage: Some(token_usage_sample()),
            provider_usage: None,
            stream_summary: None,
            attempts: None,
        },
        TraceEvent::ProviderEvent {
            event: TraceProviderEvent {
                provider: "test".to_string(),
                sequence: 0,
                elapsed_ms: 0,
                direction: LlmProviderTraceDirection::Request {
                    endpoint: "chat/completions".to_string(),
                },
                item_id: None,
                output_index: None,
                raw_len: 13,
                raw_sha256: "abcd".to_string(),
                raw_json: Some(json!({ "input": CONTENT })),
                raw_json_omitted_reason: None,
            },
        },
        TraceEvent::EffectEnvelopeDiff {
            event: TraceEffectEnvelopeDiffEvent {
                recorded_envelope_hash: "old".to_string(),
                reconstructed_envelope_hash: "new".to_string(),
                divergent_paths: vec![TraceEffectEnvelopeDiffEntry {
                    path: "command.input.value".to_string(),
                    recorded: TraceEffectEnvelopeDiffValue::Present {
                        json_len: 16,
                        json_sha256: "one".to_string(),
                        value_json: Some(json!(CONTENT)),
                        value_json_omitted_reason: None,
                    },
                    reconstructed: TraceEffectEnvelopeDiffValue::Missing,
                }],
            },
        },
        TraceEvent::ProviderEvent {
            event: TraceProviderEvent {
                provider: "test".to_string(),
                sequence: 1,
                elapsed_ms: 0,
                direction: LlmProviderTraceDirection::Response {
                    event_name: "delta".to_string(),
                },
                item_id: Some("item-1".to_string()),
                output_index: Some(0),
                raw_len: 16,
                raw_sha256: "abcd".to_string(),
                raw_json: Some(json!({ "delta": CONTENT })),
                raw_json_omitted_reason: None,
            },
        },
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 1,
                elapsed_ms: 0,
                payload: TraceRuntimeStreamPayload::Block {
                    event: StreamBlockEvent::delta(
                        StreamBlockKind::AssistantText,
                        StreamBlockIdentity::new("msg_1:text:0", 0)
                            .with_item_id(Some("item-1".to_string())),
                        CONTENT,
                    ),
                    raw_text: Some(CONTENT.to_string()),
                },
            },
        },
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 2,
                elapsed_ms: 0,
                payload: TraceRuntimeStreamPayload::ReasoningPart {
                    text: CONTENT.to_string(),
                    item_id: Some("item-1".to_string()),
                },
            },
        },
        TraceEvent::RuntimeStreamEvent {
            event: TraceRuntimeStreamEvent {
                sequence: 3,
                elapsed_ms: 0,
                payload: TraceRuntimeStreamPayload::ToolCallPart {
                    call_id: "provider-call".to_string(),
                    tool_name: "search".to_string(),
                    input_json: json!({ "query": CONTENT }),
                    item_id: None,
                },
            },
        },
        TraceEvent::ToolCallStarted {
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            provider_call_id: Some("provider-call".to_string()),
            name: "search".to_string(),
            args: json!({ "query": CONTENT }),
            issuing_node_id: None,
        },
        TraceEvent::ToolCallCompleted {
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            provider_call_id: Some("provider-call".to_string()),
            name: "search".to_string(),
            args: json!({ "query": CONTENT }),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Failure(json!({
                    "class": "permission_denied",
                    "code": "tool_failed",
                    "message": CONTENT,
                    "source": "tool",
                    "raw": { "detail": CONTENT },
                })),
                control: Some(json!({ "finish": CONTENT })),
            },
            duration_ms: 3,
            issuing_node_id: None,
            attempts: failed_tool_attempt(),
        },
        TraceEvent::ExecCodeStarted {
            code: CONTENT.to_string(),
            code_chars: 14,
        },
        TraceEvent::ExecCodeCompleted {
            duration_ms: 12,
            output: CONTENT.to_string(),
            output_chars: 14,
            observation_count: 1,
            observation_projections: Vec::new(),
            error: Some(lash_trace::CellFailure::new(
                lash_trace::CellFailureKind::Program,
                CONTENT,
            )),
            terminal_finish: Some(json!(CONTENT)),
            tool_calls: vec![TraceExecToolCall {
                call_id: lash_sansio::ToolCallId::fixture("call-1"),
                name: "search".to_string(),
                status: TraceToolCallStatus::Failure,
            }],
        },
        TraceEvent::ExecCodeFailed {
            reason: lash_trace::ExecCodeFailureReason::RuntimeStopped,
            error: CONTENT.to_string(),
        },
        TraceEvent::StoreErrorObserved {
            operation: "session_restore".to_string(),
            error_class: lash_trace::TraceStoreErrorClass::StoredDataCorrupt,
            message: CONTENT.to_string(),
        },
        TraceEvent::ProgramStep {
            step_index: 1,
            outcome: lash_trace::TraceProgramStepOutcome::Failure {
                diagnostic: CONTENT.to_string(),
            },
        },
        TraceEvent::ProtocolStep {
            plugin_id: "custom".to_string(),
            payload: json!({ "assistant": CONTENT }),
        },
        language(TraceLanguageExecutionPayload::ExecutionFinished {
            status: TraceLanguageExecutionStatus::Failed,
            error: Some(CONTENT.to_string()),
        }),
        language(TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("n1", 1),
            fact: lash_trace::TraceNodeFact::Failed {
                call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
                failure: lash_trace::TraceLanguageExecutionFailure::Runtime {
                    code: "runtime_failed".to_string(),
                    message: CONTENT.to_string(),
                },
            },
        }),
    ]
}

/// FIG-5530: the host's content policy governs the whole record vocabulary.
/// A record under an omitted policy says so, carries no content in any event
/// kind and keeps the event's identities, counts and outcomes; the same
/// record under a captured policy is the event as built. Both are records the
/// published schema accepts.
#[test]
fn omitted_content_policy_empties_every_content_field_and_keeps_identity() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/trace-record/v36.schema.json"
    ))
    .expect("published trace schema");
    for event in content_bearing_events() {
        let kind = event.kind();
        let built = fixture_record(TraceContext::default().for_session("s1"), event);
        assert!(
            serde_json::to_string(&built).unwrap().contains(CONTENT),
            "{kind}: the sample carries content"
        );

        let captured = built
            .clone()
            .governed(lash_trace::TelemetryContent::Captured);
        assert_eq!(captured, built, "{kind}: a captured record is as built");

        let omitted = built
            .clone()
            .governed(lash_trace::TelemetryContent::Omitted);
        let value = serde_json::to_value(&omitted).expect("encode record");
        assert!(
            !value.to_string().contains(CONTENT),
            "{kind}: content survived an omitted policy: {value}"
        );
        assert_eq!(value["content"], "omitted", "{kind}");
        assert_eq!(value["type"], kind.as_str());
        assert_eq!(value["id"], "fixture-record");
        assert_eq!(value["context"]["session_id"], "s1");
        assert_schema_accepts(&validator, &value, kind.as_str());
        assert_eq!(
            serde_json::from_value::<TraceRecord>(value).expect("decode an omitted record"),
            omitted,
            "{kind}: an omitted record round-trips"
        );
    }

    let completed = fixture_record(
        TraceContext::default(),
        content_bearing_events()
            .into_iter()
            .find(|event| event.kind() == TraceEventKind::ToolCallCompleted)
            .expect("tool completion sample"),
    )
    .governed(lash_trace::TelemetryContent::Omitted);
    let value = serde_json::to_value(&completed).unwrap();
    assert_eq!(
        value["call_id"],
        lash_sansio::ToolCallId::fixture("call-1").to_string()
    );
    assert_eq!(value["provider_call_id"], "provider-call");
    assert_eq!(value["name"], "search");
    assert_eq!(value["duration_ms"], 3);
    assert_eq!(value["output"]["outcome"]["status"], "failure");
    assert_eq!(
        value["output"]["outcome"]["payload"],
        json!({ "class": "permission_denied", "code": "tool_failed", "source": "tool" }),
        "a failed outcome keeps its typed classification"
    );
    assert_eq!(
        value["attempts"][0]["detail"]["outcome"]["code"],
        "tool_failed"
    );

    let request = fixture_record(
        TraceContext::default(),
        content_bearing_events()
            .into_iter()
            .find(|event| event.kind() == TraceEventKind::ProviderEvent)
            .expect("provider request sample"),
    )
    .governed(lash_trace::TelemetryContent::Omitted);
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["event"]["raw_len"], 13);
    assert_eq!(value["event"]["raw_sha256"], "abcd");
    assert_eq!(
        value["event"]["direction"]["endpoint"], "chat/completions",
        "{value}"
    );
    assert_eq!(value["event"]["raw_json_omitted_reason"], "content_policy");
}

/// A process execution cannot silently lose its start through a missing entry reference.
#[test]
fn execution_identity_requires_its_process_document() {
    let wire = serde_json::to_value(lash_vm_identity()).expect("identity wire");
    for missing in ["document", "function"] {
        let mut incomplete = wire.clone();
        if missing == "document" {
            incomplete
                .as_object_mut()
                .expect("identity object")
                .remove(missing);
        } else {
            incomplete["document"]["entry"]
                .as_object_mut()
                .expect("process entry")
                .remove(missing);
        }
        assert!(
            serde_json::from_value::<TraceLanguageExecutionIdentity>(incomplete).is_err(),
            "an execution must name its complete typed document before it can emit a start: {missing}"
        );
    }
}

/// Schema diagnostics are content under the host's omission policy (FIG-5530).
#[test]
fn omitted_schema_admission_diagnostics_keep_classification_without_text() {
    const COMPILATION: &str = "PRIVATE-COMPILATION-SENTINEL";
    const REFERENCE: &str = "https://private.example/PRIVATE-REFERENCE-SENTINEL";
    let cases = [
        (
            lash_sansio::SchemaAdmissionError::Compilation {
                schema_path: "/properties/mode".into(),
                message: COMPILATION.into(),
            },
            "compilation",
        ),
        (
            lash_sansio::SchemaAdmissionError::NonLocalReference {
                schema_path: "/properties/mode/$ref".into(),
                reference: REFERENCE.into(),
            },
            "non_local_reference",
        ),
    ];
    for (diagnostic, kind) in cases {
        let path = match &diagnostic {
            lash_sansio::SchemaAdmissionError::Compilation { schema_path, .. }
            | lash_sansio::SchemaAdmissionError::NonLocalReference { schema_path, .. } => {
                schema_path.clone()
            }
            lash_sansio::SchemaAdmissionError::InvalidKind { .. } => unreachable!(),
        };
        let mut event = exec_code_completed_event();
        let TraceEvent::ExecCodeCompleted { error, .. } = &mut event else {
            unreachable!()
        };
        *error = Some(
            lash_trace::CellFailure::new(lash_trace::CellFailureKind::Program, COMPILATION)
                .with_schema_admission(diagnostic),
        );
        let record = fixture_record(TraceContext::default().for_session("s1"), event);
        let captured = record
            .clone()
            .governed(lash_trace::TelemetryContent::Captured);
        assert_eq!(captured, record);
        let captured = serde_json::to_value(captured).expect("captured record");
        assert!(captured.to_string().contains(COMPILATION));
        if kind == "non_local_reference" {
            assert!(captured.to_string().contains(REFERENCE));
        }

        let omitted = serde_json::to_value(record.governed(lash_trace::TelemetryContent::Omitted))
            .expect("omitted record");
        assert!(!omitted.to_string().contains(COMPILATION), "{omitted}");
        assert!(!omitted.to_string().contains(REFERENCE), "{omitted}");
        assert_eq!(omitted["type"], "exec_code_completed");
        assert_eq!(omitted["id"], "fixture-record");
        assert_eq!(omitted["context"]["session_id"], "s1");
        assert_eq!(omitted["duration_ms"], 12);
        assert_eq!(omitted["output_chars"], 11);
        assert_eq!(omitted["observation_count"], 2);
        assert_eq!(omitted["error"]["kind"], "program");
        assert_eq!(omitted["error"]["schema_admission"]["kind"], kind);
        assert_eq!(omitted["error"]["schema_admission"]["schema_path"], path);
        assert_eq!(
            omitted["tool_calls"][0]["call_id"],
            lash_sansio::ToolCallId::fixture("call-1").to_string(),
        );
    }
}
