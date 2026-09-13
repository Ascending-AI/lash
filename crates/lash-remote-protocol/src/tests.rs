use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::{BTreeMap, HashMap};

use schemars::JsonSchema;

use super::*;

#[path = "tests/identity.rs"]
mod identity_tests;
#[path = "tests/process_validation.rs"]
mod process_validation_tests;
mod reasoning_retention;

const EXAMPLE_BINDING_KEY: &str = "example.call_path";

#[path = "tests/version_refusal.rs"]
mod version_refusal_tests;
use version_refusal_tests::decode_empty_envelope;

#[derive(Clone)]
struct VecRegistry(Vec<RemoteToolGrant>);

impl RemoteToolRegistry for VecRegistry {
    fn grants(&self) -> Vec<RemoteToolGrant> {
        self.0.clone()
    }
}

#[test]
fn v37_llm_decode_refuses_v36_and_v35_before_new_or_malformed_vocabulary() {
    for peer_version in [36, 35] {
        for content in [
            serde_json::json!({
                "type": "text",
                "text": "captured response",
                "response_meta": {
                    "origin": {
                        "provider": "openai-compatible",
                        "endpoint": "https://gateway.example/v1",
                        "model": "shared-model"
                    }
                }
            }),
            serde_json::json!({
                "type": "future_route_bound_reasoning",
                "origin": { "endpoint": 17 }
            }),
        ] {
            let wire = serde_json::json!({
                "protocol_version": peer_version,
                "request_id": "request-old-peer",
                "scope": "malformed-on-purpose",
                "model_intent": { "model": "shared-model" },
                "messages": [{ "role": "assistant", "content": [content] }]
            })
            .to_string();

            assert!(matches!(
                RemoteLlmRequest::decode_json(wire.as_bytes()),
                Err(RemoteProtocolError::UnsupportedProtocolVersion { actual, expected })
                    if actual == peer_version && expected == REMOTE_PROTOCOL_VERSION
            ));
        }
    }
}

#[test]
fn removed_generation_options_are_rejected_rather_than_discarded() {
    for (key, value) in [
        ("top_p", serde_json::json!("0.9")),
        ("stop", serde_json::json!(["\n"])),
        ("provider_options", serde_json::json!({ "vendor": "x" })),
        ("unknown_option", serde_json::json!(1)),
    ] {
        let payload = serde_json::json!({ "output_token_cap": 128, key: value });
        let error = serde_json::from_value::<RemoteGenerationOptions>(payload)
            .expect_err("a removed generation option must not deserialize");
        assert!(
            error.to_string().contains(key),
            "error should name the rejected key {key}, got {error}"
        );
    }
}

#[test]
fn remote_attachment_media_types_are_validated_syntactically() {
    let mut request = RemoteLlmRequest {
        instructions: None,
        request_id: "request-invalid-mime".to_string(),
        scope: RemoteLlmRequestScope::new("session", "session:frame:test", "request-invalid-mime"),
        model_intent: RemoteModelIntent::new("gpt-test"),
        messages: vec![RemoteLlmMessage {
            role: RemoteLlmRole::User,
            content: vec![RemoteLlmContentBlock::Attachment {
                source: Box::new(RemoteAttachmentSource::ExternalUrl {
                    media_type: "invalid-mime".to_string(),
                    url: "https://example.test/file".to_string(),
                }),
            }],
            starts_user_segment: true,
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

    request.messages[0].content = vec![RemoteLlmContentBlock::Attachment {
        source: Box::new(RemoteAttachmentSource::ExternalUrl {
            media_type: "audio/mpeg".to_string(),
            url: "https://example.test/file".to_string(),
        }),
    }];
    request
        .validate()
        .expect("arbitrary valid MIME is accepted");
}

#[test]
fn remote_attachment_ref_rejects_every_hostile_id_shape() {
    for raw in [
        "../x",
        "/abs",
        "",
        "a\0b",
        &"a".repeat(129),
        "é",
        "e\u{301}",
        "．．／x",
    ] {
        let wire = RemoteAttachmentRef {
            id: raw.to_string(),
            media_type: "image/png".to_string(),
            byte_len: 0,
            type_metadata: None,
            label: None,
        };
        let request = serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "request_id": "hostile",
            "scope": { "session_id": "session", "agent_frame_id": "frame", "request_id": "hostile" },
            "model_intent": { "model": "model" },
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "attachment",
                    "source": { "source": "stored", "attachment_ref": &wire }
                }]
            }]
        });
        let error = RemoteLlmRequest::decode_json(&serde_json::to_vec(&request).unwrap())
            .expect_err("hostile id must fail the wire decoder itself");
        assert!(matches!(
            error,
            RemoteProtocolError::InvalidAttachmentRef { .. }
        ));
        let error = lash_core::AttachmentRef::try_from(wire)
            .expect_err("hostile id must fail remote conversion");
        assert!(matches!(
            error,
            RemoteProtocolError::InvalidAttachmentRef { .. }
        ));
    }
}

/// A peer-supplied attachment id is untrusted. Before validation moved into
/// `AttachmentId`, this wire conversion built one straight from the peer string
/// and a `../`-shaped id travelled on as a well-formed-looking value; now it is
/// refused at the boundary with a typed protocol error.
#[test]
fn remote_attachment_ref_rejects_a_peer_supplied_traversal_id() {
    let hostile = RemoteAttachmentRef {
        id: "../../etc/passwd".to_string(),
        media_type: "image/png".to_string(),
        byte_len: 3,
        type_metadata: None,
        label: None,
    };

    let error = lash_core::AttachmentRef::try_from(hostile)
        .expect_err("a traversal id must not cross the wire boundary");
    assert!(
        matches!(
            &error,
            RemoteProtocolError::InvalidAttachmentRef { id, message }
                if id == "../../etc/passwd" && message.contains("invalid attachment id")
        ),
        "unexpected error: {error:?}"
    );

    let accepted = RemoteAttachmentRef {
        id: "abc123".to_string(),
        media_type: "image/png".to_string(),
        byte_len: 3,
        type_metadata: None,
        label: None,
    };
    assert_eq!(
        lash_core::AttachmentRef::try_from(accepted)
            .expect("well-formed id is accepted")
            .id
            .as_str(),
        "abc123"
    );
}

#[test]
fn remote_llm_response_json_round_trips() {
    let response = RemoteLlmResponse {
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
            collection_interruption: None,
        }),
        generation_disposition: Some(RemoteGenerationReceipt {
            output_token_cap: RemoteGenerationOptionOutcome::Applied,
            temperature: RemoteGenerationOptionOutcome::OmittedSamplingPinned,
            seed: RemoteGenerationOptionOutcome::OmittedUnsupported,
            stop_sequences: RemoteGenerationOptionOutcome::NotRequested,
            cache: RemoteGenerationOptionOutcome::Applied,
        }),
    };

    response.validate().expect("valid response");
    let value = serde_json::to_value(Envelope::new(&response)).expect("serialize envelope");
    assert_eq!(
        value["generation_disposition"],
        serde_json::json!({
            "output_token_cap": "applied",
            "temperature": "omitted_sampling_pinned",
            "seed": "omitted_unsupported",
            "stop_sequences": "not_requested",
            "cache": "applied",
        })
    );
    let decoded = serde_json::from_value::<Envelope<RemoteLlmResponse>>(value)
        .expect("deserialize envelope")
        .into_body();
    assert_eq!(decoded.full_text, "done");
    assert_eq!(
        decoded.generation_disposition,
        response.generation_disposition
    );
}

