use lash_sansio::{ProcessId, SessionId, TurnId};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;

#[path = "core_conversions_tests/process_fixtures.rs"]
mod process_fixtures;
use process_fixtures::*;

#[path = "core_conversions_tests/cancellation.rs"]
mod cancellation;

#[path = "core_conversions_tests/generation_receipt.rs"]
mod generation_receipt;

#[path = "core_conversions_tests/reasoning_capability.rs"]
mod reasoning_capability;

#[path = "core_conversions_tests/observation_projection.rs"]
mod observation_projection;

#[path = "core_conversions_tests/registration_parity.rs"]
mod registration_parity;

const EXAMPLE_BINDING_KEY: &str = "example.call_path";

fn unbounded_policy() -> lash_core::SessionPolicy {
    lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
        lash_core::MaxToolCalls::new(1024),
    )
}

#[test]
fn runtime_replay_round_trip_retains_minting_emission_key() {
    let replay = lash_core::runtime::RuntimeReplay {
        key: "tool-intent:derived".to_string(),
        attribution: Some(lash_core::RuntimeReplayAttribution::ToolIntent(
            lash_core::ToolIntentIdentity {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                execution_scope_id: "turn".to_string(),
                tool_call_id: lash_core::ToolCallId::fixture("call"),
                intent_index: 0,
                replay_key: "intent:derived".to_string(),
                minting_emission_replay_key: Some("emission:minting".to_string()),
            },
        )),
    };

    let round_trip =
        lash_core::runtime::RuntimeReplay::from(RemoteRuntimeReplay::from(replay.clone()));
    assert_eq!(round_trip, replay);
}

#[test]
fn turn_input_remote_conversion_drops_runtime_correlation() {
    struct Correlation;

    let mut input = lash_core::TurnInput::text("remote words");
    input.trace_turn_id = Some(TurnId::from("remote-attempt"));
    input.turn_context.set_runtime_correlation(Correlation);
    let retained = input.clone();
    let remote = RemoteTurnInput::try_from(input).expect("runtime correlation stays local");
    let decoded = lash_core::TurnInput::try_from(remote).expect("remote input converts back");
    assert!(
        matches!(&decoded.items[..], [lash_core::InputItem::Text { text }] if text == "remote words")
    );
    assert_eq!(decoded.trace_turn_id, Some(TurnId::from("remote-attempt")));
    assert!(
        decoded
            .turn_context
            .runtime_correlation::<Correlation>()
            .is_none()
    );
    assert!(
        retained
            .turn_context
            .runtime_correlation::<Correlation>()
            .is_some()
    );
}

