use lash_trace::{
    TraceBranchSelection, TraceContext, TraceDurableTimerStatus, TraceDurableWaitResolution,
    TraceEffectEnvelopeDiffEntry, TraceEffectEnvelopeDiffEvent, TraceEffectEnvelopeDiffValue,
    TraceError, TraceEvent, TraceEventKind, TraceExecToolCall, TraceJournaledEffectStatus,
    TraceLanguageChildExecution, TraceLanguageExecution, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceLlmRequest, TraceLlmResponse,
    TraceProviderReplayDropEvent, TraceProviderReplayDropReason, TraceProviderReplayKind,
    TraceProviderRequestEvent, TraceProviderRouteIdentity, TraceProviderStreamEvent, TraceRecord,
    TraceRuntimeScope, TraceRuntimeStreamEvent, TraceRuntimeSubject, TraceTokenUsage,
    TraceToolCallOutcome, TraceToolCallOutput, TraceToolCallStatus, TraceTurnCompletionReason,
    TraceTurnOutcome,
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
fn node_kind_refuses_unrecognized_wire_value() {
    let payload = serde_json::json!({
        "kind": "node_started",
        "node_id": "node-1",
        "node_kind": "future_kind",
        "label": "step",
        "occurrence": 1
    });
    let error = serde_json::from_value::<lash_trace::TraceLanguageExecutionPayload>(payload)
        .expect_err("node kinds are a closed wire vocabulary");
    assert!(error.to_string().contains("future_kind"));
}

#[test]
fn node_failure_requires_typed_provenance() {
    let legacy = json!({
        "kind": "node_failed",
        "node_id": "node-1",
        "node_kind": "resource_operation",
        "label": "read",
        "occurrence": 1,
        "error": "permission denied"
    });
    let refusal = serde_json::from_value::<TraceLanguageExecutionPayload>(legacy)
        .expect_err("the former string-only node failure must be refused");
    assert!(refusal.to_string().contains("failure"), "{refusal}");

    let payload = TraceLanguageExecutionPayload::NodeFailed {
        node_id: "node-1".to_owned(),
        node_kind: lash_sansio::ExecutionNodeKind::ResourceOperation,
        label: "read".to_owned(),
        occurrence: 1,
        call_id: Some(lash_sansio::ToolCallId::fixture("effect-1")),
        failure: lash_trace::TraceLanguageExecutionFailure::Effect {
            class: lash_sansio::ToolFailureClass::PermissionDenied,
            code: "approval_denied".to_owned(),
            message: "permission denied".to_owned(),
            replay_key: "effect-1".to_owned(),
            source: lash_sansio::ToolFailureSource::Policy,
            suggested_delay_ms: None,
        },
    };
    let wire = serde_json::to_value(&payload).expect("encode typed failure");
    assert_eq!(
        wire["failure"],
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
    unknown_kind["failure"]["kind"] = json!("unknown");
    assert!(serde_json::from_value::<TraceLanguageExecutionPayload>(unknown_kind).is_err());
    let mut unknown_class = wire.clone();
    unknown_class["failure"]["class"] = json!("unknown");
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
            process_id: lash_sansio::ProcessId::fixture("p1"),
        },
        source_identity: "source".to_string(),
        module_ref: "module".to_string(),
        entry_kind: "process".to_string(),
        entry_ref: Some("component:0".to_string()),
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
                retryable: true,
                terminal_reason: lash_trace::TraceLlmTerminalReason::Unknown,
                failure_kind: lash_trace::TraceProviderFailureKind::Unknown,
                code: None,
            },
            stream_summary: None,
            attempts: None,
        },
        TraceEvent::LlmAttemptCompleted {
            attempt: lash_trace::TraceLlmAttempt {
                ordinal: 1,
                provider: Some("test".to_string()),
                request_model: "m".to_string(),
                response_model: None,
                started_at_ms: Some(1),
                ended_at_ms: Some(2),
                outcome: lash_trace::TraceLlmAttemptOutcome::Completed,
                error: None,
                usage: None,
            },
        },
        TraceEvent::DomainCompleted {
            completion: lash_trace::TraceDomainCompletion::new(
                lash_trace::TraceDomainOperation::Run,
                1,
                lash_trace::TraceDomainStatus::Completed,
            ),
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
                block_id: None,
                output_index: None,
                call_id: None,
                tool_name: None,
                input_json: None,
                usage: None,
            },
        },
        TraceEvent::ToolReceipt {
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            name: "search".to_string(),
            started_at_ms: 1,
            terminal: Some(lash_trace::TraceToolTerminal::Final),
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
            started_at_ms: 0,
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

/// Core tracing is seam-neutral: no event names the plugin, durable
/// substrate, store backend or provider that produced it. Those
/// implementations report through the shared vocabulary.
#[test]
fn trace_event_vocabulary_names_no_seam_implementation() {
    const IMPLEMENTATIONS: &[&str] = &[
        "rlm",
        "lashlang",
        "restate",
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

#[test]
fn unknown_attempt_usage_disposition_is_refused() {
    // The four legal spellings are the vocabulary `AttemptUsageOutcome`
    // owns upstream; the trace layer was the only one that flattened them to a
    // free-form string, so any capitalisation or invention decoded silently.
    let attempt = json!({
        "ordinal": 1,
        "detail": {"kind": "llm", "outcome": "aborted", "usage_disposition": "REPORTED"},
    });
    let error = serde_json::from_value::<lash_trace::TraceRetryAttempt>(attempt)
        .expect_err("an unknown usage disposition must be refused");
    assert_eq!(
        error.to_string(),
        "unknown variant `REPORTED`, expected one of `reported`, `unreported_by_provider`, \
         `unreported_after_abort`, `unreported_after_failure`",
    );

    let legal = json!({
        "ordinal": 1,
        "detail": {"kind": "llm", "outcome": "aborted", "usage_disposition": "unreported_after_abort"},
    });
    let decoded =
        serde_json::from_value::<lash_trace::TraceRetryAttempt>(legal).expect("legal spelling");
    assert_eq!(
        serde_json::to_value(&decoded).expect("re-encode")["detail"]["usage_disposition"],
        json!("unreported_after_abort"),
    );
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

/// One language-execution payload of every kind, in an order the graph fold
/// accepts: the execution map first, then the observed nodes, then the finish.
fn language_execution_payload_samples() -> Vec<TraceLanguageExecutionPayload> {
    let site = |path: &[u32], kind, label: &str| {
        lash_sansio::WorkflowExecutionSite::new("main", path, kind, label)
    };
    vec![
        TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: TraceLanguageExecutionMap {
                nodes: vec![
                    TraceLanguageExecutionMapNode {
                        id: "branch".to_string(),
                        site: site(&[0], lash_sansio::ExecutionNodeKind::Branch, "if ready"),
                        kind: lash_sansio::ExecutionNodeKind::Branch,
                        label: "if ready".to_string(),
                        branch_memberships: Vec::new(),
                        label_metadata: Some(lash_trace::TraceLabelMetadata {
                            title: "Ready?".to_string(),
                            description: Some("gate".to_string()),
                        }),
                    },
                    TraceLanguageExecutionMapNode {
                        id: "then".to_string(),
                        site: site(&[0, 1, 0], lash_sansio::ExecutionNodeKind::Call, "notify()"),
                        kind: lash_sansio::ExecutionNodeKind::Call,
                        label: "notify()".to_string(),
                        branch_memberships: vec![lash_trace::TraceBranchMembership {
                            branch_node_id: "branch".to_string(),
                            arm: TraceBranchSelection::Then,
                        }],
                        label_metadata: None,
                    },
                ],
                edges: vec![TraceLanguageExecutionMapEdge {
                    id: "then-edge".to_string(),
                    from: "branch".to_string(),
                    to: "then".to_string(),
                    label: "sequence".to_string(),
                }],
            },
        },
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "branch".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Branch,
            label: "if ready".to_string(),
            occurrence: 1,
            call_id: None,
        },
        TraceLanguageExecutionPayload::BranchSelected {
            node_id: "branch".to_string(),
            occurrence: 1,
            edge_id: "then-edge".to_string(),
            selected: TraceBranchSelection::Then,
        },
        TraceLanguageExecutionPayload::NodeCompleted {
            node_id: "branch".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Branch,
            label: "if ready".to_string(),
            occurrence: 1,
            call_id: None,
        },
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 1,
            call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
        },
        TraceLanguageExecutionPayload::NodeWaiting {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 1,
            awaited: lash_trace::TraceNodeAwaited::Signal {
                name: "approved".to_string(),
                key: "approved:1".to_string(),
            },
        },
        TraceLanguageExecutionPayload::NodeResumed {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 1,
            resolution: lash_trace::TraceNodeWaitResolution::Resumed,
        },
        TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id: "then".to_string(),
            occurrence: 1,
            child: TraceLanguageChildExecution {
                scope: TraceRuntimeScope::new("s1"),
                process_id: lash_sansio::ProcessId::fixture("child-1"),
                attempt: Some(1),
                module_ref: Some("child-module".to_string()),
                entry_ref: None,
                entry_name: Some("child".to_string()),
            },
        },
        TraceLanguageExecutionPayload::NodeFailed {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 1,
            call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
            failure: lash_trace::TraceLanguageExecutionFailure::Runtime {
                code: "boom".to_string(),
                message: "notify failed".to_string(),
            },
        },
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 2,
            call_id: None,
        },
        TraceLanguageExecutionPayload::NodeCancelled {
            node_id: "then".to_string(),
            node_kind: lash_sansio::ExecutionNodeKind::Call,
            label: "notify()".to_string(),
            occurrence: 2,
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
                    language: "lashlang".to_string(),
                    event: TraceLanguageExecution {
                        event_key: format!("process:p1:{index}"),
                        identity: lashlang_identity(),
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
fn published_graph_schema_accepts_a_folded_snapshot_and_enforces_its_row() {
    let validator = published_schema(include_str!(
        "../../../schemas/host/trace-lashlang-graph/v36.schema.json"
    ))
    .expect("published trace schema");
    let graph = lash_trace::TraceLashlangGraphStore::fold(None, &language_execution_records())
        .expect("fold every payload kind");
    assert!(!graph.nodes.is_empty() && !graph.history.is_empty());
    let mut value = serde_json::to_value(&graph).expect("encode graph");
    assert_schema_accepts(&validator, &value, "a folded graph snapshot");

    value["future_field"] = json!(true);
    assert_schema_accepts(&validator, &value, "an additive snapshot field");
    for (field, pointer) in [
        ("graph status", "/status"),
        ("completeness", "/completeness"),
        ("node kind", "/nodes/0/kind"),
    ] {
        let mut changed = value.clone();
        *changed.pointer_mut(pointer).expect(field) = json!("future_variant");
        assert!(
            !validator.is_valid(&changed),
            "unknown {field} variant must be refused"
        );
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
        context,
        event,
    }
}
