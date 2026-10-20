use lash_protocol_types::{
    llm::types::LlmProviderTraceDirection, llm::types::LlmUsage, llm::types::StreamBlockEvent,
    llm::types::StreamBlockIdentity, llm::types::StreamBlockKind,
};
use lash_trace::{
    TRACE_SCHEMA_VERSION, TraceBranchSelection, TraceContext, TraceEffectEnvelopeDiffEntry,
    TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue, TraceError, TraceEvent,
    TraceEventKind, TraceExecToolCall, TraceLanguageChildExecution, TraceLanguageExecution,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceLlmRequest, TraceLlmResponse, TraceProviderEvent, TraceProviderReplayDropEvent,
    TraceProviderReplayDropReason, TraceProviderReplayKind, TraceProviderRouteIdentity,
    TraceRecord, TraceRuntimeScope, TraceRuntimeStreamEvent, TraceRuntimeStreamPayload,
    TraceRuntimeSubject, TraceToolCallOutcome, TraceToolCallOutput, TraceToolCallStatus,
    TraceTurnCompletionReason, TraceTurnOutcome,
};
use serde_json::json;

#[test]
fn count_observations_refuse_ambiguous_subject_and_accumulation() {
    let mut ambiguous = json!({
        "event_key": "kernel_execution:process:p1:ran:root:1",
        "identity": lash_vm_identity(),
        "kind": "ran",
        "segment": "root",
        "ran": [[1, 2]]
    });
    ambiguous["identity"]["subject"] = json!({
        "type": "process", "process_id": lash_protocol_types::ProcessId::fixture("p1")
    });
    assert!(serde_json::from_value::<TraceLanguageExecution>(ambiguous).is_err());

    let schema = serde_json::to_value(schemars::schema_for!(TraceLanguageExecution)).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    for payload in language_execution_payload_samples() {
        let TraceLanguageExecutionPayload::Counts { .. } = &payload else {
            continue;
        };
        let mut execution = TraceLanguageExecution {
            event_key: String::new(),
            identity: lash_vm_identity(),
            payload,
        };
        execution.event_key = execution.count_event_key().unwrap();
        let wire = serde_json::to_value(&execution).unwrap();
        assert_eq!(
            serde_json::from_value::<TraceLanguageExecution>(wire.clone()).unwrap(),
            execution
        );
        assert_schema_accepts(&validator, &wire, "typed counts");

        let mut bad_key = wire.clone();
        bad_key["event_key"] = json!("another frame");
        assert!(serde_json::from_value::<TraceLanguageExecution>(bad_key).is_err());

        let mut inconsistent = Vec::new();
        let mut old_subject = wire.clone();
        old_subject["identity"]["subject"] = json!({
            "type": "process", "process_id": lash_protocol_types::ProcessId::fixture("other")
        });
        inconsistent.push(old_subject);
        let mut extra_subject = wire.clone();
        extra_subject["subject"] = json!({
            "type": "process", "process_id": lash_protocol_types::ProcessId::fixture("other")
        });
        inconsistent.push(extra_subject);
        let mut wrong_mode = wire.clone();
        wrong_mode["counts"]["type"] = json!(if wire["counts"]["type"] == "process" {
            "cell"
        } else {
            "process"
        });
        inconsistent.push(wrong_mode);
        for field in ["segment", "frame"] {
            let mut missing = wire.clone();
            missing["counts"].as_object_mut().unwrap().remove(field);
            inconsistent.push(missing);
        }
        let mut mixed_arithmetic = wire.clone();
        mixed_arithmetic["counts"][if wire["counts"]["type"] == "process" {
            "totals"
        } else {
            "delta"
        }] = json!([[1, 2]]);
        inconsistent.push(mixed_arithmetic);
        for invalid in inconsistent {
            assert!(
                serde_json::from_value::<TraceLanguageExecution>(invalid.clone()).is_err(),
                "{invalid}"
            );
            assert!(!validator.is_valid(&invalid), "schema admitted {invalid}");
        }
    }
}

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
        "subject": { "type": "process", "process_id": lash_protocol_types::ProcessId::fixture("p1") },
        "at": lash_protocol_types::effect_identity_fixture("node-1", 1),
        "fact": { "kind": "failed", "error": "permission denied" }
    });
    let refusal = serde_json::from_value::<TraceLanguageExecutionPayload>(legacy)
        .expect_err("the former string-only node failure must be refused");
    assert!(refusal.to_string().contains("failure"), "{refusal}");

    let payload = TraceLanguageExecutionPayload::Node {
        subject: TraceRuntimeSubject::Process {
            process_id: lash_protocol_types::ProcessId::fixture("p1"),
        },
        at: lash_protocol_types::effect_identity_fixture("node-1", 1),
        fact: lash_trace::TraceNodeFact::Failed {
            call_id: Some(lash_protocol_types::ToolCallId::fixture("effect-1")),
            failure: lash_trace::TraceLanguageExecutionFailure::Effect {
                class: lash_protocol_types::ToolFailureClass::PermissionDenied,
                code: "approval_denied".to_owned(),
                message: "permission denied".to_owned(),
                replay_key: "effect-1".to_owned(),
                source: lash_protocol_types::ToolFailureSource::Policy,
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
        format!("unsupported trace schema version 3; expected {TRACE_SCHEMA_VERSION}")
    );

    let stale_and_malformed = r#"{"schema_version":3,"payload":"not a current event"}"#;
    let error = serde_json::from_str::<TraceRecord>(stale_and_malformed)
        .expect_err("the version refusal must precede current-shape validation");
    assert_eq!(
        error.to_string(),
        format!("unsupported trace schema version 3; expected {TRACE_SCHEMA_VERSION}")
    );
}

fn token_usage_sample() -> LlmUsage {
    LlmUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 1,
        cache_write_input_tokens: 2,
        reasoning_output_tokens: 3,
        unknown_calls: 0,
    }
}