#[test]
fn llm_request_and_response_round_trip_owned_dtos() {
    let request = core_llm::LlmRequest {
        instructions: Some(Arc::from("I")),
        model: llm_profile_passthrough::request_profile(),
        messages: vec![core_llm::LlmMessage::new(
            core_llm::LlmRole::User,
            vec![
                core_llm::LlmContentBlock::Text {
                    text: "hello".into(),
                    response_meta: None,
                    cache_breakpoint: false,
                },
                core_llm::LlmContentBlock::Attachment {
                    source: Box::new(core_llm::AttachmentSource::inline(
                        lash_core::MediaType::parse("image/png").unwrap(),
                        vec![1, 2, 3],
                    )),
                },
            ],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(vec![core_llm::LlmToolSpec {
            name: "search".to_string(),
            description: "Search".to_string(),
            input_schema: lash_core::SchemaContract::admit(serde_json::json!({
                "type": "object",
                "properties": { "raw": { "const": "x" } }
            }))
            .expect("valid declared schema")
            .with_override(
                lash_core::SchemaDialect::OpenaiToolParameters,
                lash_sansio::JsonSchema::admit(serde_json::json!({
                    "type": "object",
                    "properties": { "raw": { "type": "string", "enum": ["x"] } }
                }))
                .expect("valid declared projection schema"),
            ),
            output_schema: lash_sansio::SchemaContract::admit(serde_json::json!({}))
                .expect("valid declared schema"),
        }]),
        tool_choice: core_llm::LlmToolChoice::Auto,
        attachment_acceptance: Default::default(),
        generation: core_llm::GenerationOptions {
            output_token_cap: NonZeroUsize::new(42),
            temperature: Some(
                core_llm::NonNegativeFiniteF64::new(0.25).expect("finite temperature"),
            ),
            seed: Some(-9),
            stop_sequences: Vec::new(),
            parallel_tool_calls: Some(false),
            projection_provenance: Default::default(),
        },
        scope: core_llm::LlmRequestScope {
            attempt: Some(2),
            ..core_llm::LlmRequestScope::new(
                "session-1",
                "session-1:frame:test",
                "session-1:request:test",
            )
            .with_turn(
                lash_sansio::RunId::parse("run-1").expect("run id"),
                lash_sansio::TurnId::parse("turn-2").expect("turn id"),
            )
        },
        output_spec: Some(core_llm::LlmOutputSpec::JsonObject),
        stream_events: None,
        provider_trace: None,
    };

    let remote = RemoteLlmRequest::from_core("request-1", request);
    let remote_json = serde_json::to_value(&remote).expect("serialize remote request");
    assert_eq!(
        remote_json["model"]["reasoning"],
        serde_json::json!({ "effort": "fast" })
    );
    assert_eq!(
        remote_json["model"]["metadata"]["capability"]["reasoning"]["disable"],
        serde_json::json!(true)
    );
    assert_eq!(
        remote_json["model"]["metadata"]["capability"]["cache_control"],
        serde_json::json!("anthropic")
    );
    assert_eq!(
        remote_json["model"]["metadata"]["extra_body"],
        serde_json::json!({"host_option":{"enabled":true}})
    );
    let remote: RemoteLlmRequest =
        serde_json::from_value(remote_json).expect("deserialize remote request");
    remote.validate().expect("valid remote request");
    assert_eq!(remote.request_id, "request-1");
    assert_eq!(remote.scope.agent_frame_id, "session-1:frame:test");
    let core = core_llm::LlmRequest::try_from(remote).expect("core request");
    // FIG-5219: the call's typed turn, Run and attempt cross the wire typed.
    assert_eq!(
        core.scope.turn,
        Some(core_llm::LlmTurnScope {
            run: lash_sansio::RunId::parse("run-1").expect("run id"),
            turn_id: lash_sansio::TurnId::parse("turn-2").expect("turn id"),
        })
    );
    assert_eq!(core.scope.attempt, Some(2));
    assert_eq!(core.instructions.as_deref(), Some("I"));
    assert_eq!(
        core.model.metadata().capability.instruction_role,
        core_llm::InstructionRole::Developer
    );
    assert!(
        core.model
            .metadata()
            .capability
            .native_mid_conversation_system
    );
    assert_eq!(core.model.wire_model(), "gpt-test");
    assert_eq!(
        core.model.metadata().extra_body["host_option"],
        serde_json::json!({"enabled":true})
    );
    assert_eq!(
        core.model.reasoning,
        core_llm::ReasoningSelection::Effort("fast".to_string())
    );
    let reasoning = core
        .model
        .metadata()
        .capability
        .reasoning
        .as_ref()
        .expect("capability must round-trip");
    assert_eq!(reasoning.efforts, vec!["fast", "slow"]);
    assert!(reasoning.disable);
    assert_eq!(
        core.model.metadata().capability.cache_control,
        Some(core_llm::CacheControlDialect::Anthropic)
    );
    assert_eq!(
        reasoning.encoding,
        core_llm::ReasoningEncoding::Budget(std::collections::BTreeMap::from([
            ("fast".to_string(), 1024u32),
            ("slow".to_string(), 2048u32)
        ]))
    );
    assert_eq!(core.generation.output_token_cap, NonZeroUsize::new(42));
    assert_eq!(
        core.generation.temperature,
        Some(core_llm::NonNegativeFiniteF64::new(0.25).expect("finite temperature"))
    );
    assert_eq!(core.generation.seed, Some(-9));
    assert_eq!(core.generation.parallel_tool_calls, Some(false));
    assert_eq!(core.session_id(), "session-1");
    assert_eq!(core.agent_frame_id(), "session-1:frame:test");
    assert_eq!(core.request_id(), "session-1:request:test");
    assert!(matches!(
        &core.attachments()[0],
        core_llm::AttachmentSource::Inline { bytes, .. } if bytes == &[1, 2, 3]
    ));
    assert_eq!(
        core.tools[0].input_schema.projection.overrides[0].dialect,
        lash_core::SchemaDialect::OpenaiToolParameters
    );

    let response_metadata = BTreeMap::from([
        ("body:/cost".to_string(), serde_json::json!(0.000063)),
        (
            "header:x-opper-cost".to_string(),
            serde_json::json!("0.000008"),
        ),
    ]);
    let response = core_llm::LlmResponse {
        parts: vec![core_llm::LlmOutputPart::Text {
            text: "done".to_string(),
            response_meta: None,
        }],
        usage: core_llm::LlmUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        terminal_reason: core_llm::LlmTerminalReason::Stop,
        terminal_diagnostic: Some("ok".to_string()),
        provider_usage: Some(serde_json::json!({"provider": "usage"})),
        request_body: Some("{}".to_string()),
        http_summary: Some("200".to_string()),
        execution_evidence: Some(core_llm::ExecutionEvidence {
            served_model: Some("openai/gpt-5.4-mini".to_string()),
            provider_response_id: Some("response-1".to_string()),
            provider_request_id: Some("request-1".to_string()),
            reasoning_output_tokens: Some(0),
            provider_finish_reason: Some("stop".to_string()),
            collection_interruption: None,
        }),
        generation_disposition: None,
        response_metadata: response_metadata.clone(),
        expose_thinking: Some(false),
    };
    let remote = RemoteLlmResponse::from_core("request-1", response);
    remote.validate().expect("valid remote response");
    assert_eq!(remote.provider_metadata.data, response_metadata);
    let core = core_llm::LlmResponse::from(remote);
    assert_eq!(core.full_text(), "done");
    assert_eq!(core.terminal_reason, core_llm::LlmTerminalReason::Stop);
    assert_eq!(
        core.execution_evidence
            .as_ref()
            .and_then(|evidence| evidence.reasoning_output_tokens),
        Some(0)
    );
    assert_eq!(
        core.provider_usage,
        Some(serde_json::json!({"provider": "usage"}))
    );
    assert_eq!(core.response_metadata, response_metadata);
}

#[path = "core_conversions_tests/llm_profile_passthrough.rs"]
mod llm_profile_passthrough;

#[test]
fn process_start_requests_round_trip_core_values() {
    let held = lash_core::ProcessStartRequest::new(
        lash_core::testing::held_engine_input(serde_json::json!({ "label": "Held" })),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_wake_session_id(Some(SessionId::from("session-a")))
    .with_observers([SessionId::from("session-a")])
    .with_event_types([process_event_type()]);
    assert_process_start_roundtrip(held.clone());
    // A host start's session-lookup grant crosses as `until_session`.
    let mut until_session = held.clone();
    until_session.originator =
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new("session-a"));
    until_session.lifetime = lash_core::LifetimeDecision::Until {
        scope: lash_core::ScopeId::Session(SessionId::from("session-a")),
        grant: lash_core::ScopeGrant::HostSessionLookup,
    };
    assert_process_start_roundtrip(until_session);
    // A runtime start's ancestor grant never crosses the wire as a start.
    for scope in [
        lash_core::ScopeId::turn(
            SessionId::from("session-a"),
            lash_core::TurnId::from("turn-a"),
        ),
        lash_core::ScopeId::session_operation(SessionId::from("session-a"), "drain-a".to_string()),
        lash_core::ScopeId::process(lash_sansio::ProcessId::fixture("parent")),
        lash_core::ScopeId::Session(SessionId::from("session-a")),
    ] {
        let mut scoped = held.clone();
        scoped.lifetime = lash_core::LifetimeDecision::Until {
            scope,
            grant: lash_core::ScopeGrant::Ancestor,
        };
        assert!(
            RemoteProcessStartRequest::try_from(scoped).is_err(),
            "an ancestor grant is a runtime fact a remote start cannot request"
        );
    }

    let lashlang = lash_core::ProcessStartRequest::new(
        engine_process_input("main", serde_json::json!({ "event": true })),
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new("session-a")),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        (lash_core::ProcessExecutionEnvSpec::new(
            {
                let mut config =
                    lash_core::PluginConfig::for_protocol(Some("protocol".to_string()));
                config.insert(
                    "snapshot-tools",
                    serde_json::json!({ "snapshot_ref": "tool-authority:sha256:abc" }),
                );
                lash_core::AdmittedPluginConfig::new(config, 3)
            },
            lash_core::SessionPolicy {
                model: Some(lash_core::LlmProfileConfig::new(
                    lash_core::RecordedLlmProfile::mint(
                        lash_core::LlmProfileKey::from("process-model"),
                        lash_core::LlmProfileMetadata::builder("process-model")
                            .context_window_tokens(4096)
                            .output_token_capacity(512)
                            .build()
                            .expect("model"),
                    ),
                )),
                generation: lash_core::GenerationOptions {
                    output_token_cap: std::num::NonZeroUsize::new(256),
                    temperature: Some(
                        lash_core::NonNegativeFiniteF64::new(0.25).expect("finite temperature"),
                    ),
                    seed: Some(4242),
                    stop_sequences: Vec::new(),
                    parallel_tool_calls: None,
                    projection_provenance: Default::default(),
                },
                ..unbounded_policy()
            },
        ))
        .stable_ref()
        .expect("captured environment digest"),
    )
    .with_event_types([process_event_type()]);
    assert_process_start_roundtrip(lashlang);

    let session_turn = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "remote-session-turn:v1".to_string(),
            create_request: Box::new(
                lash_core::SessionCreateRequest::child_session(
                    "session-a",
                    lash_core::SessionStartPoint::Empty,
                    Default::default(),
                )
                .with_session_id("child-session"),
            ),
            turn_input: Box::new(lash_core::TurnInput::text("hello child")),
            result: lash_core::SessionTurnOutcome::FinalValue {
                schema: Some(
                    lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "object" }))
                        .expect("valid final-value schema"),
                ),
            },
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    );
    assert_process_start_roundtrip(session_turn);
}

