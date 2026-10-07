use lash_sansio::SessionId;
use lash_sansio::TurnId;
use std::collections::{BTreeMap, HashMap};

use super::*;

#[test]
fn decoded_schema_contract_refuses_unknown_dialect() {
    for dialect in ["openai_tool_paramters", "custom_provider", ""] {
        let contract = serde_json::json!({
            "canonical": {"type": "object"},
            "projection": {
                "overrides": [{"dialect": dialect, "schema": {"type": "object"}}]
            }
        });
        let remote_error = serde_json::from_value::<RemoteSchemaContract>(contract.clone())
            .expect_err("unknown wire dialect must be refused before conversion");
        assert!(remote_error.to_string().contains("unknown variant"));
        let core_error = serde_json::from_value::<lash_sansio::SchemaContract>(contract)
            .expect_err("unknown contract dialect must be refused before resolution");
        assert!(core_error.to_string().contains("unknown variant"));
    }
}

#[test]
fn execution_policy_rejects_the_retired_session_id() {
    let policy = RemoteProcessExecutionPolicy::new(
        RemoteTurnBudget::Unbounded,
        std::num::NonZeroUsize::new(1024).expect("nonzero limit"),
    );
    let encoded = serde_json::to_value(&policy).expect("encode policy");
    assert!(encoded.get("session_id").is_none());
    for retired in [serde_json::Value::Null, serde_json::json!("session")] {
        let mut fields = encoded.clone();
        fields["session_id"] = retired;
        let error = serde_json::from_value::<RemoteProcessExecutionPolicy>(fields)
            .expect_err("the retired session id is refused");
        assert!(error.to_string().contains("unknown field `session_id`"));
    }
}

#[path = "tests/identity.rs"]
mod identity_tests;
#[path = "tests/model_call_ledger.rs"]
mod model_call_ledger_tests;
#[path = "tests/process_validation.rs"]
mod process_validation_tests;
mod reasoning_retention;

const EXAMPLE_BINDING_KEY: &str = "example.call_path";

#[test]
fn live_completeness_refuses_subscription_gap_reasons() {
    for reason in [
        "overflow",
        "expired",
        "subscriber_lagged",
        "cross_process",
        "invalid_cursor",
        "sequence_unbridged",
        "history_unavailable",
    ] {
        let bytes = serde_json::json!({"state": "incomplete", "reason": reason});
        assert!(
            serde_json::from_value::<RemoteProcessObservationCompleteness>(bytes).is_err(),
            "{reason}"
        );
    }
}

#[test]
fn subscription_gap_refuses_graph_incompleteness_reasons() {
    for reason in [
        "publisher_joined_mid_run",
        "incomplete_graph",
        "projection_truncated",
    ] {
        assert!(
            serde_json::from_value::<RemoteProcessObservationGapReason>(serde_json::json!(reason))
                .is_err(),
            "{reason}"
        );
    }
}

#[path = "tests/version_refusal.rs"]
mod version_refusal_tests;

#[derive(Clone)]
struct VecRegistry(Vec<RemoteToolGrant>);

