use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;

use super::*;

const EXAMPLE_BINDING_KEY: &str = "example.call_path";

#[derive(Clone)]
struct VecRegistry(Vec<RemoteToolGrant>);

impl RemoteToolRegistry for VecRegistry {
    fn grants(&self) -> Vec<RemoteToolGrant> {
        self.0.clone()
    }
}

#[test]
fn remote_llm_request_json_round_trips() {
    let request = RemoteLlmRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        request_id: "request-1".to_string(),
        scope: RemoteLlmRequestScope::new("session", "session:frame:test", "request-1"),
        model_intent: RemoteModelIntent::new("gpt-test"),
        messages: vec![RemoteLlmMessage {
            role: RemoteLlmRole::User,
            content: vec![RemoteLlmContentBlock::Text {
                text: "hello".to_string(),
                response_meta: None,
                cache_breakpoint: false,
            }],
        }],
        attachments: vec![RemoteAttachmentSource::Inline {
            media_type: "image/png".to_string(),
            data_base64: "AQID".to_string(),
        }],
        tools: Vec::new(),
        tool_choice: RemoteLlmToolChoice::Auto,
        output_spec: Some(RemoteLlmOutputSpec::JsonObject),
        generation: RemoteGenerationOptions {
            output_token_cap: Some(128),
            ..Default::default()
        },
        metadata: HashMap::new(),
    };

    request.validate().expect("valid request");
    let value = serde_json::to_value(&request).expect("serialize");
    let decoded: RemoteLlmRequest = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.request_id, request.request_id);
    assert_eq!(decoded.scope, request.scope);
    assert_eq!(decoded.messages, request.messages);
}

#[test]
fn remote_attachment_media_types_are_validated_syntactically() {
    let mut request = RemoteLlmRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        request_id: "request-invalid-mime".to_string(),
        scope: RemoteLlmRequestScope::new("session", "session:frame:test", "request-invalid-mime"),
        model_intent: RemoteModelIntent::new("gpt-test"),
        messages: Vec::new(),
        attachments: vec![RemoteAttachmentSource::ExternalUrl {
            media_type: "not a mime".to_string(),
            url: "https://example.test/file".to_string(),
        }],
        tools: Vec::new(),
        tool_choice: RemoteLlmToolChoice::Auto,
        output_spec: None,
        generation: RemoteGenerationOptions::default(),
        metadata: HashMap::new(),
    };

    let error = request
        .validate()
        .expect_err("invalid MIME must fail validation");
    assert!(
        error
            .to_string()
            .contains("syntactically valid type/subtype")
    );

    request.attachments = vec![RemoteAttachmentSource::ExternalUrl {
        media_type: "audio/mpeg".to_string(),
        url: "https://example.test/file".to_string(),
    }];
    request
        .validate()
        .expect("arbitrary valid MIME is accepted");
}

#[test]
fn remote_llm_response_json_round_trips() {
    let response = RemoteLlmResponse {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        request_id: "request-1".to_string(),
        full_text: "done".to_string(),
        output_parts: vec![RemoteLlmOutputPart::Text {
            text: "done".to_string(),
            response_meta: None,
        }],
        usage: RemoteUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        terminal_reason: RemoteLlmTerminalReason::Stop,
        diagnostics: Vec::new(),
        provider_metadata: RemoteProviderMetadata::default(),
        execution_evidence: Some(RemoteExecutionEvidence {
            served_model: Some("openai/gpt-5.4-mini".to_string()),
            provider_response_id: Some("response-1".to_string()),
            provider_request_id: Some("request-1".to_string()),
            reasoning_output_tokens: Some(0),
            provider_finish_reason: Some("stop".to_string()),
        }),
    };

    response.validate().expect("valid response");
    let value = serde_json::to_value(&response).expect("serialize");
    let decoded: RemoteLlmResponse = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.full_text, "done");
}

#[test]
fn remote_turn_request_json_round_trips() {
    let request = RemoteTurnRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        idempotency_key: Some("idem".to_string()),
        input: RemoteTurnInput {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            items: vec![
                RemoteInputItem::Text {
                    text: "first".to_string(),
                },
                RemoteInputItem::Attachment {
                    source: RemoteAttachmentSource::Inline {
                        media_type: "image/png".to_string(),
                        data_base64: "AQID".to_string(),
                    },
                },
            ],
            protocol_turn_options: Some(RemoteProtocolTurnOptions {
                payload: serde_json::json!({ "answer": "raw" }),
            }),
            trace_turn_id: Some("trace".to_string()),
            prompt_layer: Some(RemotePromptLayer::new()),
        },
        tool_grants: vec![demo_grant("demo", "tools", "search")],
        metadata: HashMap::new(),
    };

    request.validate().expect("valid request");
    let value = serde_json::to_value(&request).expect("serialize");
    assert!(value.get("model_intent").is_none());
    let decoded: RemoteTurnRequest = serde_json::from_value(value).expect("deserialize");

    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.session_id, "session");
    assert!(matches!(
        &decoded.input.items[1],
        RemoteInputItem::Attachment {
            source: RemoteAttachmentSource::Inline { data_base64, .. }
        } if data_base64 == "AQID"
    ));
    assert_eq!(decoded.tool_grants.len(), 1);
}