#[test]
fn process_await_output_keeps_code_value_and_display_projection_distinct() {
    let structured = serde_json::json!({"channels":[{"name":"engineering"}]});
    let envelope = serde_json::json!({"structuredContent":structured,"content":[]});
    let core = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(envelope.clone())
            .with_projection_value(structured.clone()),
    );
    let remote = RemoteProcessAwaitOutput::try_from(core.clone()).expect("remote output");
    let encoded = serde_json::to_vec(&remote).expect("encode remote");
    let decoded =
        serde_json::from_slice::<RemoteProcessAwaitOutput>(&encoded).expect("decode remote");
    let restored = lash_core::ProcessAwaitOutput::try_from(decoded).expect("core output");
    assert_eq!(restored, core);
    let lash_core::ProcessAwaitOutput::Settled { output } = restored else {
        panic!("settled output");
    };
    assert_eq!(output.value_for_projection(), structured);
    assert!(
        matches!(output.outcome, lash_core::ToolCallOutcome::Success(value)
        if value.to_json_value() == envelope)
    );
}

#[test]
fn process_list_cancel_signal_and_await_requests_convert_to_core_commands() {
    let filter = lash_core::ProcessListFilter {
        definition_id: Some(lash_sansio::ProcessDefinitionId::from_sha256_digest(
            [1; 32],
        )),
        status: lash_core::ProcessStatusFilter::any_of([lash_core::ProcessStatus::Waiting]),
        originator: Some(lash_core::ProcessOriginatorFilter::session("test")),
        until: Some(lash_core::ScopeId::turn(
            lash_sansio::SessionId::from("test"),
            lash_core::TurnId::from("turn-1"),
        )),
        cancel_pending_before_ms: Some(99),
        identity_kind: Some("engine".to_string()),
        identity_label: Some("Main".to_string()),
        caused_by_occurrence_id: Some("occurrence-1".to_string()),
        caused_by_subscription_id: Some("subscription-1".to_string()),
        created_at_start_ms: Some(10),
        created_at_end_ms: Some(20),
        retired_since_ms: Some(15),
    };
    let remote = RemoteProcessListFilter::from(filter.clone());
    remote.validate().expect("valid list filter");
    let core = lash_core::ProcessListFilter::try_from(remote).expect("core filter");
    assert_eq!(core.status, filter.status);
    assert_eq!(core.originator, filter.originator);
    assert_eq!(core.until, filter.until);
    assert_eq!(
        core.cancel_pending_before_ms,
        filter.cancel_pending_before_ms
    );
    assert_eq!(core.identity_kind, filter.identity_kind);
    assert_eq!(core.identity_label, filter.identity_label);
    assert_eq!(core.caused_by_occurrence_id, filter.caused_by_occurrence_id);
    assert_eq!(
        core.caused_by_subscription_id,
        filter.caused_by_subscription_id
    );
    assert_eq!(core.created_at_start_ms, filter.created_at_start_ms);
    assert_eq!(core.created_at_end_ms, filter.created_at_end_ms);
    assert_eq!(core.retired_since_ms, filter.retired_since_ms);
    assert!(core.definition_id.is_some());

    let cancel = RemoteProcessCancelRequest {
        process_id: lash_sansio::ProcessId::fixture("process:cancel"),
        requester: "actor:remote-host".to_string(),
    };
    cancel.validate().expect("valid cancel");
    let command = lash_core::ProcessCommand::from(cancel);
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Cancel { process_id, .. }
            if process_id == lash_sansio::ProcessId::fixture("process:cancel")
    ));

    let signal = RemoteProcessSignalRequest {
        process_id: lash_sansio::ProcessId::fixture("process:signal"),
        signal_name: "ready".to_string(),
        signal_id: "signal:1".to_string(),
        payload: serde_json::json!({ "ok": true }),
        trace_cause: Default::default(),
    };
    // The conversion keeps the whole identity and derives the append key
    // from it: no caller-selected key survives the crossing (FIG-4299).
    let admitted = lash_core::ProcessSignal::try_from(signal.clone()).expect("signal");
    assert_eq!(
        admitted.identity.process_id(),
        &lash_sansio::ProcessId::fixture("process:signal")
    );
    assert_eq!(admitted.identity.signal_name(), "ready");
    assert_eq!(admitted.identity.signal_id(), "signal:1");
    let append = admitted.append_request();
    assert_eq!(append.event_type, "signal.ready");
    assert_eq!(
        append.replay.map(|replay| replay.key),
        Some(lash_core::facade_support::process_signal_wait_key(
            &lash_sansio::ProcessId::fixture("process:signal"),
            "ready",
            "signal:1",
        ))
    );
    let command = lash_core::ProcessCommand::try_from(signal.clone()).expect("signal command");
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Signal { signal } if signal == admitted
    ));
    // A request that still names its own replay key is refused, not
    // silently re-keyed.
    let mut keyed = serde_json::to_value(&signal).expect("encode the signal request");
    serde_json::from_value::<RemoteProcessSignalRequest>(keyed.clone())
        .expect("the request round-trips");
    keyed["replay_key"] = serde_json::json!("caller-selected");
    assert!(serde_json::from_value::<RemoteProcessSignalRequest>(keyed).is_err());

    let await_request = RemoteProcessAwaitRequest {
        process_id: lash_sansio::ProcessId::fixture("process:await"),
    };
    await_request.validate().expect("valid await");
    let command = lash_core::ProcessCommand::from(await_request);
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Await { process_id }
            if process_id == lash_sansio::ProcessId::fixture("process:await")
    ));
}