#[test]
fn remote_turn_request_json_round_trips() {
    let request = RemoteTurnRequest {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        idempotency_key: Some("idem".to_string()),
        input: RemoteTurnInput {
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
            trace_turn_id: Some(TurnId::from("trace")),
            prompt_layer: Some(RemotePromptLayer::new()),
        },
        tool_grants: vec![demo_grant("demo", "tools", "search")],
        metadata: HashMap::new(),
    };

    request.validate().expect("valid request");
    let value: serde_json::Value = serde_json::from_slice(
        &request
            .encode_json()
            .expect("serialize turn request envelope"),
    )
    .expect("envelope json");
    assert!(value.get("model_intent").is_none());
    let decoded = RemoteTurnRequest::decode_json(
        &serde_json::to_vec(&value).expect("serialize envelope value"),
    )
    .expect("deserialize turn request envelope");

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
    let call_record = RemoteLlmCallRecord {
        call_id: "llm-call".to_string(),
        label: Some("answer".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 7,
            duration_ms: 9,
            outcome: RemoteAttemptOutcome::Interrupted,
            protocol_position: RemoteProtocolPosition::OutputStarted,
            retry_budget_consumed: true,
            retry_decision: Some(RemoteRetryDecision {
                scheduled: false,
                delay_ms: Some(0),
                reason: Some("partial output is not retryable".to_string()),
            }),
            error: Some(RemoteNormalizedError {
                class: "stream_interrupted".to_string(),
                provider_code: Some("eof".to_string()),
                http_status: None,
                provider_request_id: Some("provider-request".to_string()),
                retry_after_ms: Some(0),
            }),
            evidence: Some(RemoteExecutionEvidence {
                served_model: Some("served-model".to_string()),
                provider_response_id: Some("provider-response".to_string()),
                provider_request_id: Some("provider-request".to_string()),
                reasoning_output_tokens: Some(0),
                provider_finish_reason: None,
                collection_interruption: None,
            }),
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
    };
    let result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput {
            safe_text: "done".to_string(),
            raw_text: "done".to_string(),
            state: RemoteAssistantOutputState::Usable,
        },
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: vec![RemoteToolCallRecord {
            call_id: Some("call".to_string()),
            tool_name: "demo".to_string(),
            args: serde_json::json!({"x": 1}),
            outcome: RemoteToolCallOutcome::Success(serde_json::json!({"ok": true})),
            duration_ms: 5,
        }],
        llm_calls: vec![call_record.clone()],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "corr".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: call_record,
            },
        }],
        metadata: HashMap::new(),
    };

    result.validate().expect("valid result");
    let value: serde_json::Value = serde_json::from_slice(
        &result
            .encode_json()
            .expect("serialize turn report envelope"),
    )
    .expect("envelope json");
    assert!(
        !value
            .to_string()
            .contains("stream ended before terminal evidence"),
        "remote result and activity payloads must not publish diagnostic prose"
    );
    let decoded = RemoteTurnReport::decode_json(
        &serde_json::to_vec(&value).expect("serialize envelope value"),
    )
    .expect("deserialize turn report envelope");
    assert_eq!(decoded.session_id, "session");
    assert_eq!(decoded.tool_calls.len(), 1);
    assert_eq!(decoded.llm_calls.len(), 1);
    assert_eq!(
        value.pointer("/llm_calls/0/attempts/0"),
        Some(&serde_json::json!({
            "ordinal": 1,
            "started_at_ms": 7,
            "duration_ms": 9,
            "outcome": "interrupted",
            "protocol_position": "output_started",
            "retry_budget_consumed": true,
            "retry_decision": {
                "scheduled": false,
                "delay_ms": 0,
                "reason": "partial output is not retryable",
            },
            "error": {
                "class": "stream_interrupted",
                "provider_code": "eof",
                "provider_request_id": "provider-request",
                "retry_after_ms": 0,
            },
            "evidence": {
                "served_model": "served-model",
                "provider_response_id": "provider-response",
                "provider_request_id": "provider-request",
                "reasoning_output_tokens": 0,
            },
        }))
    );
}

#[test]
fn model_call_records_are_validated_from_result_and_activity_envelopes() {
    let valid_record = RemoteLlmCallRecord {
        call_id: "llm-call".to_string(),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 0,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: false,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
    };
    let mut activity = RemoteTurnActivity {
        sequence: 1,
        id: "event".to_string(),
        correlation_id: "correlation".to_string(),
        event: RemoteTurnEvent::ModelCallRecorded {
            record: valid_record.clone(),
        },
    };
    activity.validate().expect("valid model-call activity");
    let RemoteTurnEvent::ModelCallRecorded { record } = &mut activity.event else {
        unreachable!("constructed model-call activity")
    };
    record.call_id.clear();
    assert!(activity.validate().is_err());

    let mut result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: vec![valid_record.clone()],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            sequence: 1,
            id: "model-call".to_string(),
            correlation_id: "llm-call".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: valid_record.clone(),
            },
        }],
        metadata: HashMap::new(),
    };
    result.validate().expect("valid model-call result");
    result.llm_calls[0].attempts.clear();
    assert!(result.validate().is_err());
    result.llm_calls[0] = valid_record.clone();
    result.llm_calls[0].attempts[0].ordinal = 0;
    assert!(result.validate().is_err());
    result.llm_calls[0] = valid_record;
    result.llm_calls[0].attempts[0].error = Some(RemoteNormalizedError {
        class: String::new(),
        provider_code: None,
        http_status: None,
        provider_request_id: None,
        retry_after_ms: None,
    });
    assert!(result.validate().is_err());
}

#[test]
fn turn_result_rejects_conflicting_summary_and_activity_for_the_same_model_call() {
    let summary = RemoteLlmCallRecord {
        call_id: "same-call".to_string(),
        label: Some("foreground".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 7,
            duration_ms: 9,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: Some(RemoteExecutionEvidence {
                served_model: Some("served-model".to_string()),
                provider_response_id: Some("provider-response".to_string()),
                ..RemoteExecutionEvidence::default()
            }),
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
    };
    let activity_record = RemoteLlmCallRecord {
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 7,
            duration_ms: 9,
            outcome: RemoteAttemptOutcome::Failed,
            protocol_position: RemoteProtocolPosition::NoResponse,
            retry_budget_consumed: true,
            retry_decision: None,
            error: Some(RemoteNormalizedError {
                class: "transport".to_string(),
                provider_code: Some("connection_failed".to_string()),
                http_status: None,
                provider_request_id: None,
                retry_after_ms: None,
            }),
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }],
        ..summary.clone()
    };
    let result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: vec![summary],
        issues: Vec::new(),
        activities: vec![RemoteTurnActivity {
            sequence: 1,
            id: "model-call".to_string(),
            correlation_id: "same-call".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: activity_record,
            },
        }],
        metadata: HashMap::new(),
    };

    assert!(matches!(
        result.validate(),
        Err(RemoteProtocolError::ConflictingLlmCallRecord { call_id })
            if call_id == "same-call"
    ));
}