#[test]
fn remote_turn_result_json_round_trips() {
    let result = RemoteTurnResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        status: RemoteTurnStatus::Completed,
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        cancellation: None,
        assistant_output: RemoteAssistantOutput {
            safe_text: "done".to_string(),
            raw_text: "done".to_string(),
            state: RemoteAssistantOutputState::Usable,
        },
        usage: RemoteTurnUsageSummary::default(),
        execution: RemoteExecutionSummary::default(),
        tool_calls: vec![RemoteToolCallSummary {
            call_id: Some("call".to_string()),
            tool_name: "demo".to_string(),
            args: serde_json::json!({"x": 1}),
            outcome: RemoteToolCallOutcome::Success(serde_json::json!({"ok": true})),
            duration_ms: 5,
        }],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "corr".to_string(),
            event: RemoteTurnEvent::AssistantProseDelta {
                text: "done".to_string(),
            },
        }],
        metadata: HashMap::new(),
    };

    result.validate().expect("valid result");
    let value = serde_json::to_value(&result).expect("serialize");
    let decoded: RemoteTurnResult = serde_json::from_value(value).expect("deserialize");
    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.session_id, "session");
    assert_eq!(decoded.tool_calls.len(), 1);
}

#[test]
fn model_attempt_reset_has_pinned_wire_shape() {
    let activity = RemoteTurnActivity {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sequence: 3,
        id: "reset-event".to_string(),
        correlation_id: "reset-correlation".to_string(),
        event: RemoteTurnEvent::ModelAttemptReset {
            assistant_prose_correlation_ids: vec!["prose-correlation".to_string()],
            reasoning_correlation_ids: vec!["reasoning-correlation".to_string()],
        },
    };

    assert_eq!(
        serde_json::to_value(activity).expect("serialize model attempt reset"),
        serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "sequence": 3,
            "id": "reset-event",
            "correlation_id": "reset-correlation",
            "type": "model_attempt_reset",
            "assistant_prose_correlation_ids": ["prose-correlation"],
            "reasoning_correlation_ids": ["reasoning-correlation"],
        })
    );
}

#[test]
fn remote_turn_result_requires_cancellation_evidence_iff_cancelled() {
    let mut result = RemoteTurnResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        status: RemoteTurnStatus::Cancelled,
        outcome: RemoteTurnOutcome::Stopped {
            stop: RemoteTurnStop::Cancelled,
        },
        cancellation: None,
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageSummary::default(),
        execution: RemoteExecutionSummary::default(),
        tool_calls: Vec::new(),
        issues: Vec::new(),
        activities: Vec::new(),
        metadata: HashMap::new(),
    };
    assert!(result.validate().is_err());

    result.cancellation = Some(RemoteTurnCancellationEvidence {
        request_id: "request-1".to_string(),
        origin: Some("workbench-user".to_string()),
        reason: Some("stop".to_string()),
    });
    result.validate().expect("cancelled result with evidence");

    result.status = RemoteTurnStatus::Completed;
    assert!(result.validate().is_err());
}

#[test]
fn remote_turn_cancel_envelopes_round_trip() {
    let request = RemoteTurnCancelRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        request_id: "request-1".to_string(),
        origin: Some("test-host".to_string()),
        reason: Some("superseded by newer input".to_string()),
    };
    request.validate().expect("valid cancellation request");
    let decoded: RemoteTurnCancelRequest = serde_json::from_value(
        serde_json::to_value(&request).expect("serialize cancellation request"),
    )
    .expect("deserialize cancellation request");
    assert_eq!(decoded, request);

    let mut request_without_origin = request.clone();
    request_without_origin.origin = None;
    let encoded = serde_json::to_value(&request_without_origin)
        .expect("serialize cancellation request without origin");
    assert!(encoded.get("origin").is_none());
    assert_eq!(
        serde_json::from_value::<RemoteTurnCancelRequest>(encoded)
            .expect("deserialize cancellation request without origin"),
        request_without_origin
    );

    let evidence = RemoteTurnCancellationEvidence {
        request_id: "request-1".to_string(),
        origin: Some("test-host".to_string()),
        reason: None,
    };
    for outcome in [
        RemoteTurnCancelOutcome::Requested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::AlreadyRequested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::CompletionWonRace,
        RemoteTurnCancelOutcome::UnknownOrRevoked,
    ] {
        let receipt = RemoteTurnCancelReceipt::new(
            "session",
            "turn",
            RemoteTurnControlDurabilityTier::Durable,
            outcome,
        );
        receipt.validate().expect("valid cancellation receipt");
        let decoded: RemoteTurnCancelReceipt = serde_json::from_value(
            serde_json::to_value(&receipt).expect("serialize cancellation receipt"),
        )
        .expect("deserialize cancellation receipt");
        assert_eq!(decoded, receipt);
    }
}