#[test]
fn settled_report_keeps_authoritative_model_records_when_live_activities_are_absent() {
    let session = lash_sansio::SessionId::fixture("reattached-consumer");
    let mut turn = lash_core::testing::mock_assembled_turn(&session, "settled answer");
    turn.llm_calls.push(synthetic_terminal_call_record(
        "recorded-call",
        lash_core::AttemptOutcome::Failed,
        lash_core::ProviderFailureKind::Validation,
        "InvalidParameter",
        false,
    ));
    let report = RemoteTurnReport::from_core(
        session,
        lash_sansio::TurnId::fixture("admitted-run"),
        turn,
        [],
    );
    assert!(
        report.validate().is_ok(),
        "reattached settled report must remain transportable: {:?}",
        report.validate()
    );
    assert_eq!(report.llm_calls.len(), 1);
    assert_eq!(
        report
            .activities
            .iter()
            .filter(|activity| matches!(activity.event, RemoteTurnEvent::ModelCallRecorded { .. }))
            .count(),
        1
    );
}

fn synthetic_terminal_call_record(
    call_id: &str,
    outcome: lash_core::AttemptOutcome,
    failure_kind: lash_core::ProviderFailureKind,
    code: &str,
    retry_budget_consumed: bool,
) -> lash_core::LlmCallRecord {
    lash_core::LlmCallRecord {
        call_id: lash_core::LlmCallId(call_id.to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![lash_core::AttemptRecord {
            ordinal: 1,
            outcome,
            protocol_position: lash_core::ProtocolPosition::NoResponse,
            retry_budget_consumed,
            retry_decision: None,
            error: Some(lash_core::NormalizedError {
                class: failure_kind,
                code: Some(lash_core::FailureCode::provider(code)),
                http_status: None,
                provider_request_id: None,
                retry_after: None,
            }),
            evidence: None,
            generation_disposition: None,
            usage: None,
        }],
    }
}

fn assert_terminal_call_record_converts_and_validates(
    record: lash_core::LlmCallRecord,
    cancelled: bool,
) {
    let activity = RemoteTurnActivity::from_core(
        1,
        lash_core::TurnActivity::independent(lash_core::TurnEvent::ModelCallRecorded {
            record: record.clone(),
        }),
    )
    .expect("model call recorded activity");
    activity
        .validate()
        .expect("ModelCallRecorded conversion validates");
    let activity_json = activity
        .encode_json(&crate::negotiation::test_negotiated())
        .expect("encode activity envelope");
    RemoteTurnActivity::decode_json(&activity_json).expect("activity decoder validates");

    let turn = lash_core::facade_support::AssembledTurn {
        turn_input_acceptance: None,
        turn_cancel_input_outcome: Default::default(),
        state: lash_core::SessionSnapshot::new("session".into(), unbounded_policy()),
        outcome: if cancelled {
            lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::Cancelled {
                    evidence: lash_core::facade_support::TurnCancellationEvidence {
                        request_id: "cancel-request".to_string(),
                        origin: None,
                        reason: None,
                        undelivered:
                            lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer,
                        mode: lash_core::facade_support::TurnCancelMode::Immediate,
                        honoured_after_step: None,
                    },
                },
            )
        } else {
            lash_core::facade_support::TurnOutcome::Stopped(
                lash_core::facade_support::TurnStop::ProviderError,
            )
        },
        assistant_output: lash_core::facade_support::AssistantOutput {
            safe_text: String::new(),
            raw_text: String::new(),
            state: lash_core::facade_support::OutputState::EmptyOutput,
        },
        execution: lash_core::facade_support::TurnExecutionMetrics::default(),
        token_usage: lash_core::TokenUsage::default(),
        llm_calls: vec![record],
        tool_calls: Vec::new(),
        omitted: None,
        retained_outputs: Vec::new(),
        failure_evidence: Vec::new(),
        errors: Vec::new(),
    };
    let result = RemoteTurnReport::from_core("session", "turn", turn, [activity]);
    result
        .validate()
        .expect("turn-result conversion validates the terminal call record");
}