#[test]
fn turn_result_requires_one_summary_and_one_activity_per_model_call() {
    fn reconciled_result() -> RemoteTurnReport {
        let record = RemoteLlmCallRecord {
            call_id: "call-1".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![RemoteAttemptRecord {
                ordinal: 1,
                started_at_ms: 1,
                duration_ms: 2,
                outcome: RemoteAttemptOutcome::Completed,
                protocol_position: RemoteProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
                usage_disposition: Default::default(),
            }],
        };
        RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: vec![RemoteTurnActivity {
                sequence: 1,
                id: "event".to_string(),
                correlation_id: "call-1".to_string(),
                event: RemoteTurnEvent::ModelCallRecorded { record },
            }],
            metadata: HashMap::new(),
        }
    }

    let mut missing_activity = reconciled_result();
    missing_activity.activities.clear();
    assert!(matches!(
        missing_activity.validate(),
        Err(RemoteProtocolError::MissingLlmCallActivity { call_id }) if call_id == "call-1"
    ));

    let mut missing_summary = reconciled_result();
    missing_summary.llm_calls.clear();
    assert!(matches!(
        missing_summary.validate(),
        Err(RemoteProtocolError::MissingLlmCallSummary { call_id }) if call_id == "call-1"
    ));

    let mut duplicate_summary = reconciled_result();
    duplicate_summary
        .llm_calls
        .push(duplicate_summary.llm_calls[0].clone());
    assert!(matches!(
        duplicate_summary.validate(),
        Err(RemoteProtocolError::DuplicateLlmCallSummary { call_id }) if call_id == "call-1"
    ));

    let mut duplicate_activity = reconciled_result();
    duplicate_activity
        .activities
        .push(duplicate_activity.activities[0].clone());
    assert!(matches!(
        duplicate_activity.validate(),
        Err(RemoteProtocolError::DuplicateLlmCallActivity { call_id }) if call_id == "call-1"
    ));
}

#[test]
fn contradictory_model_call_ledgers_are_rejected_from_both_envelopes() {
    fn valid_attempt() -> RemoteAttemptRecord {
        RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 0,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        }
    }

    fn assert_rejected(attempt: RemoteAttemptRecord) {
        let record = RemoteLlmCallRecord {
            call_id: "llm-call".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts: vec![attempt],
        };
        let activity = RemoteTurnActivity {
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "correlation".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: record.clone(),
            },
        };
        assert!(activity.validate().is_err(), "activity accepted {record:?}");

        let result = RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: Vec::new(),
            metadata: HashMap::new(),
        };
        assert!(result.validate().is_err(), "result accepted {record:?}");
    }

    let normalized_error = || RemoteNormalizedError {
        class: "provider".to_string(),
        provider_code: None,
        http_status: None,
        provider_request_id: None,
        retry_after_ms: None,
    };

    let mut completed_with_error = valid_attempt();
    completed_with_error.error = Some(normalized_error());
    assert_rejected(completed_with_error);

    let mut completed_with_retry = valid_attempt();
    completed_with_retry.retry_decision = Some(RemoteRetryDecision {
        scheduled: true,
        delay_ms: Some(1),
        reason: Some("retry".to_string()),
    });
    assert_rejected(completed_with_retry);

    let mut completed_before_terminal = valid_attempt();
    completed_before_terminal.protocol_position = RemoteProtocolPosition::OutputStarted;
    assert_rejected(completed_before_terminal);

    let mut failed_without_error = valid_attempt();
    failed_without_error.outcome = RemoteAttemptOutcome::Failed;
    failed_without_error.protocol_position = RemoteProtocolPosition::NoResponse;
    assert_rejected(failed_without_error);
}

#[test]
fn valid_panic_partial_and_retry_ledgers_are_accepted_from_both_envelopes() {
    fn normalized_error(class: &str) -> RemoteNormalizedError {
        RemoteNormalizedError {
            class: class.to_string(),
            provider_code: None,
            http_status: None,
            provider_request_id: None,
            retry_after_ms: None,
        }
    }

    fn assert_accepted(attempts: Vec<RemoteAttemptRecord>) {
        let record = RemoteLlmCallRecord {
            call_id: "llm-call".to_string(),
            label: None,
            replay_drops: Vec::new(),
            attempts,
        };
        RemoteTurnActivity {
            sequence: 1,
            id: "event".to_string(),
            correlation_id: "correlation".to_string(),
            event: RemoteTurnEvent::ModelCallRecorded {
                record: record.clone(),
            },
        }
        .validate()
        .expect("valid ledger in activity envelope");
        RemoteTurnReport {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            outcome: RemoteTurnOutcome::Finished {
                finish: RemoteTurnFinish::AssistantMessage {
                    text: "done".to_string(),
                },
            },
            assistant_output: RemoteAssistantOutput::default(),
            usage: RemoteTurnUsageReport::default(),
            execution: RemoteTurnExecutionMetrics::default(),
            tool_calls: Vec::new(),
            llm_calls: vec![record.clone()],
            issues: Vec::new(),
            activities: vec![RemoteTurnActivity {
                sequence: 1,
                id: "event".to_string(),
                correlation_id: "correlation".to_string(),
                event: RemoteTurnEvent::ModelCallRecorded { record },
            }],
            metadata: HashMap::new(),
        }
        .validate()
        .expect("valid ledger in result envelope");
    }

    assert_accepted(vec![RemoteAttemptRecord {
        ordinal: 1,
        started_at_ms: 0,
        duration_ms: 1,
        outcome: RemoteAttemptOutcome::Failed,
        protocol_position: RemoteProtocolPosition::NoResponse,
        retry_budget_consumed: false,
        retry_decision: None,
        error: Some(normalized_error("provider_panicked")),
        evidence: None,
        generation_disposition: None,
        usage: None,
        usage_disposition: Default::default(),
    }]);
    assert_accepted(vec![RemoteAttemptRecord {
        ordinal: 1,
        started_at_ms: 0,
        duration_ms: 1,
        outcome: RemoteAttemptOutcome::Interrupted,
        protocol_position: RemoteProtocolPosition::OutputStarted,
        retry_budget_consumed: false,
        retry_decision: None,
        error: Some(normalized_error("stream_interrupted")),
        evidence: Some(RemoteExecutionEvidence {
            collection_interruption: Some(
                RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort,
            ),
            ..RemoteExecutionEvidence::default()
        }),
        generation_disposition: None,
        usage: None,
        usage_disposition: Default::default(),
    }]);
    assert_accepted(vec![
        RemoteAttemptRecord {
            ordinal: 1,
            started_at_ms: 0,
            duration_ms: 1,
            outcome: RemoteAttemptOutcome::Failed,
            protocol_position: RemoteProtocolPosition::NoResponse,
            retry_budget_consumed: true,
            retry_decision: Some(RemoteRetryDecision {
                scheduled: true,
                delay_ms: Some(1),
                reason: Some("retry".to_string()),
            }),
            error: Some(normalized_error("transport")),
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        },
        RemoteAttemptRecord {
            ordinal: 2,
            started_at_ms: 2,
            duration_ms: 1,
            outcome: RemoteAttemptOutcome::Completed,
            protocol_position: RemoteProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: None,
            usage: None,
            usage_disposition: Default::default(),
        },
    ]);
}

#[test]
fn turn_started_has_pinned_wire_shape_and_non_empty_identity() {
    let mut activity = RemoteTurnActivity {
        sequence: 0,
        id: "turn-start-event".to_string(),
        correlation_id: "turn-start-correlation".to_string(),
        event: RemoteTurnEvent::TurnStarted {
            turn_id: TurnId::from("physical-turn"),
        },
    };

    assert_eq!(
        serde_json::to_value(Envelope::new(activity.clone())).expect("serialize turn start"),
        serde_json::json!({
            "protocol_version": REMOTE_PROTOCOL_VERSION,
            "sequence": 0,
            "id": "turn-start-event",
            "correlation_id": "turn-start-correlation",
            "type": "turn_started",
            "turn_id": "physical-turn",
        })
    );
    activity.validate().expect("non-empty turn identity");
    let RemoteTurnEvent::TurnStarted { turn_id } = &mut activity.event else {
        unreachable!("constructed turn start activity")
    };
    *turn_id = TurnId::from("");
    assert!(activity.validate().is_err());
}