#[test]
fn remote_trigger_dtos_json_round_trip() {
    let request = RemoteTriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "source-key",
        serde_json::json!({ "button": "Blue" }),
        "button-blue-1",
    )
    .with_source(serde_json::json!({ "id": "blue" }));
    request
        .validate()
        .expect("valid trigger occurrence request");
    let decoded: RemoteTriggerOccurrenceRequest =
        serde_json::from_value(serde_json::to_value(&request).expect("serialize request"))
            .expect("deserialize request");
    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.source_type, "ui.button.pressed");
    assert_eq!(decoded.source.as_ref().unwrap()["id"], "blue");

    let report = RemoteTriggerEmitReport {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        occurrence_id: "occurrence:1".to_string(),
        deliveries: vec![RemoteTriggerDeliveryEmitReport {
            occurrence_id: "occurrence:1".to_string(),
            subscription_id: "subscription:1".to_string(),
            process_id: "process:1".to_string(),
            outcome: RemoteTriggerDeliveryEmitOutcome::Started,
        }],
    };
    report.validate().expect("valid report");
    let decoded: RemoteTriggerEmitReport =
        serde_json::from_value(serde_json::to_value(&report).expect("serialize report"))
            .expect("deserialize report");
    assert_eq!(decoded.deliveries[0].process_id, "process:1");

    let mut filter = RemoteTriggerSubscriptionFilter::for_source_type("ui.button.pressed");
    filter.source_key = Some("source-key".to_string());
    filter.enabled = Some(true);
    filter.validate().expect("valid filter");
    let decoded: RemoteTriggerSubscriptionFilter =
        serde_json::from_value(serde_json::to_value(&filter).expect("serialize filter"))
            .expect("deserialize filter");
    assert_eq!(decoded.source_key.as_deref(), Some("source-key"));

    let registration = RemoteTriggerRegistration {
        subscription_key: "button-watcher".to_string(),
        incarnation: "incarnation-1".to_string(),
        revision: 7,
        registrant: RemoteProcessOriginator::Host { scope: None },
        manifest_membership: RemoteTriggerManifestMembership::PresentInCurrentArtifact,
        source_key: "source-key".to_string(),
        name: Some("button watcher".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source: serde_json::json!({}),
        target: RemoteTriggerTargetSummary {
            label: Some("on_button".to_string()),
            identity: RemoteProcessIdentity {
                kind: "lashlang".to_string(),
                label: Some("on_button".to_string()),
                definition: Some(remote_process_definition_identity()),
            },
            input: RemoteProcessInput::Engine {
                kind: "lashlang".to_string(),
                payload: serde_json::json!({
                    "args": {}
                }),
            },
            inputs: remote_trigger_input_template(),
        },
        enabled: true,
    };
    let decoded: RemoteTriggerRegistration = serde_json::from_value(
        serde_json::to_value(&registration).expect("serialize registration"),
    )
    .expect("deserialize registration");
    assert_eq!(decoded.target.label.as_deref(), Some("on_button"));

    let cause = RemoteCausalRef::TriggerOccurrence {
        occurrence_id: "occurrence:1".to_string(),
        subscription_id: Some("subscription:1".to_string()),
        subscription_incarnation: Some("incarnation:1".to_string()),
        subscription_revision: Some(4),
    };
    let value = serde_json::to_value(&cause).expect("serialize cause");
    assert_eq!(value["type"], "trigger_occurrence");
    assert_eq!(value["occurrence_id"], "occurrence:1");
}

#[test]
fn session_scoped_trigger_occurrence_has_pinned_wire_shape() {
    let request = RemoteTriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "source-key",
        serde_json::json!({ "button": "Blue" }),
        "button-blue-1",
    )
    .with_source(serde_json::json!({ "id": "blue" }))
    .for_session("session-blue");

    assert_eq!(
        serde_json::to_value(request).expect("serialize session-scoped trigger occurrence"),
        serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "source_type": "ui.button.pressed",
            "source_key": "source-key",
            "payload": { "button": "Blue" },
            "idempotency_key": "button-blue-1",
            "source": { "id": "blue" },
            "session_id": "session-blue",
        })
    );
}