#[test]
fn task_join_failure_record_converts_and_validates() {
    assert_terminal_call_record_converts_and_validates(
        synthetic_terminal_call_record(
            "join-failure-call",
            lash_core::AttemptOutcome::Interrupted,
            lash_core::ProviderFailureKind::Unknown,
            "task_join_failed",
            true,
        ),
        false,
    );
}

#[test]
fn invalid_endpoint_record_converts_and_validates() {
    assert_terminal_call_record_converts_and_validates(
        synthetic_terminal_call_record(
            "invalid-endpoint-call",
            lash_core::AttemptOutcome::Failed,
            lash_core::ProviderFailureKind::Validation,
            "invalid_provider_endpoint",
            false,
        ),
        false,
    );
}

#[test]
fn attempt_records_expose_only_structured_failure_facts() {
    const PRIVATE_DIAGNOSTIC: &str = "secret provider panic: token=raw-secret";
    let record = lash_core::LlmCallRecord {
        call_id: lash_core::LlmCallId("panic-call".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![lash_core::AttemptRecord {
            ordinal: 1,
            outcome: lash_core::AttemptOutcome::Failed,
            protocol_position: lash_core::ProtocolPosition::NoResponse,
            retry_budget_consumed: true,
            retry_decision: None,
            error: Some(lash_core::NormalizedError {
                class: lash_sansio::llm::types::ProviderFailureKind::Unknown,
                code: Some(lash_core::FailureCode::provider("provider_panicked")),
                http_status: None,
                provider_request_id: None,
                retry_after: None,
            }),
            evidence: None,
            generation_disposition: None,
            usage: None,
        }],
    };

    let mut legacy_record = serde_json::to_value(&record).expect("serialize core record");
    legacy_record["attempts"][0]["error"]["diagnostic"] = serde_json::json!(PRIVATE_DIAGNOSTIC);
    let record: lash_core::LlmCallRecord =
        serde_json::from_value(legacy_record).expect("read old attempt record");
    let remote_record = RemoteLlmCallRecord::from(record.clone());
    let activity = RemoteTurnActivity::from_core(
        1,
        lash_core::TurnActivity::independent(lash_core::TurnEvent::ModelCallRecorded { record }),
    )
    .expect("model call recorded activity");
    for value in [
        serde_json::to_value(remote_record).expect("serialize remote result record"),
        serde_json::to_value(activity).expect("serialize remote activity"),
    ] {
        let encoded = value.to_string();
        assert!(!encoded.contains(PRIVATE_DIAGNOSTIC));
        assert!(!encoded.contains("raw-secret"));
        assert!(encoded.contains("provider_panicked"));
    }
}

#[test]
fn remote_tool_grants_validate_explicit_bindings_and_duplicates() {
    let grant = demo_grant("one", "tools", "search");
    grant.validate().expect("valid grant");
    assert_eq!(
        grant.binding_call_path(EXAMPLE_BINDING_KEY).unwrap(),
        "tools.search"
    );

    let mut missing_binding = grant.clone();
    missing_binding.bindings.remove(EXAMPLE_BINDING_KEY);
    assert!(matches!(
        missing_binding.binding_call_path(EXAMPLE_BINDING_KEY),
        Err(RemoteProtocolError::MissingToolBinding { .. })
    ));

    let duplicate = demo_grant("two", "tools", "search");
    assert!(matches!(
        RemoteToolGrant::validate_all(&[grant, duplicate]),
        Err(RemoteProtocolError::DuplicateRemoteCallPath { .. })
    ));
}

#[test]
fn remote_tool_grants_convert_explicit_core_ids_without_binding_call_path() {
    let grant = demo_grant("one", "tools", "search");
    let definition = lash_core::ToolDefinition::try_from(&grant).expect("tool definition");
    assert_eq!(definition.manifest().id.as_str(), "remote-tool:one");
    assert_eq!(
        definition.manifest().bindings[EXAMPLE_BINDING_KEY],
        grant.bindings[EXAMPLE_BINDING_KEY],
        "remote bindings remain opaque metadata on the manifest"
    );

    let changed_binding = demo_grant("one", "other_module", "other_operation");
    assert_eq!(
        changed_binding
            .binding_call_path(EXAMPLE_BINDING_KEY)
            .expect("changed binding call path"),
        "other_module.other_operation"
    );
    let definition =
        lash_core::ToolDefinition::try_from(&changed_binding).expect("tool definition");
    assert_eq!(
        definition.manifest().id.as_str(),
        "remote-tool:one",
        "remote grant IDs are independent of binding call paths"
    );
    assert_ne!(
        definition.manifest().id.as_str(),
        "remote-tool:other_module.other_operation"
    );

    let mut renamed = grant;
    renamed.name = "renamed_one".to_string();
    let definition = lash_core::ToolDefinition::try_from(&renamed).expect("tool definition");
    assert_eq!(
        definition.manifest().id.as_str(),
        "remote-tool:one",
        "remote grant IDs are stable across model-facing renames"
    );
}

#[test]
fn journaled_process_lifecycle_kinds_keep_their_sequence_on_the_wire() {
    for event_type in [
        "process.first_started",
        "process.waiting",
        "process.resumed",
        "process.cancel_requested",
        "process.completed",
        "process.failed",
        "process.cancelled",
        "process.abandoned",
    ] {
        let local = lash_core::SessionProcessEventKind::from_durable_event(event_type, 17)
            .expect("journaled lifecycle maps to session observation");
        let remote = RemoteSessionProcessEventKind::from(local);
        let value = serde_json::to_value(remote).expect("encode lifecycle kind");
        assert_eq!(value["sequence"], 17);
        assert_eq!(
            serde_json::from_value::<RemoteSessionProcessEventKind>(value.clone())
                .expect("decode lifecycle kind"),
            remote,
        );
        let mut unknown = value;
        unknown["retired"] = serde_json::json!(true);
        assert!(serde_json::from_value::<RemoteSessionProcessEventKind>(unknown).is_err());
    }
    assert!(lash_core::SessionProcessEventKind::from_durable_event("custom.event", 1).is_none());
}

#[test]
fn remote_activity_exposes_typed_turn_input_application_without_display_text() {
    let application = lash_core::TurnInputApplication {
        input_id: lash_core::InputId::from("input-1"),
        source_key: Some("host:source-1".to_string()),
        turn_id: lash_core::TurnId::from("turn-1"),
        committed_message_id: "message-1".to_string(),
        checkpoint: Some(lash_core::CheckpointKind::BeforeCompletion),
    };
    let activity =
        lash_core::TurnActivity::independent(lash_core::TurnEvent::QueuedInputAccepted {
            applications: vec![application],
        });

    let remote =
        RemoteTurnActivity::from_core(5, activity).expect("queued input accepted activity");
    remote.validate().expect("typed application validates");
    assert_eq!(
        remote.event,
        RemoteTurnEvent::TurnInputApplied {
            applications: vec![RemoteTurnInputApplication {
                input_id: "input-1".to_string(),
                source_key: Some("host:source-1".to_string()),
                turn_id: TurnId::from("turn-1"),
                committed_message_id: "message-1".to_string(),
                checkpoint: Some(RemoteTurnInputCheckpoint::BeforeCompletion),
            }],
        }
    );
    let json = serde_json::to_value(remote).expect("serialize typed application");
    assert_eq!(
        json.pointer("/type").and_then(serde_json::Value::as_str),
        Some("turn_input_applied")
    );
    assert!(
        json.get("kind").is_none() && !json.to_string().contains("queued_input_accepted"),
        "application evidence must not use an untyped diagnostic: {json}"
    );
    assert!(
        !json.to_string().contains("display") && !json.to_string().contains("text"),
        "application evidence must carry identity only: {json}"
    );
}

#[test]
fn remote_turn_activity_sink_writes_exact_newline_delimited_json() {
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct FlushTrackingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl std::io::Write for FlushTrackingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    let activities = [
        lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("activity-1"),
            correlation_id: lash_core::TurnActivityId::new("correlation-1"),
            event: lash_core::TurnEvent::AssistantProseDelta {
                text: "hello".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        },
        lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("activity-2"),
            correlation_id: lash_core::TurnActivityId::new("correlation-2"),
            event: lash_core::TurnEvent::ReasoningDelta {
                text: "checking".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        },
    ];
    let expected = activities
        .iter()
        .cloned()
        .enumerate()
        .map(|(sequence, activity)| {
            serde_json::to_string(&Envelope::at(
                &crate::negotiation::test_negotiated(),
                RemoteTurnActivity::from_core(sequence as u64, activity)
                    .expect("remote turn activity"),
            ))
            .expect("serialize expected remote activity")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let (result_tx, result_rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let sink = RemoteTurnActivitySink::new(
            FlushTrackingWriter::default(),
            0,
            crate::negotiation::test_negotiated(),
        );
        runtime.block_on(async {
            for activity in activities {
                lash_core::facade_support::TurnActivitySink::emit(&sink, activity).await;
            }
        });
        let errors = sink.take_errors();
        let writer = sink.into_inner().expect("remote sink writer lock");
        let _ = result_tx.send((writer, errors));
    });

    let (writer, errors) = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("remote activity sink timed out (possible writer-lock deadlock)");
    assert!(errors.is_empty(), "remote sink errors: {errors:?}");
    assert_eq!(writer.bytes, expected.as_bytes());
    assert_eq!(writer.flushes, 2, "each activity must be flushed");
    assert!(
        writer.bytes.ends_with(b"\n"),
        "NDJSON must be newline-terminated"
    );

    let lines = std::str::from_utf8(&writer.bytes)
        .expect("NDJSON is UTF-8")
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let activity =
            Envelope::<RemoteTurnActivity>::decode_json(line.as_bytes(), crate::REMOTE_PROTOCOL)
                .expect("each NDJSON line is one remote activity envelope")
                .into_body();
        activity.validate().expect("valid remote activity body");
    }
}

#[test]
fn remote_turn_activity_sink_records_write_error_and_continues_with_later_events() {
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct FailFirstWriter {
        bytes: Vec<u8>,
        flushes: usize,
        failed_once: bool,
    }

    impl std::io::Write for FailFirstWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.failed_once {
                self.failed_once = true;
                return Err(std::io::Error::other("simulated write failure"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    let activities = [
        lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("activity-1"),
            correlation_id: lash_core::TurnActivityId::new("correlation-1"),
            event: lash_core::TurnEvent::AssistantProseDelta {
                text: "first".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        },
        lash_core::TurnActivity {
            id: lash_core::TurnActivityId::new("activity-2"),
            correlation_id: lash_core::TurnActivityId::new("correlation-2"),
            event: lash_core::TurnEvent::ReasoningDelta {
                text: "second".into(),
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
            },
        },
    ];

    let (result_tx, result_rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let sink = RemoteTurnActivitySink::new(
            FailFirstWriter::default(),
            0,
            crate::negotiation::test_negotiated(),
        );
        runtime.block_on(async {
            for activity in activities {
                lash_core::facade_support::TurnActivitySink::emit(&sink, activity).await;
            }
        });
        let errors = sink.take_errors();
        let writer = sink.into_inner().expect("remote sink writer lock");
        let _ = result_tx.send((writer, errors));
    });

    let (writer, errors) = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("remote activity sink timed out");
    assert_eq!(errors.len(), 1, "expected one write error");
    assert!(
        errors[0].contains("simulated write failure"),
        "unexpected error message: {}",
        errors[0]
    );

    let lines = std::str::from_utf8(&writer.bytes)
        .expect("NDJSON is UTF-8")
        .lines()
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "only second activity should be written");
    let activity: RemoteTurnActivity =
        serde_json::from_str(lines[0]).expect("valid remote activity");
    assert_eq!(
        activity.sequence, 1,
        "sequence must advance past the failed first activity"
    );
    assert_eq!(activity.id, "activity-2");
}

#[test]
fn remote_turn_activity_sink_records_flush_error() {
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct FailFlushWriter {
        bytes: Vec<u8>,
    }

    impl std::io::Write for FailFlushWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated flush failure"))
        }
    }

    let activity = lash_core::TurnActivity {
        id: lash_core::TurnActivityId::new("activity-1"),
        correlation_id: lash_core::TurnActivityId::new("correlation-1"),
        event: lash_core::TurnEvent::AssistantProseDelta {
            text: "flush test".into(),
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
        },
    };

    let (result_tx, result_rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let sink = RemoteTurnActivitySink::new(
            FailFlushWriter::default(),
            0,
            crate::negotiation::test_negotiated(),
        );
        runtime.block_on(async {
            lash_core::facade_support::TurnActivitySink::emit(&sink, activity).await;
        });
        let errors = sink.take_errors();
        let writer = sink.into_inner().expect("remote sink writer lock");
        let _ = result_tx.send((writer, errors));
    });

    let (_writer, errors) = result_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("remote activity sink timed out");
    assert_eq!(errors.len(), 1, "expected one flush error");
    assert!(
        errors[0].contains("simulated flush failure"),
        "unexpected error message: {}",
        errors[0]
    );
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
        execution_ms: 30_000,
        bindings: BTreeMap::from([(
            EXAMPLE_BINDING_KEY.to_string(),
            serde_json::json!({
                "module_path": [module],
                "operation": operation
            }),
        )]),
    }
}