#[test]
fn model_attempt_reset_has_pinned_wire_shape() {
    let activity = RemoteTurnActivity {
        sequence: 3,
        id: "reset-event".to_string(),
        correlation_id: "reset-correlation".to_string(),
        event: RemoteTurnEvent::ModelAttemptReset {
            assistant_prose_correlation_ids: vec!["prose-correlation".to_string()],
            reasoning_correlation_ids: vec!["reasoning-correlation".to_string()],
        },
    };

    assert_eq!(
        serde_json::to_value(Envelope::new(activity)).expect("serialize model attempt reset"),
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
fn remote_turn_result_derives_status_from_its_outcome() {
    let mut result = RemoteTurnReport {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        outcome: RemoteTurnOutcome::Stopped {
            stop: RemoteTurnStop::Cancelled {
                evidence: RemoteTurnCancellationEvidence {
                    request_id: "request-1".to_string(),
                    origin: Some("workbench-user".to_string()),
                    reason: Some("stop".to_string()),
                    undelivered: RemoteTurnCancelDisposition::Defer,
                },
            },
        },
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: Vec::new(),
        issues: Vec::new(),
        activities: Vec::new(),
        metadata: HashMap::new(),
    };
    result.validate().expect("cancelled result with evidence");

    assert_eq!(result.status(), RemoteTurnStatus::Cancelled);
    let wire = serde_json::to_value(&result).unwrap();
    assert!(wire.get("status").is_none());
    assert!(wire.get("cancellation").is_none());
    result.outcome = RemoteTurnOutcome::Stopped {
        stop: RemoteTurnStop::RuntimeError,
    };
    assert_eq!(result.status(), RemoteTurnStatus::Failed);
    result.outcome = RemoteTurnOutcome::Finished {
        finish: RemoteTurnFinish::AssistantMessage {
            text: "done".into(),
        },
    };
    assert_eq!(result.status(), RemoteTurnStatus::Completed);
    let wire = result.encode_json().unwrap();
    assert_eq!(RemoteTurnReport::decode_json(&wire).unwrap(), result);
}

#[test]
fn remote_cancelled_stop_requires_and_preserves_evidence() {
    let stop = RemoteTurnStop::Cancelled {
        evidence: RemoteTurnCancellationEvidence {
            request_id: "request-1".to_string(),
            origin: Some("workbench-user".to_string()),
            reason: Some("stop".to_string()),
            undelivered: RemoteTurnCancelDisposition::Drop,
        },
    };
    let wire = serde_json::to_value(&stop).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"type":"cancelled","evidence": {
            "request_id":"request-1", "origin":"workbench-user", "reason":"stop", "undelivered":"drop"
        }})
    );
    assert_eq!(
        serde_json::from_value::<RemoteTurnStop>(wire).unwrap(),
        stop
    );
    assert!(
        serde_json::from_value::<RemoteTurnStop>(serde_json::json!({"type":"cancelled"})).is_err()
    );
}

#[test]
fn remote_turn_cancel_envelopes_round_trip() {
    let request = RemoteTurnCancelRequest {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        request_id: "request-1".to_string(),
        origin: Some("test-host".to_string()),
        reason: Some("superseded by newer input".to_string()),
        undelivered: RemoteTurnCancelDisposition::Drop,
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
        undelivered: RemoteTurnCancelDisposition::Defer,
    };
    for outcome in [
        RemoteTurnCancelOutcome::Requested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::AlreadyRequested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::PolicyConflict {
            requested: RemoteTurnCancelDisposition::Drop,
            accepted: evidence.clone(),
        },
        RemoteTurnCancelOutcome::CompletionWonRace,
        RemoteTurnCancelOutcome::UnknownOrRevoked,
    ] {
        let receipt = RemoteTurnCancelReceipt::new("session", "turn", outcome);
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
    assert_eq!(decoded.source_type, "ui.button.pressed");
    assert_eq!(decoded.source.as_ref().unwrap()["id"], "blue");
    assert_eq!(decoded.outcome, RemoteTriggerOccurrenceOutcome::Fired);

    let dropped = RemoteTriggerOccurrenceRequest::new(
        "cron.Schedule",
        "cron-source",
        serde_json::json!({ "scheduled_for": "2026-08-30T12:00:00Z" }),
        "cron-outcome-1",
    )
    .with_outcome(RemoteTriggerOccurrenceOutcome::Dropped {
        reason: "session_retired".to_string(),
    });
    assert_eq!(
        serde_json::to_value(&dropped).expect("serialize dropped occurrence"),
        serde_json::json!({
            "source_type": "cron.Schedule",
            "source_key": "cron-source",
            "payload": { "scheduled_for": "2026-08-30T12:00:00Z" },
            "idempotency_key": "cron-outcome-1",
            "outcome": { "kind": "dropped", "reason": "session_retired" },
        })
    );
    assert_eq!(
        serde_json::from_value::<RemoteTriggerOccurrenceRequest>(
            serde_json::to_value(&dropped).expect("serialize dropped occurrence again")
        )
        .expect("deserialize dropped occurrence"),
        dropped
    );

    let report = RemoteTriggerEmitReport {
        occurrence_id: "occurrence:1".to_string(),
        deliveries: vec![RemoteTriggerDeliveryEmitReceipt {
            occurrence_id: "occurrence:1".to_string(),
            subscription_id: "subscription:1".to_string(),
            process_id: ProcessId::from("process:1"),
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
        source_key: "source-key".to_string(),
        name: Some("button watcher".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source: serde_json::json!({}),
        target: RemoteTriggerTarget {
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

    let mut version_57 =
        serde_json::to_value(&registration).expect("serialize version-57 registration body");
    version_57["protocol_version"] = serde_json::json!(57);
    version_57["manifest_membership"] = serde_json::json!("present_in_current_artifact");
    let error = Envelope::<RemoteTriggerRegistration>::decode_json(
        &serde_json::to_vec(&version_57).expect("serialize version-57 registration envelope"),
    )
    .expect_err("version-57 trigger registration must be refused before body decoding");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 57,
            expected: 65,
        }
    ));

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

/// Frozen from `RemoteTriggerSubscriptionFilter` at origin/main
/// `847ba3b0b3428e8c81c4459ec9ee2d7d870ea1a4`, the immediate pre-v63
/// protocol source.
#[derive(serde::Serialize)]
struct Protocol62TriggerSubscriptionFilterEnvelope {
    protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registrant_scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<RemoteProcessDefinitionIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

#[test]
fn protocol_62_session_filter_is_refused_before_removed_field_decode() {
    let predecessor = Protocol62TriggerSubscriptionFilterEnvelope {
        protocol_version: 62,
        registrant_scope_id: None,
        session_id: Some(SessionId::from("session-blue")),
        subscription_key: None,
        name: None,
        source_type: None,
        source_key: None,
        target: None,
        enabled: None,
    };
    let wire = serde_json::to_vec(&predecessor).expect("serialize frozen version-62 filter");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&wire).expect("inspect predecessor filter"),
        serde_json::json!({
            "protocol_version": 62,
            "session_id": "session-blue",
        })
    );
    let error = Envelope::<RemoteTriggerSubscriptionFilter>::decode_json(&wire)
        .expect_err("version-62 session spelling must be refused");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 62,
            expected: 65,
        }
    ));

    assert_eq!(
        serde_json::to_value(Envelope::new(RemoteTriggerSubscriptionFilter::for_session(
            "session-blue",
        )))
        .expect("serialize canonical version-65 filter"),
        serde_json::json!({
            "protocol_version": 65,
            "registrant_scope_id": "session:session-blue",
        })
    );
}