#[test]
fn remote_session_observation_dtos_json_round_trip_typed_kinds() {
    let observation = RemoteSessionObservation {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        cursor: "lashsc1:3:7:session".to_string(),
        turn_index: 3,
        usage: RemoteUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
    };
    observation.validate().expect("valid observation");
    let decoded: RemoteSessionObservation =
        serde_json::from_value(serde_json::to_value(&observation).expect("serialize observation"))
            .expect("deserialize observation");
    assert_eq!(decoded, observation);

    let event = RemoteSessionObservationEvent {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        replay_incarnation_id: "replay-incarnation".to_string(),
        turn_id: None,
        revision: 3,
        cursor: "lashsc1:3:7:session".to_string(),
        event: RemoteSessionObservationEventPayload::QueueChanged {
            kind: RemoteSessionQueueEventKind::Enqueued,
            batch_ids: vec!["batch-1".to_string()],
        },
    };
    event.validate().expect("valid queue event");
    let value = serde_json::to_value(&event).expect("serialize event");
    assert!(
        value.to_string().contains("\"kind\":\"enqueued\""),
        "queue kind should serialize as snake_case: {value}"
    );
    let decoded: RemoteSessionObservationEvent =
        serde_json::from_value(value).expect("deserialize event");
    assert_eq!(decoded, event);

    let process = RemoteSessionObservationEventPayload::ProcessChanged {
        kind: RemoteSessionProcessEventKind::Cancelled,
        process_ids: vec!["process-1".to_string()],
    };
    let value = serde_json::to_value(&process).expect("serialize process payload");
    assert!(
        value.to_string().contains("\"kind\":\"cancelled\""),
        "process kind should serialize as snake_case: {value}"
    );
    let decoded: RemoteSessionObservationEventPayload =
        serde_json::from_value(value).expect("deserialize process payload");
    assert_eq!(decoded, process);
}

#[test]
fn remote_process_dtos_json_round_trip() {
    let start = RemoteProcessStartRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        id: "process:1".to_string(),
        input: RemoteProcessInput::External {
            metadata: serde_json::json!({ "label": "Import" }),
        },
        disposition: RemoteRecoveryDisposition::ExternallyOwned,
        max_attempts: None,
        env_spec: Some(RemoteProcessExecutionEnvSpec {
            plugin_options: RemoteProcessPluginOptions {
                plugins: BTreeMap::from([(
                    "snapshot-tools".to_string(),
                    serde_json::json!({ "snapshot_ref": "tool-authority:sha256:abc" }),
                )]),
            },
            policy: RemoteProcessExecutionPolicy {
                provider_id: "remote-provider".to_string(),
                model: RemoteProcessModelSpec {
                    id: "remote-model".to_string(),
                    limits: RemoteProcessModelLimits {
                        context_window_tokens: 4096,
                        output_token_capacity: Some(1024),
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        }),
        originator: RemoteProcessOriginator::Session {
            scope: RemoteSessionScope::new("session"),
        },
        wake_target: Some(RemoteSessionScope::new("session")),
        grant: Some(RemoteProcessStartGrant {
            session_scope: RemoteSessionScope::new("session"),
            descriptor: RemoteProcessHandleDescriptor {
                kind: Some("external".to_string()),
                label: Some("Import".to_string()),
            },
        }),
        event_types: vec![remote_process_event_type()],
    };
    start.validate().expect("valid process start request");
    let mut invalid_max_attempts = start.clone();
    invalid_max_attempts.max_attempts = Some(0);
    assert!(matches!(
        invalid_max_attempts.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
    let decoded: RemoteProcessStartRequest =
        serde_json::from_value(serde_json::to_value(&start).expect("serialize start"))
            .expect("deserialize start");
    assert_eq!(decoded.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(decoded.id, "process:1");
    assert_eq!(
        decoded.env_spec.as_ref().unwrap().plugin_options.plugins["snapshot-tools"]["snapshot_ref"],
        "tool-authority:sha256:abc"
    );

    let record = remote_process_record();
    record
        .validate("RemoteProcessRecord")
        .expect("valid record");
    let decoded: RemoteProcessRecord =
        serde_json::from_value(serde_json::to_value(&record).expect("serialize record"))
            .expect("deserialize record");
    assert_eq!(decoded.process_id, "process:1");

    let event = remote_process_event();
    event.validate("RemoteProcessEvent").expect("valid event");
    let decoded: RemoteProcessEvent =
        serde_json::from_value(serde_json::to_value(&event).expect("serialize event"))
            .expect("deserialize event");
    assert_eq!(decoded.event_type, "process.completed");

    let snapshot = RemoteProcessWorkSnapshot {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        visible_process_ids: vec!["process:1".to_string()],
        items: vec![RemoteProcessWorkItem {
            process: RemoteObservedProcess {
                process_id: "process:1".to_string(),
                graph_key: "process:process:1".to_string(),
                kind: "external".to_string(),
                identity: RemoteProcessIdentity {
                    kind: "external".to_string(),
                    label: Some("Import".to_string()),
                    definition: None,
                },
                lifecycle: RemoteProcessLifecycleStatus::Running,
                status_label: "running".to_string(),
                terminal: false,
                disposition: RemoteRecoveryDisposition::ExternallyOwned,
                error: None,
                created_at_ms: 1,
                updated_at_ms: 2,
                first_started: None,
                lease_holder: None,
                lease_expires_at_ms: None,
                abandon_request: None,
                input: RemoteProcessInput::External {
                    metadata: serde_json::json!({ "label": "Import" }),
                },
                originator: RemoteProcessOriginator::Host { scope: None },
                env_ref: None,
                wake_target: Some(RemoteSessionScope::new("session")),
                caused_by: None,
                external_ref: None,
                wait: None,
                child_session_id: None,
                label: "Import".to_string(),
            },
            descriptor: RemoteProcessHandleDescriptor {
                kind: Some("external".to_string()),
                label: Some("Import".to_string()),
            },
            events: vec![RemoteObservedProcessEvent {
                sequence: 1,
                event_type: "process.yield".to_string(),
                occurred_at_ms: 2,
                payload: serde_json::json!({ "ok": true }),
            }],
            kind: "external".to_string(),
            label: "Import".to_string(),
        }],
    };
    snapshot.validate().expect("valid process work snapshot");

    let list_filter = RemoteProcessListFilter {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        definition: Some(remote_process_definition_identity()),
        status: RemoteProcessStatusFilter::Any,
        waiting: Some(false),
        ..RemoteProcessListFilter::default()
    };
    list_filter.validate().expect("valid process list filter");
    let list_response = RemoteProcessListResponse {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        records: snapshot
            .items
            .iter()
            .map(|item| item.process.clone())
            .collect(),
    };
    list_response.validate().expect("valid list response");

    let cancel = RemoteProcessCancelRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        reason: Some("requested by host".to_string()),
    };
    cancel.validate().expect("valid cancel request");
    let cancel_result = RemoteProcessCancelResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        status: RemoteProcessLifecycleStatus::Cancelled,
        record: Some(remote_process_record()),
    };
    cancel_result.validate().expect("valid cancel result");

    let signal = RemoteProcessSignalRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        signal_name: "ready".to_string(),
        signal_id: "signal:1".to_string(),
        payload: serde_json::json!({ "ready": true }),
        replay_key: Some("process:1:signal:ready:1".to_string()),
        wake_target_scope: Some(RemoteSessionScope::new("session")),
    };
    signal.validate().expect("valid signal request");
    let signal_result = RemoteProcessSignalResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        event: remote_process_event(),
    };
    signal_result.validate().expect("valid signal result");

    let await_request = RemoteProcessAwaitRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
    };
    await_request.validate().expect("valid await request");
    let await_result = RemoteProcessAwaitResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        output: RemoteProcessAwaitOutput::Success {
            value: serde_json::json!({ "done": true }),
            control: None,
        },
    };
    await_result.validate().expect("valid await result");

    let events_request = RemoteProcessEventsRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        after_sequence: 0,
    };
    events_request.validate().expect("valid events request");
    let events_response = RemoteProcessEventsResponse {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:1".to_string(),
        events: vec![remote_process_event()],
    };
    events_response.validate().expect("valid events response");
}