fn lash_vm_identity() -> TraceLanguageExecutionIdentity {
    TraceLanguageExecutionIdentity {
        scope: TraceRuntimeScope::new("s1"),
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
        TraceEvent::ProducerSettled {
            producer: lash_trace::ProducerKind::Send,
            outcome: lash_trace::ProducerOutcome::Refused {
                error_type: "session_closed".into(),
            },
            phases: Vec::new(),
        },
        TraceEvent::ProducerSettled {
            producer: lash_trace::ProducerKind::ProcessStart,
            outcome: lash_trace::ProducerOutcome::Selected,
            phases: [
                lash_trace::ProcessStartPhase::DefinitionLoad,
                lash_trace::ProcessStartPhase::DefinitionResolve,
                lash_trace::ProcessStartPhase::DefinitionAcquire,
                lash_trace::ProcessStartPhase::DefinitionVerify,
                lash_trace::ProcessStartPhase::LinkCheck,
            ].into_iter().map(|phase| lash_trace::ProcessStartPhaseTime {
                phase,
                elapsed_us: 1_250,
            }).collect(),
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
            position: lash_protocol_types::llm::attachment_delivery::AttachmentPosition::Message,
            reason: lash_protocol_types::AttachmentMaterializationReason::TargetModelDoesNotAcceptMimeAndPosition,
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
                system_instructions: None,
                composition_fingerprint: Default::default(),
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
            model: None,
            request: Default::default(),
            response_metadata: Default::default(),
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
            model: None,
            request: Default::default(),
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
            response_metadata: Default::default(),
            attempt: lash_protocol_types::llm::types::AttemptRecord {
                ordinal: 1,
                outcome: lash_protocol_types::llm::types::AttemptOutcome::Completed,
                protocol_position: lash_protocol_types::llm::types::ProtocolPosition::TerminalObserved,
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
                lash_trace::TraceDomainSubject::Process {
                    kind: "session_turn".into(),
                    label: None,
                },
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
            conflict: lash_protocol_types::ToolCheckConflict {
                phase: lash_protocol_types::ToolCheckPhase::ToolArgsCheck,
                winner: lash_protocol_types::ToolCheckReply {
                    plugin_id: "alpha".to_string(),
                    callback: "tool_args_check:check".to_string(),
                    verdict: lash_protocol_types::ToolCheckVerdictKind::Deny,
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
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            name: "read_file".to_string(),
            args: json!({ "path": "README.md" }),
            issuing_node_id: None,
        },
        TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            name: "read_file".to_string().into(),
            args: json!({ "path": "README.md" }).into(),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Success(json!("ok")),
                control: None,
            },
            duration_ms: 3,
            issuing_node_id: None,
            attempts: None,
            span_attributes: Default::default(),
        },
        TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture("abandoned"),
            provider_call_id: None,
            name: lash_trace::TraceToolName::Identity { tool_id: lash_protocol_types::ToolId::new("tool:read") },
            args: lash_trace::TraceToolArgs::Unavailable,
            output: TraceToolCallOutput { outcome: TraceToolCallOutcome::Discarded, control: None },
            duration_ms: 3, issuing_node_id: None, attempts: None, span_attributes: Default::default(),
        },
        TraceEvent::ExecCodeStarted {
            language: "python".to_string(),
            code_chars: 8,
        },
        exec_code_completed_event(),
        TraceEvent::ExecCodeFailed {
            trailing_discarded: 0,
            code: String::new(),
            language: "python".to_string(),
            started_at_ms: 0,
            duration_ms: None,
            reason: lash_trace::ExecCodeFailureReason::RuntimeStopped,
            error: "boom".to_string(),
        },
        TraceEvent::ExecCodeDiscarded {
            trailing_discarded: 0,
            code: String::new(),
            language: "python".to_string(),
            started_at_ms: 0,
            duration_ms: 9,
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
 subject: TraceRuntimeSubject::Process { process_id: lash_protocol_types::ProcessId::fixture("p1") },
                    status: TraceLanguageExecutionStatus::Completed,
                    error: None,
                },
            },
        },
        TraceEvent::TurnCompleted {
            output: vec![lash_trace::TraceLlmMessage {
                role: "assistant".into(),
                blocks: vec![lash_trace::TraceContentBlock::Text {
                    text: "turn answer".into(),
                    cache_breakpoint: false,
                }],
            }],
            input: Vec::new(),
            inference_calls: Some(2),
            tool_calls: Some(1),
            inputs: 1,
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
        },
        TraceEvent::ExtensionSpan(lash_trace::ExtensionSpanRecord {
            plugin_id: "plugin".into(),
            callback: "tool_provider:x".into(),
            name: "x.span".into(),
            kind: lash_trace::ExtensionSpanKind::Client,
            operation: Some(lash_trace::ExtensionOperation::Retrieval {
                data_source_id: "docs".into(),
            }),
            started_at_ms: 1,
            ended_at_ms: 2,
            point: false,
            error_type: Some("timeout".into()),
            ended_by: Some(lash_trace::ExtensionSpanEnd::BodyExit),
            attributes: vec![lash_trace::ExtensionAttribute {
                key: "x.count".into(),
                value: lash_trace::TraceValue::Int(3),
            }],
            events: vec![lash_trace::ExtensionSpanEvent {
                name: "x.event".into(),
                at_ms: 2,
                attributes: Vec::new(),
            }],
            content: vec![lash_trace::ExtensionContent {
                key: "x.content".into(),
                value: json!({ "ok": true }),
            }],
        }),
        TraceEvent::BodyOmissions(lash_trace::TraceOmissionDiagnostic {
            site: lash_trace::TraceOmissionSite::Body {
                owner: "plugin".into(),
                key: "tool_provider:x".into(),
                at: None,
                call: None,
                attempt: Some(1),
            },
            counts: lash_trace::TraceOmissionCounts::default(),
        }),
        TraceEvent::ContextPressure {
            plugin_id: "plugin".into(),
            hook_id: "pressure".into(),
            task: "compact".into(),
            frame_node_id: "frame".into(),
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
        trailing_discarded: 0,
        code: String::new(),
        language: "python".to_string(),
        started_at_ms: 0,
        duration_ms: Some(0),
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
        "language": "python",
        "started_at_ms": 0,
        "duration_ms": 0,
        "error": "boom",
    });
    serde_json::from_value::<TraceEvent>(reasonless)
        .expect_err("a missing reason is refused, not defaulted");

    let unknown_reason = serde_json::json!({
        "type": "exec_code_failed",
        "language": "python",
        "started_at_ms": 0,
        "duration_ms": 0,
        "reason": "executor_went_to_lunch",
        "error": "boom",
    });
    serde_json::from_value::<TraceEvent>(unknown_reason)
        .expect_err("a reason outside the closed set is refused");
}