#[test]
fn remote_protocol_65_session_filter_refuses_retired_session_id() {
    let wire = br#"{"protocol_version":65,"session_id":"session-blue"}"#;
    let error = Envelope::<RemoteTriggerSubscriptionFilter>::decode_json(wire)
        .expect_err("version-65 filter must reject the retired session_id field");
    assert!(matches!(error, RemoteProtocolError::MessageDecode(_)));
    assert!(error.to_string().contains("session_id"), "{error}");
}

#[test]
fn remote_protocol_65_session_filter_refuses_nested_duplicate_fields() {
    let wire = br#"{"protocol_version":65,"target":{"value":1,"value":2}}"#;
    let error = Envelope::<RemoteTriggerSubscriptionFilter>::decode_json(wire)
        .expect_err("version-65 envelope must preserve nested duplicate-field rejection");
    assert!(matches!(error, RemoteProtocolError::MessageDecode(_)));
    assert!(
        error.to_string().contains("duplicate field `value`"),
        "{error}"
    );
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
        serde_json::to_value(Envelope::new(request))
            .expect("serialize session-scoped trigger occurrence"),
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
        session_id: SessionId::from("session"),
        cursor: "lashsc2:replay-incarnation:3:7:session".to_string(),
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
        session_id: SessionId::from("session"),
        replay_incarnation_id: "replay-incarnation".to_string(),
        turn_id: None,
        revision: 3,
        cursor: "lashsc2:replay-incarnation:3:7:session".to_string(),
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
        process_ids: vec![ProcessId::from("process-1".to_string())],
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

/// Frozen version-41 observation-envelope reader copied from
/// `e4681ed877605ec094fc5a609f4425c6a5aeccc3`, the last pre-v42 protocol source.
/// The literal version and closed payload vocabulary are deliberately
/// independent of current protocol types.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Protocol41ObservationEnvelope {
    protocol_version: u32,
    session_id: SessionId,
    replay_incarnation_id: String,
    #[serde(default)]
    turn_id: Option<TurnId>,
    revision: u64,
    cursor: String,
    #[serde(flatten)]
    event: Protocol41ObservationSignal,
}

impl Protocol41ObservationEnvelope {
    fn decode_json(bytes: &[u8]) -> Result<Self, RemoteProtocolError> {
        #[derive(serde::Deserialize)]
        struct VersionProbe {
            protocol_version: u32,
        }

        let probe: VersionProbe = serde_json::from_slice(bytes)?;
        if probe.protocol_version != 41 {
            return Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: probe.protocol_version,
                expected: 41,
            });
        }
        Ok(serde_json::from_slice(bytes)?)
    }
}

static PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug)]
#[allow(dead_code)]
struct Protocol41ObservationSignal(Protocol41ObservationSignalShape);

impl<'de> serde::Deserialize<'de> for Protocol41ObservationSignal {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: serde::Deserializer<'de>,
    {
        PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED.store(true, std::sync::atomic::Ordering::SeqCst);
        <Protocol41ObservationSignalShape as serde::Deserialize>::deserialize(deserializer)
            .map(Self)
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum Protocol41ObservationSignalShape {
    TurnActivity {
        activity: serde_json::Value,
    },
    Committed,
    AgentFrameSwitched {
        frame_id: String,
    },
    QueueChanged {
        kind: serde_json::Value,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        kind: serde_json::Value,
        process_ids: Vec<ProcessId>,
    },
}

/// A version 41 peer has no resident-replacement signal. Its frozen envelope
/// reader must reject a current-version envelope at the version probe, before
/// its closed payload decoder can see `resident_changed`.
#[test]
fn protocol_41_peer_rejects_current_resident_changed_without_commit_fallback() {
    let resident = RemoteSessionObservationEvent {
        session_id: SessionId::from("resident-session"),
        replay_incarnation_id: "resident-incarnation".to_string(),
        turn_id: None,
        revision: 7,
        cursor: "resident-cursor".to_string(),
        event: RemoteSessionObservationEventPayload::ResidentChanged,
    };
    let wire = Envelope::new(resident)
        .encode_json()
        .expect("serialize complete current-version envelope");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&wire).expect("inspect emitted envelope"),
        serde_json::json!({
            "protocol_version": 65,
            "session_id": "resident-session",
            "replay_incarnation_id": "resident-incarnation",
            "revision": 7,
            "cursor": "resident-cursor",
            "type": "resident_changed",
        })
    );

    PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED.store(false, std::sync::atomic::Ordering::SeqCst);
    let error = Protocol41ObservationEnvelope::decode_json(&wire)
        .expect_err("version 41 reader must reject a complete current-version envelope");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 65,
            expected: 41,
        }
    ));
    assert!(
        !PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED.load(std::sync::atomic::Ordering::SeqCst),
        "version refusal must happen before the frozen payload decoder runs"
    );

    let mut mislabeled = serde_json::from_slice::<serde_json::Value>(&wire)
        .expect("inspect current-version fixture");
    mislabeled["protocol_version"] = serde_json::json!(41);
    PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED.store(false, std::sync::atomic::Ordering::SeqCst);
    let error = Protocol41ObservationEnvelope::decode_json(
        &serde_json::to_vec(&mislabeled).expect("serialize mislabeled control envelope"),
    )
    .expect_err("the v41 payload vocabulary cannot decode resident_changed");
    assert!(error.to_string().contains("unknown variant"), "{error}");
    assert!(
        PROTOCOL_41_OBSERVATION_PAYLOAD_DECODED.load(std::sync::atomic::Ordering::SeqCst),
        "the control proves the frozen payload decoder records when it is reached"
    );
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct Protocol51ProcessAwaitEnvelope {
    protocol_version: u32,
    process_id: ProcessId,
}

#[test]
fn protocol_51_process_reference_is_refused_before_incarnation_decode() {
    let predecessor = serde_json::json!({
        "protocol_version": 51,
        "process_id": "process:reused",
    });
    let error = Envelope::<RemoteProcessAwaitRequest>::decode_json(
        &serde_json::to_vec(&predecessor).expect("serialize version-51 request"),
    )
    .expect_err("version-51 process request must be refused");
    assert!(matches!(
        error,
        RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 51,
            expected: 65,
        }
    ));

    let current = Envelope::new(RemoteProcessAwaitRequest {
        process_id: ProcessId::from("process:reused"),
        incarnation: 7,
    })
    .encode_json()
    .expect("serialize current process request");
    let error = serde_json::from_slice::<Protocol51ProcessAwaitEnvelope>(&current)
        .expect_err("the frozen version-51 shape cannot consume incarnation");
    assert!(error.to_string().contains("incarnation"), "{error}");
}