/// ADR 0107: a remote caller's raw key is global: the same bytes are one host
/// key whichever originator sends them, and exactly the key a host minting
/// those bytes gets. A record's `start_key_digest` echoed back as a caller's
/// key is refused rather than hashed again into a key that starts a second
/// process.
///
/// Red on the parent commit, where the conversion mixed the originator into
/// the key and session B's bytes derived another key.
#[test]
fn a_remote_start_key_is_global_and_never_rehashed() {
    let remote_start = |session: &'static str, start_key: &str| {
        let mut remote = RemoteProcessStartRequest::try_from(lash_core::ProcessStartRequest::new(
            lash_core::testing::held_engine_input(serde_json::json!({ "label": "Held" })),
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session)),
            lash_core::Lifetime::Detached,
        ))
        .expect("remote start");
        remote.start_key = Some(start_key.to_string());
        remote
    };
    let key_of = |remote: RemoteProcessStartRequest| {
        lash_core::ProcessStartRequest::try_from(remote)
            .expect("core start")
            .start_key()
            .cloned()
            .expect("a caller's key is kept")
    };
    let session_a = key_of(remote_start("session-a", "nightly-report"));
    assert_eq!(
        key_of(remote_start("session-a", "nightly-report")),
        session_a,
        "one caller's retry re-derives its key"
    );
    assert_eq!(
        key_of(remote_start("session-b", "nightly-report")),
        session_a,
        "the same raw key from another originator is the same key"
    );
    assert_eq!(
        session_a,
        lash_core::StartKey::for_host("nightly-report"),
        "a remote key is the host key of the caller's bytes, nothing mixed in"
    );
    assert!(session_a.is_host_supplied());

    let echoed = remote_start("session-a", session_a.as_str());
    let error = lash_core::ProcessStartRequest::try_from(echoed)
        .expect_err("an echoed start-key digest is refused");
    assert!(
        error.to_string().contains("start_key_digest"),
        "the refusal names the digest field: {error}"
    );
}