#[test]
fn remote_process_env_spec_rejects_unknown_product_metadata_fields() {
    for field in ["tool_grants", "resolved_tool_bindings"] {
        let request = serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "id": "process:1",
            "input": {
                "type": "external",
                "metadata": {}
            },
            "env_spec": {
                field: []
            },
            "originator": {
                "type": "host"
            }
        });
        let err = serde_json::from_value::<RemoteProcessStartRequest>(request)
            .expect_err("loose process env fields must be rejected");
        assert!(
            err.to_string().contains(field),
            "error should name rejected field `{field}`: {err}"
        );
    }
}

#[test]
fn remote_trigger_subscription_dtos_json_round_trip() {
    let draft = RemoteTriggerSubscriptionDraft {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        subscription_key: "button-watcher".to_string(),
        env_ref:
            "process-env:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .expect("canonical env ref"),
        wake_target: Some(RemoteSessionScope::new("session")),
        name: Some("button watcher".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: "source-key".to_string(),
        source: serde_json::json!({ "button": "blue" }),
        payload_schema: serde_json::json!({ "kind": "any" }),
        target: RemoteProcessInput::Engine {
            kind: "lashlang".to_string(),
            payload: serde_json::json!({
                "args": {}
            }),
        },
        target_identity: RemoteProcessIdentity {
            kind: "lashlang".to_string(),
            label: Some("on_button".to_string()),
            definition: Some(remote_process_definition_identity()),
        },
        event_types: vec![remote_process_event_type()],
        input_template: remote_trigger_input_template(),
        target_label: Some("on_button".to_string()),
    };
    draft.validate().expect("valid trigger draft");
    let decoded: RemoteTriggerSubscriptionDraft =
        serde_json::from_value(serde_json::to_value(&draft).expect("serialize draft"))
            .expect("deserialize draft");
    assert_eq!(decoded.source_type, "ui.button.pressed");

    let record = RemoteTriggerSubscriptionRecord {
        subscription_id: "trigger-subscription:v1:sha256:test".to_string(),
        owner_scope: RemoteTriggerOwnerScope::Session {
            session_id: "session".to_string(),
        },
        subscription_key: draft.subscription_key.clone(),
        incarnation: "incarnation-a".to_string(),
        revision: 1,
        definition_hash: "definition-hash-a".to_string(),
        registrant: RemoteProcessOriginator::Session {
            scope: RemoteSessionScope::new("session"),
        },
        env_ref: draft.env_ref.clone(),
        wake_target: draft.wake_target.clone(),
        name: draft.name.clone(),
        source_type: draft.source_type.clone(),
        source_key: draft.source_key.clone(),
        source: draft.source.clone(),
        payload_schema: draft.payload_schema.clone(),
        target: draft.target.clone(),
        target_identity: draft.target_identity.clone(),
        event_types: draft.event_types.clone(),
        input_template: draft.input_template.clone(),
        target_label: draft.target_label.clone(),
        enabled: true,
        tombstoned: false,
        deleted_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    record
        .validate("RemoteTriggerSubscriptionRecord")
        .expect("valid trigger record");

    let register = RemoteTriggerRegisterSubscriptionRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        draft,
    };
    register.validate().expect("valid register request");
    let register_result = RemoteTriggerRegisterSubscriptionResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        record: record.clone(),
    };
    register_result.validate().expect("valid register result");
    let list = RemoteTriggerListSubscriptionsResponse {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        subscriptions: vec![record],
    };
    list.validate().expect("valid trigger list");
}