#[test]
fn remote_process_dtos_json_round_trip() {
    assert_eq!(REMOTE_PROTOCOL_VERSION, 65, "remote DTO wire-shape pin");
    let start = RemoteProcessStartRequest {
        id: ProcessId::from("process:1"),
        input: RemoteProcessInput::External {
            metadata: serde_json::json!({ "label": "Import" }),
        },
        disposition: RemoteRecoveryContract::ExternallyOwned,
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
                ..RemoteProcessExecutionPolicy::new(RemoteTurnBudget::Unbounded)
            },
        }),
        originator: RemoteProcessOriginator::Session {
            session_id: SessionId::from("session"),
            agent_frame_id: Some("frame-a".to_string()),
        },
        identity: Some(RemoteProcessIdentity {
            kind: "import".to_string(),
            label: Some("Import".to_string()),
            definition: None,
        }),
        wake_session_id: Some(SessionId::from("session")),
        observers: vec![SessionId::from("session".to_string())],
        event_types: vec![remote_process_event_type()],
        lifecycle: Some(crate::RemoteProcessLifecyclePolicy {
            parent: crate::RemoteParentScope::Host,
            on_parent_end: crate::RemoteOnParentEnd::Abandon,
        }),
    };
    start.validate().expect("valid process start request");
    let mut missing_lifecycle = start.clone();
    assert!(missing_lifecycle.lifecycle.take().is_some());
    assert!(
        matches!(missing_lifecycle.validate(), Err(RemoteProtocolError::InvalidEnvelope { message, .. }) if message.contains("lifecycle"))
    );

    let mut invalid_max_attempts = start.clone();
    invalid_max_attempts.max_attempts = Some(0);
    assert!(matches!(
        invalid_max_attempts.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
    let decoded: RemoteProcessStartRequest =
        serde_json::from_value(serde_json::to_value(&start).expect("serialize start"))
            .expect("deserialize start");
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
        session_id: SessionId::from("session"),
        visible_processes: vec![RemoteProcessRef {
            process_id: ProcessId::from("process:1"),
            incarnation: 1,
        }],
        items: vec![RemoteProcessWorkItem {
            process: RemoteObservedProcess {
                process_id: ProcessId::from("process:1"),
                incarnation: 1,
                last_event_sequence: 1,
                graph_key: "process:process:1:incarnation:1".to_string(),
                kind: "external".to_string(),
                identity: RemoteProcessIdentity {
                    kind: "external".to_string(),
                    label: Some("Import".to_string()),
                    definition: None,
                },
                lifecycle: RemoteProcessStatus::Running,
                status_label: "running".to_string(),
                terminal: false,
                disposition: RemoteRecoveryContract::ExternallyOwned,
                error: None,
                created_at_ms: 1,
                updated_at_ms: 2,
                first_started: None,
                lease_holder: None,
                lease_expires_at_ms: None,
                abandon_request: None,
                cancel_request: None,
                input: RemoteProcessInput::External {
                    metadata: serde_json::json!({ "label": "Import" }),
                },
                originator: RemoteProcessOriginator::Host { scope: None },
                env_ref: None,
                caused_by: None,
                external_ref: None,
                wait: None,
                child_session_id: None,
                label: "Import".to_string(),
            },
            events: vec![RemoteObservedProcessEvent {
                sequence: 1,
                event_type: "process.yield".to_string(),
                occurred_at_ms: 2,
                payload: serde_json::json!({ "ok": true }),
            }],
            event_tail_sequence: 1,
            state: RemoteObservedWorkItemState::Coherent,
            kind: "external".to_string(),
            label: "Import".to_string(),
        }],
    };
    snapshot.validate().expect("valid process work snapshot");

    let list_filter = RemoteProcessListFilter {
        definition: Some(remote_process_definition_identity()),
        status: RemoteProcessStatusFilter::any_of([
            RemoteProcessStatus::Running,
            RemoteProcessStatus::Completed,
            RemoteProcessStatus::Failed,
            RemoteProcessStatus::Cancelled,
            RemoteProcessStatus::Abandoned,
            RemoteProcessStatus::CallerDeparted,
        ]),
        ..RemoteProcessListFilter::default()
    };
    list_filter.validate().expect("valid process list filter");
    let list_response = RemoteProcessListResponse {
        records: snapshot
            .items
            .iter()
            .map(|item| item.process.clone())
            .collect(),
    };
    list_response.validate().expect("valid list response");

    let cancel = RemoteProcessCancelRequest {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        requester: "actor:remote-host".to_string(),
    };
    cancel.validate().expect("valid cancel request");
    let cancel_result = RemoteProcessCancelReceipt {
        origin: lash_sansio::CancelOrigin::OperatorRequested,
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        status: RemoteProcessStatus::Cancelled,
        record: Some(remote_process_record()),
    };
    cancel_result.validate().expect("valid cancel result");

    let signal = RemoteProcessSignalRequest {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        signal_name: "ready".to_string(),
        signal_id: "signal:1".to_string(),
        payload: serde_json::json!({ "ready": true }),
        replay_key: Some("process:1:signal:ready:1".to_string()),
    };
    signal.validate().expect("valid signal request");
    let signal_result = RemoteProcessSignalReceipt {
        event: remote_process_event(),
    };
    signal_result.validate().expect("valid signal result");

    let await_request = RemoteProcessAwaitRequest {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
    };
    await_request.validate().expect("valid await request");
    let await_result = RemoteProcessAwaitOutcome {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        output: RemoteProcessAwaitOutput::Settled {
            output: RemoteProcessToolCallOutput {
                outcome: RemoteProcessToolCallOutcome::Success(serde_json::json!({ "done": true })),
                control: None,
            },
        },
    };
    await_result.validate().expect("valid await result");

    let events_request = RemoteProcessEventsRequest {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        after_sequence: 0,
    };
    events_request.validate().expect("valid events request");
    let events_response = RemoteProcessEventsResponse {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
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
        subscription_key: "button-watcher".to_string(),
        env_ref:
            "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
        subscription_id: "trigger-subscription:v2:blake3:test".to_string(),
        owner_scope: RemoteTriggerOwnerScope::Session {
            session_id: SessionId::from("session"),
        },
        subscription_key: draft.subscription_key.clone(),
        incarnation: "incarnation-a".to_string(),
        revision: 1,
        definition_fingerprint: "definition-hash-a".to_string(),
        registrant: RemoteProcessOriginator::Session {
            session_id: SessionId::from("session"),
            agent_frame_id: None,
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

    let register = RemoteTriggerRegisterSubscriptionRequest { draft };
    register.validate().expect("valid register request");
    let register_result = RemoteTriggerRegisterSubscriptionReceipt {
        record: record.clone(),
    };
    register_result.validate().expect("valid register result");
    let list = RemoteTriggerListSubscriptionsResponse {
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

/// The runtime-effect kinds a protocol-37 peer knew, as a closed decoder.
///
/// Version 38 adds `language_runtime_value`; a 37 peer has no name for it, so
/// the version gate has to refuse the envelope before this decoder ever sees
/// the value — exactly the property the sibling activity test pins for a new
/// event variant.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Protocol37RuntimeEffectKind {
    LlmCall,
    Direct,
    ToolAttempt,
    ToolBatch,
    ToolParentEnd,
    Process,
    Trigger,
    ExecCode,
    Checkpoint,
    SyncExecutionEnvironment,
    Sleep,
    AwaitEvent,
    PeekAwaitEvent,
}

#[test]
fn protocol_37_peer_rejects_protocol_38_language_runtime_effect_before_kind_decode() {
    let kind = serde_json::json!("language_runtime_value");
    assert_eq!(kind, serde_json::json!("language_runtime_value"));

    assert!(
        matches!(
            decode_empty_envelope(37),
            Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: 37,
                expected: 65,
            })
        ),
        "the version gate refuses a 37 peer before any payload is interpreted"
    );

    let error = serde_json::from_value::<Protocol37RuntimeEffectKind>(kind)
        .expect_err("without the version gate, the new kind is unknown to a 37 peer");
    assert!(error.to_string().contains("unknown variant"), "{error}");
}

/// The tool-intent kinds a protocol-38 peer knew, as a closed decoder.
///
/// Version 39 adds `emit_trigger`; a 38 peer has no name for it, so the
/// version gate has to refuse the envelope before this decoder ever sees the
/// value.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Protocol38ToolIntentKind {
    StartProcess,
    SignalProcess,
    CancelProcess,
    EmitProcessEvent,
}