impl RemoteToolRegistry for VecRegistry {
    fn grants(&self) -> Vec<RemoteToolGrant> {
        self.0.clone()
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
        model: RemoteModelConfig::new("gpt-test"),
        attachment_acceptance: Default::default(),
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
            "model": RemoteModelConfig::new("model"),
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

#[test]
fn remote_turn_result_json_round_trips() {
    let call_record = RemoteLlmCallRecord {
        call_id: "llm-call".to_string(),
        label: Some("answer".to_string()),
        replay_drops: Vec::new(),
        attempts: vec![RemoteAttemptRecord {
            ordinal: 1,
            outcome: RemoteAttemptOutcome::Interrupted,
            protocol_position: RemoteProtocolPosition::OutputStarted,
            retry_budget_consumed: true,
            retry_decision: Some(RemoteRetryDecision::Declined(
                lash_sansio::llm::types::RetryDeclineCause::NotRetryable,
            )),
            error: Some(RemoteNormalizedError {
                class: RemoteProviderFailureKind::Stream,
                code: Some(lash_sansio::FailureCode::provider("eof")),
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
            call_id: lash_core::ToolCallId::fixture("call"),
            provider_call_id: None,
            tool_name: "demo".to_string(),
            args: serde_json::json!({"x": 1}),
            output: RemoteToolCallOutput {
                outcome: RemoteToolCallOutcome::Success(serde_json::json!({"ok": true})),
                control: None,
                view: None,
                projection_value: None,
            },
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
            .encode_json(&crate::negotiation::test_negotiated())
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
            "outcome": "interrupted",
            "protocol_position": "output_started",
            "retry_budget_consumed": true,
            "retry_decision": {
                "outcome": "declined",
                "cause": "not_retryable",
            },
            "error": {
                "class": "stream",
                "code": "provider:eof",
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
fn remote_cancelled_stop_requires_and_preserves_evidence() {
    let stop = RemoteTurnStop::Cancelled {
        evidence: RemoteTurnCancellationEvidence {
            request_id: "request-1".to_string(),
            origin: Some("workbench-user".to_string()),
            reason: Some("stop".to_string()),
            undelivered: RemoteTurnCancelUndeliveredInputPolicy::Drop,
            mode: RemoteTurnCancelMode::AfterStep,
            honoured_after_step: Some(7),
        },
    };
    let wire = serde_json::to_value(&stop).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"type":"cancelled","evidence": {
            "request_id":"request-1", "origin":"workbench-user", "reason":"stop", "undelivered":"drop",
            "mode":"after_step", "honoured_after_step":7
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
    let request_schema =
        serde_json::to_value(schemars::schema_for!(RemoteTurnCancelRequest)).unwrap();
    let request_schema = jsonschema::validator_for(&request_schema).unwrap();
    let receipt_schema =
        serde_json::to_value(schemars::schema_for!(RemoteTurnCancelReceipt)).unwrap();
    let receipt_schema = jsonschema::validator_for(&receipt_schema).unwrap();
    let request = RemoteTurnCancelRequest {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        request_id: "request-1".to_string(),
        origin: Some("test-host".to_string()),
        reason: Some("superseded by newer input".to_string()),
        undelivered: RemoteTurnCancelUndeliveredInputPolicy::Drop,
        mode: RemoteTurnCancelMode::AfterStep,
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
    assert_eq!(encoded["mode"], "after_step");
    assert!(request_schema.is_valid(&encoded));
    assert_eq!(
        serde_json::from_value::<RemoteTurnCancelRequest>(encoded)
            .expect("deserialize cancellation request without origin"),
        request_without_origin
    );

    let mut immediate = serde_json::to_value(&request).unwrap();
    immediate["mode"] = serde_json::json!("immediate");
    assert!(request_schema.is_valid(&immediate));
    assert_eq!(
        serde_json::from_value::<RemoteTurnCancelRequest>(immediate.clone())
            .unwrap()
            .mode,
        RemoteTurnCancelMode::Immediate,
    );
    immediate["mode"] = serde_json::json!("unsupported");
    assert!(!request_schema.is_valid(&immediate));
    assert!(serde_json::from_value::<RemoteTurnCancelRequest>(immediate).is_err());

    let evidence = RemoteTurnCancellationEvidence {
        request_id: "request-1".to_string(),
        origin: Some("test-host".to_string()),
        reason: None,
        undelivered: RemoteTurnCancelUndeliveredInputPolicy::Defer,
        mode: RemoteTurnCancelMode::AfterStep,
        honoured_after_step: Some(7),
    };
    for outcome in [
        RemoteTurnCancelOutcome::Requested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::AlreadyRequested {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::Escalated {
            cancellation: evidence.clone(),
        },
        RemoteTurnCancelOutcome::PolicyConflict {
            requested: RemoteTurnCancelUndeliveredInputPolicy::Drop,
            accepted: evidence.clone(),
        },
        RemoteTurnCancelOutcome::CompletionWonRace,
        RemoteTurnCancelOutcome::UnknownOrRevoked,
    ] {
        let receipt = RemoteTurnCancelReceipt::new("session", "turn", outcome);
        receipt.validate().expect("valid cancellation receipt");
        let encoded = serde_json::to_value(&receipt).expect("serialize cancellation receipt");
        assert!(receipt_schema.is_valid(&encoded));
        let decoded: RemoteTurnCancelReceipt =
            serde_json::from_value(encoded).expect("deserialize cancellation receipt");
        assert_eq!(decoded, receipt);
    }
    let malformed = serde_json::json!({
        "session_id": "session", "turn_id": "turn", "outcome": {
            "outcome": "escalated", "cancellation": {
                "request_id": "request-1", "mode": "after_step", "honoured_after_step": "seven"
            }
        }
    });
    assert!(!receipt_schema.is_valid(&malformed));
    assert!(serde_json::from_value::<RemoteTurnCancelReceipt>(malformed).is_err());
}

#[test]
fn remote_protocol_92_session_filter_refuses_retired_session_id() {
    let wire = br#"{"protocol_version":100,"session_id":"session-blue"}"#;
    let error =
        Envelope::<RemoteTriggerSubscriptionFilter>::decode_json(wire, crate::REMOTE_PROTOCOL)
            .expect_err("current-version filter must reject the retired session_id field");
    assert!(matches!(error, RemoteProtocolError::MessageDecode(_)));
    assert!(error.to_string().contains("session_id"), "{error}");
}

#[test]
fn remote_protocol_92_session_filter_refuses_nested_duplicate_fields() {
    // Re-pinned for FIG-2992: a trigger filter's `target` is now the bare
    // engine-owned definition value, and an opaque JSON value cannot reject a
    // duplicate key. The property being pinned — nested duplicate-field
    // rejection inside a typed DTO — is pinned on the process-list filter's
    // typed originator selector instead.
    let wire = br#"{"protocol_version":100,"originator":{"type":"host","scope":"a","scope":"b"}}"#;
    let error = Envelope::<RemoteProcessListFilter>::decode_json(wire, crate::REMOTE_PROTOCOL)
        .expect_err("current-version envelope must preserve nested duplicate-field rejection");
    assert!(matches!(error, RemoteProtocolError::MessageDecode(_)));
    assert!(
        error.to_string().contains("duplicate field `scope`"),
        "{error}"
    );
}

#[test]
fn retired_unbounded_process_event_request_is_refused() {
    let legacy = serde_json::json!({
        "process_id": lash_sansio::ProcessId::fixture("process:1"),
        "limit": 1,
        "mode": "full",
        "after_sequence": 0
    });
    let mut current = legacy.clone();
    current
        .as_object_mut()
        .expect("request object")
        .remove("after_sequence");
    serde_json::from_value::<RemoteProcessEventsRequest>(current)
        .expect("required fields form a valid request");
    let error = serde_json::from_value::<RemoteProcessEventsRequest>(legacy)
        .expect_err("the retired field must be refused");
    assert!(error.to_string().contains("after_sequence"), "{error}");
}

#[test]
fn remote_process_env_spec_rejects_unknown_product_metadata_fields() {
    for field in ["tool_grants", "resolved_tool_bindings"] {
        let request = serde_json::json!({"env_spec": {field: []}});
        let err = serde_json::from_value::<RemotePersistProcessEnvRequest>(request)
            .expect_err("loose process env fields must be rejected at publication");
        assert!(
            err.to_string().contains(field),
            "error should name rejected field `{field}`: {err}"
        );
    }
}

#[test]
fn remote_process_starts_reject_inline_environment_specs() {
    let request = serde_json::json!({
        "input": {"type": "engine", "kind": "job", "payload": {}},
        "lifetime": {"type": "detached"},
        "originator": {"type": "host"},
        "env_spec": {},
    });
    let error = serde_json::from_value::<RemoteProcessStartRequest>(request)
        .expect_err("the inline predecessor shape has no compatibility path");
    assert!(error.to_string().contains("env_spec"), "{error}");
}

/// FIG-1951: the window-79 wire spelled a subscription's lifecycle as an
/// `enabled`/`tombstoned`/`deleted_at_ms` triple with eight representable
/// combinations and three legal ones, and no validator looked at any of them —
/// a peer could assert any of the five invalid triples and the conversion
/// copied them straight into a core record. The tagged enum makes each of the
/// five a decode refusal rather than a value.
#[test]
fn trigger_subscription_lifecycle_refuses_every_invalid_window_79_triple() {
    fn lifecycle_from(value: serde_json::Value) -> Result<RemoteTriggerSubscriptionLifecycle, ()> {
        serde_json::from_value::<RemoteTriggerSubscriptionLifecycle>(value).map_err(|_| ())
    }

    // The three legal states, in the shape this window writes.
    assert_eq!(
        lifecycle_from(serde_json::json!({ "lifecycle": "enabled" })),
        Ok(RemoteTriggerSubscriptionLifecycle::Enabled)
    );
    assert_eq!(
        lifecycle_from(serde_json::json!({ "lifecycle": "disabled" })),
        Ok(RemoteTriggerSubscriptionLifecycle::Disabled)
    );
    assert_eq!(
        lifecycle_from(serde_json::json!({
            "lifecycle": "tombstoned",
            "deleted_at_ms": 17u64,
        })),
        Ok(RemoteTriggerSubscriptionLifecycle::Tombstoned(17))
    );

    // The five invalid states a window-79 peer could assert, each now refused.
    for invalid in [
        // A tombstone with no deletion time.
        serde_json::json!({ "lifecycle": "tombstoned" }),
        // A live row claiming a deletion time.
        serde_json::json!({ "lifecycle": "enabled", "deleted_at_ms": 17u64 }),
        serde_json::json!({ "lifecycle": "disabled", "deleted_at_ms": 17u64 }),
        // A routable tombstone: the old `enabled = true, tombstoned = true`.
        serde_json::json!({ "lifecycle": "enabled_tombstoned" }),
        // The window-79 triple itself carries no tag at all.
        serde_json::json!({ "enabled": true, "tombstoned": true, "deleted_at_ms": 17u64 }),
    ] {
        assert!(
            lifecycle_from(invalid.clone()).is_err(),
            "the wire must refuse {invalid}"
        );
    }
}

/// A window-79 record body — the three flat fields, no `lifecycle` tag — must
/// be refused outright rather than defaulted into an enabled subscription.
#[test]
fn a_window_79_trigger_subscription_record_is_refused() {
    let mut body = serde_json::json!({
        "subscription_id": "trigger-subscription:v2:blake3:test",
        "owner_scope": { "scope": "session", "session_id": "session" },
        "subscription_key": "key",
        "incarnation": "incarnation-a",
        "revision": 1,
        "definition_fingerprint": "definition-hash-a",
        "registrant": { "origin": "session", "session_id": "session" },
        "env_ref": canonical_env_ref(),
        "source_type": "ui.button.pressed",
        "source_key": "source-key",
        "created_at_ms": 1,
        "updated_at_ms": 2,
        "enabled": true,
        "tombstoned": false,
    });
    assert!(
        serde_json::from_value::<RemoteTriggerSubscriptionRecord>(body.take()).is_err(),
        "a window-79 record body must not decode"
    );
}

#[test]
fn remote_process_env_persistence_dtos_validate() {
    let request = RemotePersistProcessEnvRequest {
        env_spec: RemoteProcessExecutionEnvSpec::new(
            RemoteTurnBudget::Unbounded,
            lash_sansio::MaxToolCalls::new(1024).non_zero(),
        ),
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
    invalid.env_spec.policy.model = Some(RemoteModelConfig {
        key: "remote-key".to_string(),
        metadata: RemoteLlmProfileMetadata {
            wire_model: "remote-model".to_string(),
            extra_body: Default::default(),
            request_defaults: Default::default(),
            capability: Default::default(),
            limits: RemoteProcessModelLimits {
                context_window_tokens: 0,
                output_tokens: Default::default(),
            },
        },
        reasoning: Default::default(),
    });
    assert!(matches!(
        invalid.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
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
        argument_projection: None,
        execution_policy: None,
        bindings: BTreeMap::from([(
            EXAMPLE_BINDING_KEY.to_string(),
            serde_json::json!({
                "module_path": [module],
                "operation": operation
            }),
        )]),
    }
}

fn canonical_env_ref() -> &'static str {
    "process-env:v6:blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}

fn remote_process_record() -> RemoteProcessRecord {
    // Engine input, not External: core refuses a record that captures an
    // execution env for a declarative input kind, and remote ingress now
    // refuses the same shape (FIG-2985). Keeping the env ref here is what
    // exercises its round trip.
    RemoteProcessRecord {
        process_id: lash_sansio::ProcessId::fixture("process:1"),
        start_key_digest: None,
        last_event_sequence: 0,
        input: RemoteProcessInput::Engine {
            kind: "import".to_string(),
            payload: serde_json::json!({ "label": "Import" }),
        },
        identity: RemoteProcessIdentity {
            kind: "engine".to_string(),
            label: Some("Import".to_string()),
            definition_id: None,
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
        engine_config: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        external_ref: Some(RemoteProcessExternalRef {
            backend: "worker".to_string(),
            id: "external:1".to_string(),
            metadata: None,
            segment_ordinal: None,
        }),
        first_started: None,
        cancel_request: None,
        lifecycle: RemoteProcessLifecycleState::Waiting {
        wait: RemoteProcessWaitState {
            kind: RemoteProcessWaitKind::Signal {
                name: "ready".to_string(),
                event_type: "signal.ready".to_string(),
                key: "process:1:signal.ready:1".to_string(),
                ordinal: 1,
            },
            since_ms: 2,
        },
        },
    lifetime: crate::RemoteLifetimeDecision::Detached,
    ancestry: Vec::new(),
    session_capability: None,
    trace: None,
}
}

#[test]
fn remote_trigger_registration_refuses_non_engine_target() {
    let mut draft = RemoteTriggerSubscriptionDraft::for_process(
        "engine-only",
        canonical_env_ref().parse().expect("canonical env ref"),
        "source",
        "key",
        RemoteProcessStartTarget::Input(
            RemoteProcessInput::try_from(lash_core::ProcessInput::SessionTurn {
                definition_key: "child".to_string(),
                create_request: Box::new(lash_core::SessionCreateRequest::root(
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )),
                turn_input: Box::new(lash_core::TurnInput::empty()),
                result: lash_core::SessionTurnOutcome::Turn,
            })
            .expect("a session-turn input converts"),
        ),
        RemoteProcessIdentity {
            kind: "session_turn".to_string(),
            label: None,
            definition_id: None,
        },
    );
    assert!(matches!(
        draft.validate(),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
    assert!(matches!(
        lash_core::TriggerSubscriptionDraft::try_from(draft.clone()),
        Err(RemoteProtocolError::InvalidEnvelope { .. })
    ));
    draft.target = RemoteProcessStartTarget::Input(RemoteProcessInput::Engine {
        kind: "engine".to_string(),
        payload: serde_json::json!({}),
    });
    draft
        .validate()
        .expect("an Engine target passes wire registration");
}

#[test]
fn trigger_emit_receipts_reject_impossible_process_outcomes() {
    for value in [
        serde_json::json!({"occurrence_id": "occ", "subscription_id": "sub", "outcome": "started"}),
        serde_json::json!({"occurrence_id": "occ", "subscription_id": "sub", "process_id": "process", "outcome": {"failed": {"code": "trigger_route_revoked", "reason": "revoked"}}}),
    ] {
        assert!(serde_json::from_value::<RemoteTriggerDeliveryEmitReceipt>(value).is_err());
    }
    for value in [
        serde_json::json!({"occurrence_id": "occ", "subscription_id": "sub", "outcome": "started"}),
        serde_json::json!({"occurrence_id": "occ", "subscription_id": "sub", "process_id": "process", "outcome": {"failed": {"code": "trigger_route_revoked", "reason": "revoked"}}}),
    ] {
        assert!(
            serde_json::from_value::<lash_core::facade_support::TriggerDeliveryEmitReceipt>(value)
                .is_err()
        );
    }
}

#[test]
fn trigger_occurrences_always_record_their_outcome() {
    let remote = RemoteTriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), "id");
    let core =
        lash_core::TriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), "id");
    for mut value in [
        serde_json::to_value(remote).expect("remote occurrence"),
        serde_json::to_value(core).expect("core occurrence"),
    ] {
        assert_eq!(value["outcome"], serde_json::json!({"kind": "fired"}));
        value
            .as_object_mut()
            .expect("occurrence object")
            .remove("outcome");
        assert!(serde_json::from_value::<RemoteTriggerOccurrenceRequest>(value.clone()).is_err());
        assert!(serde_json::from_value::<lash_core::TriggerOccurrenceRequest>(value).is_err());
    }
}

fn remote_process_event_type() -> RemoteProcessEventType {
    RemoteProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: lash_sansio::JsonSchema::any(),
        semantics: RemoteProcessEventSemanticsSpec {
            terminal: Some(RemoteProcessTerminalSpec {
                status: RemoteTerminalProcessStatus::Completed,
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

/// S10 F2: the wire and journal carry the same sole subscription record.
#[test]
fn trigger_mutation_receipt_round_trip_has_one_record() {
    let session_id = lash_core::SessionId::from("receipt-session");
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "receipt-key",
        lash_core::ProcessExecutionEnvRef::new(canonical_env_ref()),
        "timer.tick",
        "timer-key",
        lash_core::ProcessInput::Engine {
            kind: "fixture".into(),
            payload: serde_json::json!({}),
        },
        lash_core::ProcessIdentity::new("fixture"),
    );
    let outcome = lash_core::facade_support::evaluate_trigger_mutation(
        None,
        lash_core::TriggerCommand::Register {
            owner_scope: lash_core::TriggerOwnerScope::session(&session_id),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(&session_id)),
            draft,
        },
        "receipt-register",
        1,
    )
    .expect("registration")
    .expect("mutation");
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("mutation")
    };
    let remote = RemoteTriggerMutationReceipt::try_from(*receipt.clone()).expect("wire conversion");
    let json = serde_json::to_value(&remote).expect("wire receipt");
    assert_eq!(json.as_object().expect("receipt record").len(), 2);
    assert_eq!(
        json["record"]["incarnation"],
        serde_json::json!(receipt.incarnation())
    );
    let decoded: RemoteTriggerMutationReceipt =
        serde_json::from_value(json.clone()).expect("wire decode");
    let restored = lash_core::TriggerMutationReceipt::try_from(decoded).expect("core conversion");
    assert_eq!(restored, *receipt);
    let mut contradictory = json;
    contradictory["incarnation"] = serde_json::json!("different-incarnation");
    assert!(serde_json::from_value::<RemoteTriggerMutationReceipt>(contradictory).is_err());
}