fn assert_process_start_roundtrip(request: lash_core::ProcessStartRequest) {
    let before = serde_json::to_value(&request).expect("request json");
    let remote = RemoteProcessStartRequest::try_from(request).expect("remote start");
    remote.validate().expect("valid remote start");
    let core = lash_core::ProcessStartRequest::try_from(remote).expect("core start");
    assert_eq!(serde_json::to_value(&core).expect("core json"), before);
}

#[test]
fn observed_work_item_decode_rejects_a_mispaired_event_tail() {
    let mut remote = RemoteProcessWorkItem::try_from(observed_work_item()).expect("remote item");
    remote.event_tail_sequence = remote.event_tail_sequence.saturating_add(1);

    let error = lash_core::facade_support::ObservedWorkItem::try_from(remote)
        .expect_err("a peer work-item event tail from another snapshot must be rejected");
    assert!(
        error.to_string().contains("event-tail sequence"),
        "unexpected error: {error}"
    );
}

#[test]
fn observed_work_item_round_trip_preserves_a_typed_event_tail_mismatch() {
    let mut observed = observed_work_item();
    // The fixture record carries last_event_sequence 0; one event at sequence
    // 1 makes the derived coherence a mismatch.
    observed
        .events
        .push(lash_core::facade_support::ObservedProcessEvent {
            sequence: 1,
            event_type: "process.completed".to_string(),
            occurred_at_ms: 12,
            payload: serde_json::json!({}),
        });

    let remote = RemoteProcessWorkItem::try_from(observed).expect("remote mismatch item");
    remote
        .validate("RemoteProcessWorkItem")
        .expect("truthful mismatch state");
    let core =
        lash_core::facade_support::ObservedWorkItem::try_from(remote).expect("core mismatch item");

    assert_eq!(
        core.state(),
        lash_core::facade_support::ObservedWorkItemState::EventTailMismatch {
            record_sequence: 0,
            event_tail_sequence: 1,
        }
    );
}