#[test]
fn protocol_38_peer_rejects_protocol_39_emit_trigger_intent_before_kind_decode() {
    let kind = serde_json::to_value(RemoteToolIntentKind::EmitTrigger)
        .expect("serialize the version 39 intent kind");
    assert_eq!(kind, serde_json::json!("emit_trigger"));

    assert!(
        matches!(
            decode_empty_envelope(38),
            Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: 38,
                expected: 65,
            })
        ),
        "the version gate refuses a 38 peer before any payload is interpreted"
    );

    let error = serde_json::from_value::<Protocol38ToolIntentKind>(kind)
        .expect_err("without the version gate, the new kind is unknown to a 38 peer");
    assert!(error.to_string().contains("unknown variant"), "{error}");
}

/// The runtime-effect kinds a protocol-39 peer knew, as a closed decoder.
///
/// Version 39 added a tool-intent kind, not an effect kind, so a 39 peer's
/// effect-kind vocabulary is still the version-38 set. Spelling it out rather
/// than reusing the 37 decoder is what makes the refusal below a statement
/// about the version this change actually breaks.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Protocol39RuntimeEffectKind {
    LlmCall,
    Direct,
    ToolAttempt,
    ToolBatch,
    ToolParentEnd,
    Process,
    Trigger,
    ExecCode,
    Checkpoint,
    SyncExecutionEnvironment,
    Sleep,
    AwaitEvent,
    PeekAwaitEvent,
    LanguageRuntimeValue,
}

/// Version 40 adds `assistant_response_hooks`, the second phase of the staged
/// LLM-call effect boundary. Same property as the sibling above: a peer that
/// predates the variant must be refused by the version gate, never left to
/// choke on a kind it has no name for.
///
/// The expected version is pinned as a literal, not as
/// [`REMOTE_PROTOCOL_VERSION`]: a pin that reads the constant it is pinning
/// passes at every version and asserts nothing. Bumping the protocol is
/// supposed to cost an edit here.
#[test]
fn protocol_39_peer_rejects_protocol_40_assistant_response_hooks_before_kind_decode() {
    let kind = serde_json::json!("assistant_response_hooks");
    assert_eq!(kind, serde_json::json!("assistant_response_hooks"));

    assert!(
        matches!(
            decode_empty_envelope(39),
            Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: 39,
                expected: 65,
            })
        ),
        "the version gate refuses a 39 peer before any payload is interpreted"
    );

    let error = serde_json::from_value::<Protocol39RuntimeEffectKind>(kind)
        .expect_err("a 39 peer's decoder cannot name the version 40 kind");
    assert!(error.to_string().contains("unknown variant"), "{error}");

    serde_json::from_value::<Protocol39RuntimeEffectKind>(serde_json::json!(
        "language_runtime_value"
    ))
    .expect("a 39 peer does know every effect kind through version 39");
}

/// A version 40 peer's process-status decoder has no name for the version 41
/// `caller_departed` state, so it must be refused by the version gate rather
/// than left to decode a listing it would mis-classify.
///
/// The expected version is pinned as a literal for the reason stated above.
#[test]
fn protocol_40_peer_rejects_protocol_41_caller_departed_before_status_decode() {
    let status = serde_json::to_value(RemoteProcessStatus::CallerDeparted)
        .expect("serialize the version 41 process status");
    assert_eq!(status, serde_json::json!("caller_departed"));

    assert!(
        matches!(
            decode_empty_envelope(40),
            Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: 40,
                expected: 65,
            })
        ),
        "the version gate refuses a 40 peer before any payload is interpreted"
    );

    let error = serde_json::from_value::<Protocol40ProcessStatus>(status)
        .expect_err("a 40 peer's decoder cannot name the version 41 status");
    assert!(error.to_string().contains("unknown variant"), "{error}");

    serde_json::from_value::<Protocol40ProcessStatus>(serde_json::json!("abandoned"))
        .expect("a 40 peer does know every process status through version 40");
}

/// A version 40 peer's copy of `RemoteProcessStatus`, frozen for the test above.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum Protocol40ProcessStatus {
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

#[test]
fn protocol_35_peer_rejects_protocol_36_tool_intent_activity_before_variant_decode() {
    let wire = serde_json::json!({
        "protocol_version": 36,
        "sequence": 1,
        "id": "intent-outcome",
        "correlation_id": "tool-call-1",
        "type": "tool_intent_outcome",
        "outcome": {
            "status": "protocol_refused",
            "refusal": {
                "reason": "unsupported_protocol_version",
                "recorded": 2
            }
        }
    });

    assert!(matches!(
        RemoteTurnActivity::decode_json_expecting_protocol_version(wire.to_string().as_bytes(), 35),
        Err(RemoteProtocolError::UnsupportedProtocolVersion { actual, expected })
            if actual == 36 && expected == 35
    ));
}