fn exec_code_completed_event() -> TraceEvent {
    TraceEvent::ExecCodeCompleted {
        trailing_discarded: 0,
        code: String::new(),
        language: "python".to_string(),
        started_at_ms: 0,
        duration_ms: 12,
        output: "hello\nworld".to_string(),
        output_chars: 11,
        observation_count: 2,
        observation_projections: Vec::new(),
        error: None,
        terminal_finish: None,
        tool_calls: vec![TraceExecToolCall {
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
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
        TraceLanguageExecutionPayload::ExecutionStarted {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            observation: lash_kernel_doc::RunObservation::Off,
        },
        TraceLanguageExecutionPayload::ExecutionStarted {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            observation: lash_kernel_doc::RunObservation::Counts,
        },
        TraceLanguageExecutionPayload::Counts {
            counts: lash_trace::TraceCountObservation::Process(lash_trace::TraceProcessCounts {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
                segment: "root".into(),
                frame: 1,
                delta: vec![(1, 2)],
            }),
        },
        TraceLanguageExecutionPayload::Counts {
            counts: lash_trace::TraceCountObservation::Cell(lash_trace::TraceCellCounts {
                address: lash_protocol_types::EffectAddress {
                    execution_scope: lash_protocol_types::ExecutionScope::turn("s1", "t1"),
                    replay_key: "cell".into(),
                },
                effect_id: "cell".into(),
                segment: "root".into(),
                frame: 1,
                totals: vec![(1, 2)],
            }),
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::Started { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::BranchSelected {
                selected: TraceBranchSelection::Then,
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("branch", 1),
            fact: lash_trace::TraceNodeFact::Completed { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Started {
                call_id: Some(lash_protocol_types::ToolCallId::fixture("call-1")),
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Waiting {
                awaited: lash_trace::TraceNodeAwaited::Signal {
                    name: "approved".to_string(),
                    key: "approved:1".to_string(),
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Resumed {
                resolution: lash_trace::TraceNodeWaitResolution::Resumed,
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::ChildStarted {
                child: TraceLanguageChildExecution {
                    scope: TraceRuntimeScope::new("s1"),
                    process_id: lash_protocol_types::ProcessId::fixture("child-1"),
                    attempt: Some(1),
                    document: None,
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 1),
            fact: lash_trace::TraceNodeFact::Failed {
                call_id: Some(lash_protocol_types::ToolCallId::fixture("call-1")),
                failure: lash_trace::TraceLanguageExecutionFailure::Runtime {
                    code: "boom".to_string(),
                    message: "notify failed".to_string(),
                },
            },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 2),
            fact: lash_trace::TraceNodeFact::Started { call_id: None },
        },
        TraceLanguageExecutionPayload::Node {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
            at: lash_protocol_types::effect_identity_fixture("then", 2),
            fact: lash_trace::TraceNodeFact::Cancelled,
        },
        TraceLanguageExecutionPayload::ExecutionFinished {
            subject: TraceRuntimeSubject::Process {
                process_id: lash_protocol_types::ProcessId::fixture("p1"),
            },
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
                    event: {
                        let mut event = TraceLanguageExecution {
                            event_key: format!("process:p1:{index}"),
                            identity: lash_vm_identity(),
                            payload,
                        };
                        if let Some(key) = event.count_event_key() {
                            event.event_key = key;
                        }
                        event
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
        "../../../schemas/host/trace-record/v37.schema.json"
    ))
    .expect("published trace schema");
    let context = TraceContext {
        graph_node_id: Some("turn:s1:t1".to_string()),
        turn_index: Some(0),
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
        "../../../schemas/host/trace-record/v37.schema.json"
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

    // A tool failure's class is a closed classification, not open content.
    let failed = fixture_record(
        TraceContext::default(),
        TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
            provider_call_id: None,
            name: "search".to_string().into(),
            args: json!({}).into(),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Failure(lash_trace::TraceToolFailure::new(
                    &lash_protocol_types::ToolFailure::new("tool_failed", "failed"),
                    lash_trace::ContentPolicy::captured(),
                )),
                control: None,
            },
            duration_ms: 1,
            issuing_node_id: None,
            attempts: None,
            span_attributes: Default::default(),
        },
    );
    let mut malformed = serde_json::to_value(&failed).expect("encode record");
    assert_schema_accepts(&validator, &malformed, "a typed tool failure");
    malformed["output"]["outcome"]["payload"]["class"] = json!("retryable");
    assert!(
        !validator.is_valid(&malformed),
        "an unknown tool failure class must be refused"
    );
    assert!(
        serde_json::from_value::<lash_trace::TraceRecord>(malformed).is_err(),
        "an unknown tool failure class must not decode"
    );
}

#[test]
fn published_overlay_schema_accepts_a_folded_overlay_and_enforces_its_row() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/workflow-execution-overlay/v37.schema.json"
    ))
    .expect("published overlay schema");
    let mut records = language_execution_records();
    records.retain(|record| {
        !matches!(&record.event,
            TraceEvent::LanguageExecution { event, .. }
                if matches!(&event.payload, TraceLanguageExecutionPayload::Counts {
                    counts: lash_trace::TraceCountObservation::Cell(_)
                })
        )
    });
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
        [lash_protocol_types::effect_identity_fixture("then", 1).position()],
    );
    let overlay = lash_trace::fold_workflow_overlay(None, Some(&document), &records, 1)
        .expect("fold one process execution");
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
        process_id: lash_protocol_types::ProcessId::fixture("p1"),
        at: lash_protocol_types::effect_identity_fixture("then", 2),
        call_id: lash_protocol_types::ToolCallId::fixture("call-2"),
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
        timestamp: lash_trace::TraceTimestamp::from_unix_ms(0),
        content: lash_trace::ContentPolicy::captured().into(),
        context,
        position: None,
        omissions: Default::default(),
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
                    class: lash_protocol_types::ToolFailureClass::Execution,
                    code: "tool_failed".to_string(),
                    message: CONTENT.to_string(),
                    source: lash_protocol_types::ToolFailureSource::Tool,
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
        TraceEvent::TurnCompleted {
            output: vec![lash_trace::TraceLlmMessage {
                role: "assistant".into(),
                blocks: vec![lash_trace::TraceContentBlock::Text {
                    text: CONTENT.into(),
                    cache_breakpoint: false,
                }],
            }],
            inference_calls: None,
            tool_calls: None,
            input: vec![lash_trace::TraceLlmMessage {
                role: "user".into(),
                blocks: vec![lash_trace::TraceContentBlock::Text {
                    text: CONTENT.into(),
                    cache_breakpoint: false,
                }],
            }],
            inputs: 1,
            outcome: TraceTurnOutcome::Completed {
                done_reason: TraceTurnCompletionReason::AssistantMessage,
            },
        },
        TraceEvent::ExecCodeDiscarded {
            trailing_discarded: 0,
            code: CONTENT.into(),
            language: "typescript".into(),
            started_at_ms: 0,
            duration_ms: 1,
        },
        TraceEvent::AttachmentDegraded {
            attachment_id: Some("attachment-id".to_string()),
            label: Some(CONTENT.to_string()),
            media_type: Some("application/octet-stream".to_string()),
            position: lash_protocol_types::llm::attachment_delivery::AttachmentPosition::Message,
            reason: lash_protocol_types::AttachmentMaterializationReason::TargetModelDoesNotAcceptMimeAndPosition,
        },
        TraceEvent::CompositionChanged {
            fingerprint: "composition-sha".to_string(),
            rendered_system_prompt: CONTENT.to_string(),
            tool_schemas: vec![tool_spec()],
        },
        TraceEvent::LlmCallStarted {
            request: TraceLlmRequest {
                system_instructions: None,
                composition_fingerprint: Default::default(),
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
            model: None,
            request: Default::default(),
            response_metadata: Default::default(),
            response: TraceLlmResponse {
                text: CONTENT.to_string(),
                duration_ms: 12,
                request_model: "request-model".to_string(),
                terminal_reason: Some(lash_trace::TraceLlmTerminalReason::Stop),
                parts: Some(vec![lash_protocol_types::llm::types::LlmOutputPart::Text {
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
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
            provider_call_id: Some("provider-call".to_string()),
            name: "search".to_string(),
            args: json!({ "query": CONTENT }),
            issuing_node_id: None,
        },
        TraceEvent::ToolCallCompleted {
            call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
            provider_call_id: Some("provider-call".to_string()),
            name: "search".to_string().into(),
            args: json!({ "query": CONTENT }).into(),
            output: TraceToolCallOutput {
                outcome: TraceToolCallOutcome::Failure(lash_trace::TraceToolFailure::new(
                    &lash_protocol_types::ToolFailure::new("tool_failed", CONTENT)
                        .with_raw(json!({ "detail": CONTENT })),
                    lash_trace::ContentPolicy::captured(),
                )),
                control: Some(json!({ "finish": CONTENT })),
            },
            duration_ms: 3,
            issuing_node_id: None,
            attempts: failed_tool_attempt(),
            span_attributes: Default::default(),
        },
        TraceEvent::ExecCodeCompleted {
            trailing_discarded: 0,
            code: String::new(),
            language: "python".to_string(),
            started_at_ms: 0,
            duration_ms: 12,
            output: CONTENT.to_string(),
            output_chars: 14,
            observation_count: 1,
            observation_projections: Vec::new(),
            error: Some({
                let mut failure =
                    lash_trace::CellFailure::new(lash_trace::CellFailureKind::Program, CONTENT);
                failure.feedback.push(lash_protocol_types::CellFailureRepair {
                    plugin_id: "repair".into(),
                    callback: "cell_failure_feedback:names".into(),
                    repairs: vec![CONTENT.into()],
                });
                failure
            }),
            terminal_finish: Some(json!(CONTENT)),
            tool_calls: vec![TraceExecToolCall {
                call_id: lash_protocol_types::ToolCallId::fixture("call-1"),
                name: "search".to_string(),
                status: TraceToolCallStatus::Failure,
            }],
        },
        TraceEvent::ExecCodeFailed {
            trailing_discarded: 0,
            code: String::new(),
            language: "python".to_string(),
            started_at_ms: 0,
            duration_ms: Some(0),
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
 subject: TraceRuntimeSubject::Process { process_id: lash_protocol_types::ProcessId::fixture("p1") },
            status: TraceLanguageExecutionStatus::Failed,
            error: Some(CONTENT.to_string()),
        }),
        language(TraceLanguageExecutionPayload::Node {
 subject: TraceRuntimeSubject::Process { process_id: lash_protocol_types::ProcessId::fixture("p1") },
            at: lash_protocol_types::effect_identity_fixture("n1", 1),
            fact: lash_trace::TraceNodeFact::Failed {
                call_id: Some(lash_protocol_types::ToolCallId::fixture("call-1")),
                failure: lash_trace::TraceLanguageExecutionFailure::Runtime {
                    code: "runtime_failed".to_string(),
                    message: CONTENT.to_string(),
                },
            },
        }),
    ]
}

/// L6: every content-bearing variant is reached by the one hook traversal.
#[test]
fn content_hook_reaches_every_content_bearing_variant() {
    struct Redact;
    impl lash_trace::ContentHook for Redact {
        fn on_content(
            &self,
            _: &lash_trace::ContentView<'_>,
            content: &mut lash_trace::RecordContent<'_>,
        ) {
            fn clear(value: &mut serde_json::Value) {
                match value {
                    serde_json::Value::String(text) => *text = text.replace(CONTENT, "[redacted]"),
                    serde_json::Value::Array(values) => values.iter_mut().for_each(clear),
                    serde_json::Value::Object(fields) => fields.values_mut().for_each(clear),
                    _ => {}
                }
            }
            content.for_each(|field| match field.value() {
                lash_trace::ContentValue::Text(text) => *text = text.replace(CONTENT, "[redacted]"),
                lash_trace::ContentValue::Json(value) => clear(value),
            });
        }
    }
    let policy = lash_trace::TracePolicySnapshot {
        content: lash_trace::ContentPolicy::captured(),
        ..Default::default()
    };
    for event in content_bearing_events() {
        let kind = event.kind();
        let mut record = fixture_record(TraceContext::default(), event);
        assert!(record.apply_content(&policy, Some(&Redact)).is_none());
        assert!(
            !serde_json::to_string(&record).unwrap().contains(CONTENT),
            "{kind}: the hook missed content"
        );
    }
}

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
            lash_protocol_types::SchemaAdmissionError::Compilation {
                schema_path: "/properties/mode".into(),
                message: COMPILATION.into(),
            },
            "compilation",
        ),
        (
            lash_protocol_types::SchemaAdmissionError::NonLocalReference {
                schema_path: "/properties/mode/$ref".into(),
                reference: REFERENCE.into(),
            },
            "non_local_reference",
        ),
        (
            lash_protocol_types::SchemaAdmissionError::Unprojectable {
                schema_path: "/properties/mode/allOf".into(),
                message: COMPILATION.into(),
            },
            "unprojectable",
        ),
    ];
    for (diagnostic, kind) in cases {
        let path = match &diagnostic {
            lash_protocol_types::SchemaAdmissionError::Compilation { schema_path, .. }
            | lash_protocol_types::SchemaAdmissionError::NonLocalReference {
                schema_path, ..
            }
            | lash_protocol_types::SchemaAdmissionError::Unprojectable { schema_path, .. } => {
                schema_path.clone()
            }
            lash_protocol_types::SchemaAdmissionError::InvalidKind { .. } => unreachable!(),
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
            .governed(lash_trace::ContentPolicy::captured());
        assert_eq!(captured.event, record.event);
        let captured = serde_json::to_value(captured).expect("captured record");
        assert!(captured.to_string().contains(COMPILATION));
        if kind == "non_local_reference" {
            assert!(captured.to_string().contains(REFERENCE));
        }

        let omitted = serde_json::to_value(record.governed(lash_trace::ContentPolicy::omitted()))
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
            lash_protocol_types::ToolCallId::fixture("call-1").to_string(),
        );
    }
}