#[test]
fn remote_temperature_survives_the_conversion_with_its_spelling_intact() {
    // Every accepted JSON number crosses the boundary unchanged in both
    // directions — an integer is not re-spelled as a float on the way in, and
    // 2^53 is the largest integer that can be accepted at all.
    for spelling in ["1", "1.0", "0.7", "2", "9007199254740992"] {
        let payload = format!("{{\"temperature\":{spelling}}}");
        let remote: RemoteGenerationOptions =
            serde_json::from_str(&payload).expect("deserialize remote generation options");
        remote
            .validate("RemoteGenerationOptions")
            .expect("temperature passes validation");
        let core =
            core_llm::GenerationOptions::try_from(remote).expect("convert to core generation");
        let round_tripped = serde_json::to_string(&RemoteGenerationOptions::from(core))
            .expect("serialize remote generation options");
        assert_eq!(
            round_tripped, payload,
            "temperature {spelling} must survive the round trip byte for byte"
        );
    }
}

#[test]
fn remote_temperature_rejects_integers_binary64_cannot_hold() {
    // 2^53 + 1 has no exact binary64 form, so accepting it would hand the core
    // request a different number than the sender wrote. Both the validate-only
    // path and the converting path refuse it.
    let remote: RemoteGenerationOptions =
        serde_json::from_str("{\"temperature\":9007199254740993}").expect("deserialize");
    let validation_error = remote
        .validate("RemoteGenerationOptions")
        .expect_err("an inexact integer temperature must not validate");
    assert!(
        validation_error.to_string().contains("binary64"),
        "{validation_error}"
    );
    let conversion_error = core_llm::GenerationOptions::try_from(remote)
        .expect_err("an inexact integer temperature must not convert");
    assert!(
        conversion_error
            .to_string()
            .contains("generation.temperature"),
        "{conversion_error}"
    );
}

#[test]
fn remote_generation_options_reject_a_negative_temperature() {
    let remote = RemoteGenerationOptions {
        output_token_cap: None,
        temperature: Some(serde_json::Number::from_f64(-0.5).expect("finite")),
        seed: None,
        stop_sequences: Vec::new(),
        parallel_tool_calls: None,
    };
    let error = core_llm::GenerationOptions::try_from(remote)
        .expect_err("a negative temperature must not convert");
    assert!(
        error.to_string().contains("generation.temperature"),
        "{error}"
    );
}

struct UnencodableTestValue;

impl serde::Serialize for UnencodableTestValue {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("unencodable payload"))
    }
}

#[test]
fn encode_remote_json_surfaces_typed_invalid_envelope_error() {
    let err = encode_remote_json(UnencodableTestValue, "RemoteTurnEvent", "output")
        .expect_err("unencodable output must return an error instead of shipping null");
    match err {
        RemoteProtocolError::InvalidEnvelope { type_name, message } => {
            assert_eq!(type_name, "RemoteTurnEvent");
            assert!(
                message.starts_with("cannot encode output:"),
                "expected 'cannot encode output:' message prefix, got: {message}"
            );
        }
        other => panic!("expected InvalidEnvelope error, got: {other:?}"),
    }
}