#[test]
fn remote_process_env_ref_is_validated_but_serializes_as_string() {
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
        "process-env:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "process-env:v5:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "process-env:v6:blake3:abc",
        "process-env:v6:blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
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
        env_spec: RemoteProcessExecutionEnvSpec::new(RemoteTurnBudget::Unbounded),
    };
    request.validate().expect("valid persist env request");

    let result = RemotePersistProcessEnvReceipt {
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
fn process_execution_policy_carries_session_generation_options() {
    let mut policy = RemoteProcessExecutionPolicy {
        provider_id: "remote-provider".to_string(),
        model: RemoteProcessModelSpec {
            id: "remote-model".to_string(),
            limits: RemoteProcessModelLimits {
                context_window_tokens: 4096,
                output_token_capacity: Some(1024),
            },
            ..Default::default()
        },
        ..RemoteProcessExecutionPolicy::new(RemoteTurnBudget::Unbounded)
    };
    assert!(
        serde_json::to_value(&policy)
            .expect("serialize policy")
            .get("generation")
            .is_none(),
        "a policy expressing no generation intent must not write the key"
    );

    policy.generation = RemoteGenerationOptions {
        output_token_cap: Some(512),
        temperature: Some(serde_json::Number::from_f64(0.25).expect("finite temperature")),
        seed: Some(7),
        stop_sequences: Vec::new(),
    };
    let value = serde_json::to_value(&policy).expect("serialize policy");
    assert_eq!(
        value["generation"],
        serde_json::json!({
            "output_token_cap": 512,
            "temperature": 0.25,
            "seed": 7,
        })
    );
    let decoded: RemoteProcessExecutionPolicy =
        serde_json::from_value(value).expect("deserialize policy");
    assert_eq!(decoded, policy);

    // The env spec validates the options it carries, so a zero cap fails at
    // the boundary instead of reaching a provider.
    let mut env_spec = RemoteProcessExecutionEnvSpec {
        plugin_options: RemoteProcessPluginOptions::default(),
        policy,
    };
    env_spec
        .validate("RemoteProcessExecutionEnvSpec")
        .expect("valid env spec");
    env_spec.policy.generation.output_token_cap = Some(0);
    assert!(matches!(
        env_spec.validate("RemoteProcessExecutionEnvSpec"),
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
fn protocol_body_schemas_exclude_versions() {
    assert_schema_excludes_protocol_version::<RemoteLlmRequest>();
    assert_schema_excludes_protocol_version::<RemoteLlmResponse>();
    assert_schema_excludes_protocol_version::<RemoteTurnInput>();
    assert_schema_excludes_protocol_version::<RemoteTurnRequest>();
    assert_schema_excludes_protocol_version::<RemoteTurnCancelRequest>();
    assert_schema_excludes_protocol_version::<RemoteTurnCancelReceipt>();
    assert_schema_excludes_protocol_version::<RemoteTurnReport>();
    assert_schema_excludes_protocol_version::<RemoteSessionCursor>();
    assert_schema_excludes_protocol_version::<RemoteSessionObservation>();
    assert_schema_excludes_protocol_version::<RemoteSessionObservationEvent>();
    assert_schema_excludes_protocol_version::<RemoteLiveReplayGap>();
    assert_schema_excludes_protocol_version::<RemoteToolGrant>();
    assert_schema_excludes_protocol_version::<RemoteTurnActivity>();
    assert_schema_excludes_protocol_version::<RemoteTriggerOccurrenceRequest>();
    assert_schema_excludes_protocol_version::<RemoteTriggerEmitReport>();
    assert_schema_excludes_protocol_version::<RemoteTriggerSubscriptionFilter>();
    assert_schema_excludes_protocol_version::<RemoteTriggerSubscriptionDraft>();
    assert_schema_excludes_protocol_version::<RemoteTriggerRegisterSubscriptionRequest>();
    assert_schema_excludes_protocol_version::<RemoteTriggerRegisterSubscriptionReceipt>();
    assert_schema_excludes_protocol_version::<RemoteTriggerListSubscriptionsResponse>();
    assert_schema_excludes_protocol_version::<RemoteProcessStartRequest>();
    assert_schema_excludes_protocol_version::<RemoteProcessStartReceipt>();
    assert_schema_excludes_protocol_version::<RemoteProcessWorkSnapshot>();
    assert_schema_excludes_protocol_version::<RemoteProcessListFilter>();
    assert_schema_excludes_protocol_version::<RemoteProcessListResponse>();
    assert_schema_excludes_protocol_version::<RemoteProcessCancelRequest>();
    assert_schema_excludes_protocol_version::<RemoteProcessCancelReceipt>();
    assert_schema_excludes_protocol_version::<RemoteProcessSignalRequest>();
    assert_schema_excludes_protocol_version::<RemoteProcessSignalReceipt>();
    assert_schema_excludes_protocol_version::<RemoteProcessAwaitRequest>();
    assert_schema_excludes_protocol_version::<RemoteProcessAwaitOutcome>();
    assert_schema_excludes_protocol_version::<RemoteProcessEventsRequest>();
    assert_schema_excludes_protocol_version::<RemoteProcessEventsResponse>();
    assert_schema_excludes_protocol_version::<RemotePersistProcessEnvRequest>();
    assert_schema_excludes_protocol_version::<RemotePersistProcessEnvReceipt>();

    let envelope = schemars::schema_for!(Envelope<RemoteTurnRequest>);
    let envelope_text = serde_json::to_value(envelope)
        .expect("envelope schema json")
        .to_string();
    assert!(envelope_text.contains("protocol_version"));
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

fn assert_schema_excludes_protocol_version<T: JsonSchema>() {
    let schema = schemars::schema_for!(T);
    let schema_json = serde_json::to_value(&schema).expect("schema json");
    let schema_text = schema_json.to_string();
    assert!(
        !schema_text.contains("\"protocol_version\""),
        "body schema still includes protocol_version: {schema_text}"
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
    "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
            "module_ref": "lashlang:v2:blake3:module",
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
                status: RemoteProcessStatus::Completed,
                await_output: Some(RemoteProcessValueSelector::Pointer(
                    "/await_output".to_string(),
                )),
            }),
            wake: Some(RemoteProcessWakeSpec {
                when: None,
                input: RemoteProcessValueSelector::Pointer("/text".to_string()),
            }),
        },
    }
}

fn remote_process_record() -> RemoteProcessRecord {
    RemoteProcessRecord {
        process_id: ProcessId::from("process:1"),
        incarnation: 1,
        last_event_sequence: 0,
        input: RemoteProcessInput::External {
            metadata: serde_json::json!({ "label": "Import" }),
        },
        disposition: RemoteRecoveryContract::ExternallyOwned,
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
            "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .parse()
                .expect("canonical env ref"),
        ),
        created_at_ms: 1,
        updated_at_ms: 2,
        external_ref: Some(RemoteProcessExternalRef {
            backend: "worker".to_string(),
            id: "external:1".to_string(),
            metadata: None,
        }),
        first_started: None,
        abandon_request: None,
        cancel_request: None,
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
        outcome: None,
    lifecycle: crate::RemoteProcessLifecyclePolicy { parent: crate::RemoteParentScope::Host, on_parent_end: crate::RemoteOnParentEnd::Abandon },
}
}

fn remote_process_event() -> RemoteProcessEvent {
    RemoteProcessEvent {
        process_id: ProcessId::from("process:1"),
        process_incarnation: 1,
        sequence: 1,
        event_type: "process.completed".to_string(),
        payload: serde_json::json!({ "await_output": { "type": "success", "value": true } }),
        invocation: Some(RemoteRuntimeInvocation {
            attribution: RemoteRuntimeAttribution {
                session_id: Some(SessionId::from("session")),
                turn_id: Some(TurnId::from("turn")),
                turn_index: Some(1),
                protocol_iteration: Some(0),
            },
            subject: RemoteRuntimeSubject::ProcessEvent {
                process_id: ProcessId::from("process:1"),
                sequence: 1,
                event_type: "process.completed".to_string(),
            },
            caused_by: Some(RemoteCausalRef::Process {
                process_id: ProcessId::from("process:1"),
            }),
            replay: Some(RemoteRuntimeReplay {
                key: "process:1:completed".to_string(),
                attribution: None,
            }),
        }),
        semantics: RemoteProcessEventSemantics {
            terminal: Some(RemoteProcessTerminalSemantics {
                status: RemoteProcessStatus::Completed,
                outcome: RemoteProcessAwaitOutput::Settled {
                    output: RemoteProcessToolCallOutput {
                        outcome: RemoteProcessToolCallOutcome::Success(serde_json::json!(true)),
                        control: None,
                    },
                },
            }),
            wake: Some(RemoteProcessWake {
                input: "wake".to_string(),
            }),
        },
        occurred_at_ms: 3,
    }
}

#[test]
fn attachment_block_wire_literals_and_owned_source_are_pinned() {
    let block = RemoteLlmContentBlock::Attachment {
        source: Box::new(RemoteAttachmentSource::ExternalUrl {
            media_type: "image/png".to_string(),
            url: "https://example.test/image.png".to_string(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&block).unwrap(),
        serde_json::json!({
            "type": "attachment",
            "source": { "source": "external_url", "media_type": "image/png", "url": "https://example.test/image.png" }
        })
    );
}

mod usage_disposition_tests;