#[test]
fn remote_session_observation_schema_includes_typed_kind_enums() {
    let schema = schemars::schema_for!(RemoteSessionObservationEvent);
    let schema_text = serde_json::to_value(&schema)
        .expect("schema json")
        .to_string();
    assert!(
        schema_text.contains("enqueued") && schema_text.contains("started"),
        "schema did not include typed observation kind enum values: {schema_text}"
    );
}

#[test]
fn wrong_protocol_versions_are_rejected() {
    let request = RemoteTurnRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION - 1,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        idempotency_key: None,
        input: RemoteTurnInput::text("hello"),
        tool_grants: Vec::new(),
        metadata: HashMap::new(),
    };
    assert!(matches!(
        request.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 17,
            expected: 18,
        })
    ));

    let mut input = RemoteTurnInput::text("hello");
    input.protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    assert!(matches!(
        input.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));

    let mut grant = demo_grant("one", "tools", "search");
    grant.protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    assert!(matches!(
        grant.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));

    let activity = RemoteTurnActivity {
        protocol_version: REMOTE_PROTOCOL_VERSION + 1,
        sequence: 1,
        id: "event".to_string(),
        correlation_id: "corr".to_string(),
        event: RemoteTurnEvent::AssistantProseDelta {
            text: "hi".to_string(),
        },
    };
    assert!(matches!(
        activity.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));

    let mut event = RemoteTriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "source-key",
        serde_json::Value::Null,
        "idem",
    );
    event.protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    assert!(matches!(
        event.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));

    let mut filter = RemoteTriggerSubscriptionFilter::for_session("session");
    filter.protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    assert!(matches!(
        filter.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));

    let report = RemoteTriggerEmitReport {
        protocol_version: REMOTE_PROTOCOL_VERSION + 1,
        occurrence_id: "occurrence:1".to_string(),
        deliveries: Vec::new(),
    };
    assert!(matches!(
        report.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { .. })
    ));
}

#[test]
fn nested_protocol_versions_must_match_envelope() {
    let mut request = RemoteTurnRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        idempotency_key: None,
        input: RemoteTurnInput::text("hello"),
        tool_grants: Vec::new(),
        metadata: HashMap::new(),
    };
    request.input.protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    assert!(matches!(
        request.validate(),
        Err(RemoteProtocolError::MismatchedNestedProtocolVersion { .. })
    ));
}

#[test]
fn remote_process_env_ref_is_validated_but_serializes_as_string() {
    assert_eq!(REMOTE_PROTOCOL_VERSION, 18);
    let env_ref: RemoteProcessExecutionEnvRef =
        canonical_env_ref().parse().expect("canonical env ref");
    assert_eq!(env_ref.as_str(), canonical_env_ref());
    assert_eq!(
        serde_json::to_value(&env_ref).expect("serialize env ref"),
        serde_json::json!(canonical_env_ref())
    );
    let decoded: RemoteProcessExecutionEnvRef =
        serde_json::from_value(serde_json::json!(canonical_env_ref()))
            .expect("deserialize env ref");
    assert_eq!(decoded, env_ref);

    for invalid in [
        "",
        "process-env:sha256:abc",
        "process-env:sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "tool-authority:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(
            serde_json::from_value::<RemoteProcessExecutionEnvRef>(serde_json::json!(invalid))
                .is_err(),
            "`{invalid}` should be rejected"
        );
    }
}

#[test]
fn remote_process_env_persistence_dtos_validate() {
    let request = RemotePersistProcessEnvRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        env_spec: RemoteProcessExecutionEnvSpec::default(),
    };
    request.validate().expect("valid persist env request");

    let result = RemotePersistProcessEnvResult {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        env_ref: canonical_env_ref().parse().expect("canonical env ref"),
    };
    result.validate().expect("valid persist env result");
    assert_eq!(
        serde_json::to_value(&result).expect("serialize result")["env_ref"],
        serde_json::json!(canonical_env_ref())
    );

    let mut invalid = request;
    invalid.env_spec.policy.model.limits.context_window_tokens = 0;
    assert!(matches!(
        invalid.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
}

#[test]
fn trigger_target_label_must_match_identity_label() {
    let mut draft = RemoteTriggerSubscriptionDraft::for_process(
        "label-test",
        canonical_env_ref().parse().expect("canonical env ref"),
        "ui.button.pressed",
        "source-key",
        RemoteProcessInput::External {
            metadata: serde_json::json!({}),
        },
        RemoteProcessIdentity {
            kind: "external".to_string(),
            label: Some("identity-label".to_string()),
            definition: None,
        },
    )
    .with_target_label("other-label");
    assert!(matches!(
        draft.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
    draft.target_label = Some("identity-label".to_string());
    draft.validate().expect("matching labels validate");
}

#[test]
fn top_level_protocol_schema_exports_include_versions() {
    assert_schema_has_protocol_version::<RemoteLlmRequest>();
    assert_schema_has_protocol_version::<RemoteLlmResponse>();
    assert_schema_has_protocol_version::<RemoteTurnInput>();
    assert_schema_has_protocol_version::<RemoteTurnRequest>();
    assert_schema_has_protocol_version::<RemoteTurnResult>();
    assert_schema_has_protocol_version::<RemoteSessionCursor>();
    assert_schema_has_protocol_version::<RemoteSessionObservation>();
    assert_schema_has_protocol_version::<RemoteSessionObservationEvent>();
    assert_schema_has_protocol_version::<RemoteLiveReplayGap>();
    assert_schema_has_protocol_version::<RemoteToolGrant>();
    assert_schema_has_protocol_version::<RemoteTurnActivity>();
    assert_schema_has_protocol_version::<RemoteTriggerOccurrenceRequest>();
    assert_schema_has_protocol_version::<RemoteTriggerEmitReport>();
    assert_schema_has_protocol_version::<RemoteTriggerSubscriptionFilter>();
    assert_schema_has_protocol_version::<RemoteTriggerSubscriptionDraft>();
    assert_schema_has_protocol_version::<RemoteTriggerRegisterSubscriptionRequest>();
    assert_schema_has_protocol_version::<RemoteTriggerRegisterSubscriptionResult>();
    assert_schema_has_protocol_version::<RemoteTriggerListSubscriptionsResponse>();
    assert_schema_has_protocol_version::<RemoteProcessStartRequest>();
    assert_schema_has_protocol_version::<RemoteProcessStartResult>();
    assert_schema_has_protocol_version::<RemoteProcessWorkSnapshot>();
    assert_schema_has_protocol_version::<RemoteProcessListFilter>();
    assert_schema_has_protocol_version::<RemoteProcessListResponse>();
    assert_schema_has_protocol_version::<RemoteProcessCancelRequest>();
    assert_schema_has_protocol_version::<RemoteProcessCancelResult>();
    assert_schema_has_protocol_version::<RemoteProcessSignalRequest>();
    assert_schema_has_protocol_version::<RemoteProcessSignalResult>();
    assert_schema_has_protocol_version::<RemoteProcessAwaitRequest>();
    assert_schema_has_protocol_version::<RemoteProcessAwaitResult>();
    assert_schema_has_protocol_version::<RemoteProcessEventsRequest>();
    assert_schema_has_protocol_version::<RemoteProcessEventsResponse>();
    assert_schema_has_protocol_version::<RemotePersistProcessEnvRequest>();
    assert_schema_has_protocol_version::<RemotePersistProcessEnvResult>();
}

#[test]
fn remote_tool_registry_reopen_conformance_compares_call_paths() {
    let before = VecRegistry(vec![demo_grant("one", "tools", "search")]);
    let reopened = VecRegistry(vec![demo_grant("one", "tools", "search")]);
    assert_remote_tool_registry_reopenable(&before, &reopened).expect("same registry");

    let changed = VecRegistry(vec![demo_grant("one", "tools", "read")]);
    assert!(matches!(
        assert_remote_tool_registry_reopenable(&before, &changed),
        Err(RemoteProtocolError::RemoteToolRegistryReopenMismatch { .. })
    ));
}

fn demo_grant(name: &str, module: &str, operation: &str) -> RemoteToolGrant {
    RemoteToolGrant {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        id: format!("remote-tool:{name}"),
        name: name.to_string(),
        description: "demo".to_string(),
        input_schema: default_remote_input_schema(),
        output_schema: RemoteSchemaContract::default(),
        output_contract: RemoteToolOutputContract::Static,
        examples: Vec::new(),
        activation: None,
        argument_projection: None,
        retry_policy: None,
        bindings: BTreeMap::from([(
            EXAMPLE_BINDING_KEY.to_string(),
            serde_json::json!({
                "module_path": [module],
                "operation": operation
            }),
        )]),
    }
}

fn assert_schema_has_protocol_version<T: JsonSchema>() {
    let schema = schemars::schema_for!(T);
    let schema_json = serde_json::to_value(&schema).expect("schema json");
    let schema_text = schema_json.to_string();
    assert!(
        schema_text.contains("protocol_version"),
        "schema did not include protocol_version: {schema_text}"
    );
}

#[test]
fn remote_turn_request_schema_has_no_model_intent() {
    let schema = schemars::schema_for!(RemoteTurnRequest);
    let schema_json = serde_json::to_value(&schema).expect("schema json");
    assert!(
        !schema_json.to_string().contains("model_intent"),
        "agent-turn schema must not expose a model intent: {schema_json}"
    );
}

fn canonical_env_ref() -> &'static str {
    "process-env:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}

fn remote_trigger_input_template() -> RemoteTriggerInputTemplate {
    RemoteTriggerInputTemplate::new(BTreeMap::from([
        ("event".to_string(), RemoteTriggerInputBinding::Event),
        (
            "fixed".to_string(),
            RemoteTriggerInputBinding::Fixed {
                value: serde_json::json!("blue"),
            },
        ),
    ]))
}

fn remote_process_definition_identity() -> RemoteProcessDefinitionIdentity {
    RemoteProcessDefinitionIdentity {
        value: serde_json::json!({
            "module_ref": "lashlang:v1:sha256:module",
            "host_requirements_ref": "lashlang-host-requirements:v1:sha256:host",
            "process_ref": {
                "component": "process-component",
                "pos": 1
            },
            "process_name": "main"
        }),
    }
}

fn remote_process_event_type() -> RemoteProcessEventType {
    RemoteProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: serde_json::json!({}),
        semantics: RemoteProcessEventSemanticsSpec {
            terminal: Some(RemoteProcessTerminalSpec {
                state: RemoteProcessTerminalState::Completed,
                await_output: Some(RemoteProcessValueSelector::Pointer(
                    "/await_output".to_string(),
                )),
            }),
            wake: Some(RemoteProcessWakeSpec {
                when: None,
                input: RemoteProcessValueSelector::Pointer("/text".to_string()),
                dedupe_key: RemoteProcessWakeDedupeKey::EventIdentity,
            }),
        },
    }
}

fn remote_process_record() -> RemoteProcessRecord {
    RemoteProcessRecord {
        process_id: "process:1".to_string(),
        input: RemoteProcessInput::External {
            metadata: serde_json::json!({ "label": "Import" }),
        },
        disposition: RemoteRecoveryDisposition::ExternallyOwned,
        max_attempts: None,
        identity: RemoteProcessIdentity {
            kind: "external".to_string(),
            label: Some("Import".to_string()),
            definition: None,
        },
        event_types: vec![remote_process_event_type()],
        provenance: RemoteProcessProvenance {
            originator: RemoteProcessOriginator::Host { scope: None },
            caused_by: None,
        },
        env_ref: Some(
            "process-env:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .expect("canonical env ref"),
        ),
        wake_target: Some(RemoteSessionScope::new("session")),
        created_at_ms: 1,
        updated_at_ms: 2,
        external_ref: Some(RemoteProcessExternalRef {
            backend: "worker".to_string(),
            id: "external:1".to_string(),
            metadata: None,
        }),
        first_started: None,
        abandon_request: None,
        wait: Some(RemoteProcessWaitState {
            kind: RemoteProcessWaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:1:signal.ready:1".to_string(),
                ordinal: 1,
            },
            since_ms: 2,
        }),
        status: RemoteProcessStatus::Running,
    }
}

fn remote_process_event() -> RemoteProcessEvent {
    RemoteProcessEvent {
        process_id: "process:1".to_string(),
        sequence: 1,
        event_type: "process.completed".to_string(),
        payload: serde_json::json!({ "await_output": { "type": "success", "value": true } }),
        invocation: Some(RemoteRuntimeInvocation {
            scope: RemoteRuntimeScope {
                session_id: "session".to_string(),
                turn_id: Some("turn".to_string()),
                turn_index: Some(1),
                protocol_iteration: Some(0),
            },
            subject: RemoteRuntimeSubject::ProcessEvent {
                process_id: "process:1".to_string(),
                sequence: 1,
                event_type: "process.completed".to_string(),
            },
            caused_by: Some(RemoteCausalRef::Process {
                process_id: "process:1".to_string(),
            }),
            replay: Some(RemoteRuntimeReplay {
                key: "process:1:completed".to_string(),
            }),
        }),
        semantics: RemoteProcessEventSemantics {
            terminal: Some(RemoteProcessTerminalSemantics {
                state: RemoteProcessTerminalState::Completed,
                await_output: RemoteProcessAwaitOutput::Success {
                    value: serde_json::json!(true),
                    control: None,
                },
            }),
            wake: Some(RemoteProcessWake {
                input: "wake".to_string(),
                dedupe_key: "dedupe".to_string(),
            }),
        },
        occurred_at_ms: 3,
    }
}
