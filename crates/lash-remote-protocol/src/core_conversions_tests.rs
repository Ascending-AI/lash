use std::collections::BTreeMap;
use std::sync::Arc;

use super::*;

const EXAMPLE_BINDING_KEY: &str = "example.call_path";

#[test]
fn turn_cancel_core_conversions_round_trip_every_envelope() {
    let core_request = lash_core::TurnCancelRequest::new(
        lash_core::TurnAddress::new("session", "turn"),
        "cancel-request",
        Some("queue-superseder".to_string()),
    )
    .with_reason("newer input arrived");
    let remote_request = RemoteTurnCancelRequest::from(core_request.clone());
    remote_request
        .validate()
        .expect("valid remote cancel request");
    let round_trip =
        lash_core::TurnCancelRequest::try_from(remote_request).expect("core cancel request");
    assert_eq!(round_trip, core_request);

    let evidence = lash_core::TurnCancellationEvidence {
        request_id: "cancel-request".to_string(),
        origin: Some("workbench-user".to_string()),
        reason: Some("stop button".to_string()),
    };
    let remote_evidence = RemoteTurnCancellationEvidence::from(evidence.clone());
    assert_eq!(
        lash_core::TurnCancellationEvidence::from(remote_evidence),
        evidence
    );
    let evidence_without_origin = lash_core::TurnCancellationEvidence {
        request_id: "cancel-without-origin".to_string(),
        origin: None,
        reason: None,
    };
    let remote_evidence = RemoteTurnCancellationEvidence::from(evidence_without_origin.clone());
    assert_eq!(
        lash_core::TurnCancellationEvidence::from(remote_evidence),
        evidence_without_origin
    );

    for core_outcome in [
        lash_core::TurnCancelOutcome::Requested(evidence.clone()),
        lash_core::TurnCancelOutcome::AlreadyRequested(evidence.clone()),
        lash_core::TurnCancelOutcome::CompletionWonRace,
        lash_core::TurnCancelOutcome::UnknownOrRevoked,
    ] {
        let remote = RemoteTurnCancelOutcome::from(core_outcome.clone());
        let round_trip = lash_core::TurnCancelOutcome::from(remote);
        assert_eq!(round_trip, core_outcome);
    }

    for tier in [
        lash_core::DurabilityTier::Inline,
        lash_core::DurabilityTier::Durable,
    ] {
        let remote = RemoteTurnControlDurabilityTier::from(tier);
        assert_eq!(lash_core::DurabilityTier::from(remote), tier);
    }
}

#[test]
fn turn_input_round_trips_remote_safe_fields() {
    let mut prompt = lash_core::PromptLayer::new();
    prompt.add_contribution(lash_core::PromptContribution::guidance("Guide", "remote"));
    let mut input = lash_core::TurnInput::items([
        lash_core::InputItem::text("a"),
        lash_core::InputItem::text("b"),
        lash_core::InputItem::attachment(lash_core::AttachmentSource::inline(
            lash_core::MediaType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )),
    ])
    .with_protocol_turn_options(lash_core::ProtocolTurnOptions::from_payload(
        serde_json::json!({ "mode": "remote" }),
    ))
    .with_trace_turn_id("trace-1");
    input.turn_context.set_prompt_layer(prompt.clone());

    let remote = RemoteTurnInput::try_from(input).expect("remote conversion");
    assert_eq!(remote.items.len(), 3);
    assert!(matches!(
        &remote.items[2],
        RemoteInputItem::Attachment {
            source: RemoteAttachmentSource::Inline { data_base64, .. }
        } if data_base64 == "AQID"
    ));
    assert_eq!(remote.trace_turn_id.as_deref(), Some("trace-1"));
    assert_eq!(
        remote.protocol_turn_options.as_ref().unwrap().payload,
        serde_json::json!({ "mode": "remote" })
    );
    assert_eq!(remote.prompt_layer, Some(prompt.clone().into()));

    let core = lash_core::TurnInput::try_from(remote).expect("core conversion");
    assert!(matches!(
        &core.items[2],
        lash_core::InputItem::Attachment {
            source: lash_core::AttachmentSource::Inline { bytes, .. }
        } if bytes == &[1, 2, 3]
    ));
    assert_eq!(core.trace_turn_id.as_deref(), Some("trace-1"));
    assert_eq!(
        core.protocol_turn_options.unwrap().payload,
        serde_json::json!({ "mode": "remote" })
    );
    assert_eq!(core.turn_context.prompt_layer(), &prompt);
}

#[test]
fn turn_input_rejects_non_remote_safe_fields() {
    struct DummyTurnExtension;

    impl lash_core::ProtocolTurnExtension for DummyTurnExtension {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    let mut input = lash_core::TurnInput::text("extension");
    input.protocol_extension = Some(lash_core::ProtocolTurnExtensionHandle::new(
        DummyTurnExtension,
    ));
    assert!(matches!(
        RemoteTurnInput::try_from(input),
        Err(RemoteProtocolError::NonRemoteSafeTurnInput(message))
            if message.contains("protocol turn")
    ));

    let mut input = lash_core::TurnInput::text("live");
    input.turn_context.insert_plugin_input("demo", 1_u32);
    assert!(matches!(
        RemoteTurnInput::try_from(input),
        Err(RemoteProtocolError::NonRemoteSafeTurnInput(message))
            if message.contains("live plugin")
    ));

    let mut input = lash_core::TurnInput::text("provider");
    input.turn_context.set_provider(
        lash_core::testing::TestProvider::builder()
            .build()
            .into_handle(),
    );
    assert!(matches!(
        RemoteTurnInput::try_from(input),
        Err(RemoteProtocolError::NonRemoteSafeTurnInput(message))
            if message.contains("provider")
    ));
}

#[test]
fn provider_file_media_type_round_trips_between_core_and_remote() {
    for media_type in [
        None,
        Some(lash_core::MediaType::parse("image/png").unwrap()),
    ] {
        let core = lash_core::AttachmentSource::provider_file(
            lash_core::ProviderFileScope::new("anthropic", "credential"),
            "file-123",
            media_type,
        );

        let remote = RemoteAttachmentSource::from(core.clone());
        remote.validate(0).expect("valid remote attachment");
        let remote_json = serde_json::to_value(&remote).expect("serialize remote attachment");
        assert_eq!(
            remote_json
                .get("media_type")
                .and_then(|value| value.as_str()),
            core.media_type().map(lash_core::MediaType::as_str)
        );
        let round_trip =
            lash_core::AttachmentSource::try_from(remote).expect("core attachment conversion");
        assert_eq!(round_trip, core);
    }
}

#[test]
fn llm_request_and_response_round_trip_owned_dtos() {
    let request = core_llm::LlmRequest {
        model: "gpt-test".to_string(),
        messages: vec![core_llm::LlmMessage::text(core_llm::LlmRole::User, "hello")],
        attachments: vec![core_llm::AttachmentSource::inline(
            lash_core::MediaType::parse("image/png").unwrap(),
            vec![1, 2, 3],
        )],
        resolved_stored: Default::default(),
        tools: Arc::new(vec![core_llm::LlmToolSpec {
            name: "search".to_string(),
            description: "Search".to_string(),
            input_schema: lash_core::SchemaContract::new(serde_json::json!({
                "type": "object",
                "properties": { "raw": { "const": "x" } }
            }))
            .with_override(
                lash_core::SchemaDialect::OPENAI_TOOL_PARAMETERS,
                serde_json::json!({
                    "type": "object",
                    "properties": { "raw": { "type": "string", "enum": ["x"] } }
                }),
            ),
            output_schema: serde_json::Value::Null.into(),
        }]),
        tool_choice: core_llm::LlmToolChoice::Auto,
        model_variant: core_llm::ReasoningSelection::Effort("fast".to_string()),
        model_capability: core_llm::ModelCapability {
            reasoning: Some(core_llm::ReasoningCapability {
                efforts: vec!["fast".to_string(), "slow".to_string()],
                default_effort: Some("fast".to_string()),
                aliases: std::collections::BTreeMap::from([(
                    "quick".to_string(),
                    "fast".to_string(),
                )]),
                encoding: core_llm::ReasoningEncoding::Budget(std::collections::BTreeMap::from([
                    ("fast".to_string(), 1024u32),
                    ("slow".to_string(), 2048u32),
                ])),
                disable: Some(core_llm::ReasoningDisableEncoding::ToggleFalse),
                mandatory: false,
            }),
            cache_control: Some(core_llm::CacheControlDialect::Anthropic),
            stream_termination: Some(core_llm::StreamTermination::RequireTerminalEvidence),
        },
        generation: core_llm::GenerationOptions {
            output_token_cap: NonZeroUsize::new(42),
        },
        scope: core_llm::LlmRequestScope::new(
            "session-1",
            "session-1:frame:test",
            "session-1:request:test",
        ),
        output_spec: Some(core_llm::LlmOutputSpec::JsonObject),
        stream_events: None,
        provider_trace: None,
    };

    let remote = RemoteLlmRequest::from_core("request-1", request);
    let remote_json = serde_json::to_value(&remote).expect("serialize remote request");
    assert_eq!(
        remote_json["model_intent"]["variant"],
        serde_json::json!({ "effort": "fast" })
    );
    assert_eq!(
        remote_json["model_intent"]["capability"]["reasoning"]["disable"],
        serde_json::json!("toggle_false")
    );
    assert_eq!(
        remote_json["model_intent"]["capability"]["cache_control"],
        serde_json::json!("anthropic")
    );
    let remote: RemoteLlmRequest =
        serde_json::from_value(remote_json).expect("deserialize remote request");
    remote.validate().expect("valid remote request");
    assert_eq!(remote.protocol_version, REMOTE_PROTOCOL_VERSION);
    assert_eq!(remote.request_id, "request-1");
    assert_eq!(remote.scope.agent_frame_id, "session-1:frame:test");
    let core = core_llm::LlmRequest::try_from(remote).expect("core request");
    assert_eq!(core.model, "gpt-test");
    assert_eq!(
        core.model_variant,
        core_llm::ReasoningSelection::Effort("fast".to_string())
    );
    let reasoning = core
        .model_capability
        .reasoning
        .as_ref()
        .expect("capability must round-trip");
    assert_eq!(reasoning.efforts, vec!["fast", "slow"]);
    assert_eq!(reasoning.default_effort.as_deref(), Some("fast"));
    assert_eq!(
        core.model_capability.cache_control,
        Some(core_llm::CacheControlDialect::Anthropic)
    );
    assert_eq!(
        reasoning.aliases.get("quick").map(String::as_str),
        Some("fast")
    );
    assert_eq!(
        reasoning.encoding,
        core_llm::ReasoningEncoding::Budget(std::collections::BTreeMap::from([
            ("fast".to_string(), 1024u32),
            ("slow".to_string(), 2048u32)
        ]))
    );
    assert_eq!(core.session_id(), "session-1");
    assert_eq!(core.agent_frame_id(), "session-1:frame:test");
    assert_eq!(core.request_id(), "session-1:request:test");
    assert!(matches!(
        &core.attachments[0],
        core_llm::AttachmentSource::Inline { bytes, .. } if bytes == &[1, 2, 3]
    ));
    assert_eq!(
        core.tools[0].input_schema.projection.overrides[0].dialect,
        lash_core::SchemaDialect::OPENAI_TOOL_PARAMETERS
    );

    let response_metadata = BTreeMap::from([
        ("body:/cost".to_string(), serde_json::json!(0.000063)),
        (
            "header:x-opper-cost".to_string(),
            serde_json::json!("0.000008"),
        ),
    ]);
    let response = core_llm::LlmResponse {
        full_text: "done".to_string(),
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
        }),
        response_metadata: response_metadata.clone(),
    };
    let remote = RemoteLlmResponse::from_core("request-1", response);
    remote.validate().expect("valid remote response");
    assert_eq!(remote.provider_metadata.data, response_metadata);
    let core = core_llm::LlmResponse::from(remote);
    assert_eq!(core.full_text, "done");
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

#[test]
fn remote_model_intent_and_process_model_spec_round_trip_reasoning_selections() {
    for selection in [
        RemoteReasoningSelection::ProviderDefault,
        RemoteReasoningSelection::Disabled,
        RemoteReasoningSelection::Effort("high".to_string()),
    ] {
        let intent = RemoteModelIntent {
            model: "remote-model".to_string(),
            variant: selection.clone(),
            capability: RemoteModelCapability::default(),
            provider: None,
            metadata: HashMap::new(),
        };
        let intent_json = serde_json::to_value(&intent).expect("serialize model intent");
        let intent_round_trip: RemoteModelIntent =
            serde_json::from_value(intent_json).expect("deserialize model intent");
        assert_eq!(intent_round_trip.variant, selection);

        let spec = RemoteProcessModelSpec {
            id: "remote-model".to_string(),
            variant: selection.clone(),
            capability: RemoteModelCapability::default(),
            limits: RemoteProcessModelLimits::default(),
        };
        let spec_json = serde_json::to_value(&spec).expect("serialize process model spec");
        let spec_round_trip: RemoteProcessModelSpec =
            serde_json::from_value(spec_json).expect("deserialize process model spec");
        assert_eq!(spec_round_trip.variant, selection);
    }
}

#[test]
fn prompt_layer_round_trips_without_protocol_crate_depending_on_core_by_default() {
    let template = lash_core::PromptTemplate::new(vec![lash_core::PromptTemplateSection::titled(
        "Custom",
        vec![lash_core::PromptTemplateEntry::slot(
            lash_core::PromptSlot::Guidance,
        )],
    )]);
    let prompt = lash_core::PromptLayer::with_template(template)
        .with_contribution(lash_core::PromptContribution::guidance("Guide", "remote"));

    let remote = RemotePromptLayer::from(prompt.clone());
    let core = lash_core::PromptLayer::from(remote);
    assert_eq!(core, prompt);
}

#[test]
fn trigger_dtos_round_trip_core_values() {
    let request = lash_core::TriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "source-key",
        serde_json::json!({ "button": "Blue" }),
        "button-blue-1",
    )
    .with_source(serde_json::json!({ "id": "blue" }));
    let remote = RemoteTriggerOccurrenceRequest::from(request.clone());
    remote.validate().expect("valid remote request");
    let core = lash_core::TriggerOccurrenceRequest::try_from(remote).expect("core request");
    assert_eq!(core, request);

    let report = lash_core::TriggerEmitReport {
        occurrence_id: "occurrence:1".to_string(),
        deliveries: vec![lash_core::TriggerDeliveryEmitReport {
            occurrence_id: "occurrence:1".to_string(),
            subscription_id: "subscription:1".to_string(),
            process_id: "process:1".to_string(),
            outcome: lash_core::TriggerDeliveryEmitOutcome::Started,
        }],
    };
    let remote = RemoteTriggerEmitReport::from(report.clone());
    remote.validate().expect("valid remote report");
    let core = lash_core::TriggerEmitReport::try_from(remote).expect("core report");
    assert_eq!(core, report);

    let mut filter = lash_core::TriggerSubscriptionFilter::for_source_type("ui.button.pressed");
    filter.source_key = Some("source-key".to_string());
    filter.enabled = Some(true);
    let remote = RemoteTriggerSubscriptionFilter::from(filter.clone());
    remote.validate().expect("valid remote filter");
    let core = lash_core::TriggerSubscriptionFilter::try_from(remote).expect("core filter");
    assert_eq!(core, filter);

    let mut inputs = BTreeMap::new();
    inputs.insert("event".to_string(), lash_core::TriggerInputBinding::Event);
    let registration = lash_core::TriggerRegistration {
        subscription_key: "button-watcher".to_string(),
        incarnation: "incarnation-1".to_string(),
        revision: 7,
        registrant: lash_core::ProcessOriginator::host(),
        manifest_membership: lash_core::TriggerManifestMembership::PresentInCurrentArtifact,
        source_key: "source-key".to_string(),
        name: Some("button watcher".to_string()),
        source_type: lash_core::TriggerEventType::new("ui.button.pressed"),
        source: serde_json::json!({}),
        target: lash_core::TriggerTargetSummary {
            label: Some("on_button".to_string()),
            identity: engine_process_identity("on_button"),
            input: engine_process_input("on_button", serde_json::json!({})),
            inputs,
        },
        enabled: true,
    };
    let remote = RemoteTriggerRegistration::from(registration.clone());
    let core = lash_core::TriggerRegistration::try_from(remote).expect("core registration");
    assert_eq!(
        serde_json::to_value(&core).expect("core registration json"),
        serde_json::to_value(&registration).expect("registration json")
    );

    let cause = lash_core::CausalRef::TriggerOccurrence {
        occurrence_id: "occurrence:1".to_string(),
        subscription_id: Some("subscription:1".to_string()),
        subscription_incarnation: Some("incarnation:1".to_string()),
        subscription_revision: Some(4),
    };
    let remote = RemoteCausalRef::from(cause.clone());
    let core = lash_core::CausalRef::from(remote);
    assert_eq!(core, cause);
}

#[test]
fn trigger_subscription_dtos_round_trip_core_values() {
    let draft = trigger_subscription_draft();
    let remote = RemoteTriggerSubscriptionDraft::try_from(draft.clone()).expect("remote draft");
    remote.validate().expect("valid remote trigger draft");
    let core = lash_core::TriggerSubscriptionDraft::try_from(remote).expect("core draft");
    assert_eq!(
        serde_json::to_value(&core).expect("core draft json"),
        serde_json::to_value(&draft).expect("draft json")
    );

    let record = trigger_subscription_record();
    let remote = RemoteTriggerSubscriptionRecord::try_from(record.clone()).expect("remote record");
    remote
        .validate("RemoteTriggerSubscriptionRecord")
        .expect("valid remote trigger record");
    let core = lash_core::TriggerSubscriptionRecord::try_from(remote).expect("core record");
    assert_eq!(
        serde_json::to_value(&core).expect("core record json"),
        serde_json::to_value(&record).expect("record json")
    );

    let filter = lash_core::TriggerSubscriptionFilter {
        registrant_scope_id: Some("session:session-a".to_string()),
        session_id: None,
        subscription_key: Some("button-watcher".to_string()),
        name: Some("button watcher".to_string()),
        source_type: Some("ui.button.pressed".to_string()),
        source_key: Some("source-key".to_string()),
        target: Some(trigger_target_identity()),
        enabled: Some(true),
    };
    let remote = RemoteTriggerSubscriptionFilter::from(filter.clone());
    assert!(remote.target.is_some());
    let core = lash_core::TriggerSubscriptionFilter::try_from(remote).expect("core filter");
    assert_eq!(
        serde_json::to_value(&core).expect("core filter json"),
        serde_json::to_value(&filter).expect("filter json")
    );

    let request = RemoteTriggerRegisterSubscriptionRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        draft: RemoteTriggerSubscriptionDraft::try_from(draft).expect("remote request draft"),
    };
    let core = lash_core::TriggerSubscriptionDraft::try_from(request).expect("register request");
    assert_eq!(core.source_key, "source-key");

    let result =
        RemoteTriggerRegisterSubscriptionResult::try_from(record.clone()).expect("remote result");
    let core = lash_core::TriggerSubscriptionRecord::try_from(result).expect("register result");
    assert_eq!(core.subscription_id, record.subscription_id);

    let response =
        RemoteTriggerListSubscriptionsResponse::try_from(vec![record.clone()]).expect("response");
    let core_records =
        Vec::<lash_core::TriggerSubscriptionRecord>::try_from(response).expect("list response");
    assert_eq!(core_records.len(), 1);
    assert_eq!(
        serde_json::to_value(&core_records[0]).expect("core record json"),
        serde_json::to_value(&record).expect("record json")
    );
}

#[test]
fn process_start_requests_round_trip_core_values() {
    let external = lash_core::ProcessStartRequest::external(
        "process:external",
        lash_core::ProcessOriginator::host(),
        serde_json::json!({ "label": "External" }),
    )
    .with_wake_target(Some(lash_core::SessionScope::new("session-a")))
    .with_grant(Some(lash_core::ProcessStartGrant {
        session_scope: lash_core::SessionScope::new("session-a"),
        descriptor: lash_core::ProcessHandleDescriptor::new(Some("external"), Some("External")),
    }))
    .with_event_types([process_event_type()]);
    assert_process_start_roundtrip(external);

    let lashlang = lash_core::ProcessStartRequest::new(
        "process:lashlang",
        engine_process_input("main", serde_json::json!({ "event": true })),
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new("session-a")),
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::typed(
            "snapshot-tools",
            serde_json::json!({ "snapshot_ref": "tool-authority:sha256:abc" }),
        )
        .expect("plugin options"),
        lash_core::SessionPolicy {
            provider_id: "process-provider".to_string(),
            model: lash_core::ModelSpec::from_token_limits(
                "process-model",
                Default::default(),
                4096,
                Some(512),
            )
            .expect("model"),
            ..Default::default()
        },
    ))
    .with_event_types([process_event_type()]);
    assert_process_start_roundtrip(lashlang);

    let session_turn = lash_core::ProcessStartRequest::new(
        "process:session-turn",
        lash_core::ProcessInput::SessionTurn {
            create_request: Box::new(
                lash_core::SessionCreateRequest::child_session(
                    "session-a",
                    lash_core::SessionStartPoint::Empty,
                    Default::default(),
                )
                .with_session_id("child-session"),
            ),
            turn_input: Box::new(lash_core::TurnInput::text("hello child")),
            output_contract: lash_core::ToolOutputContract::from_input_schema(
                "schema",
                Some(serde_json::json!({ "type": "object" })),
            ),
        },
        lash_core::RecoveryDisposition::Rerunnable,
        lash_core::ProcessOriginator::host(),
    );
    assert_process_start_roundtrip(session_turn);
}

#[test]
fn process_records_events_snapshots_and_results_round_trip_core_values() {
    let mut record = process_record("process:record");
    record.status = lash_core::ProcessStatus::Completed {
        await_output: lash_core::ProcessAwaitOutput::Success {
            value: serde_json::json!({ "done": true }),
            control: None,
        },
    };
    let remote = RemoteProcessRecord::try_from(record.clone()).expect("remote record");
    remote
        .validate("RemoteProcessRecord")
        .expect("valid remote record");
    let core = lash_core::ProcessRecord::try_from(remote).expect("core record");
    assert_eq!(core.id, record.id);
    assert_eq!(core.status.label(), record.status.label());
    assert_eq!(
        serde_json::to_value(core.input.as_ref()).expect("core input json"),
        serde_json::to_value(record.input.as_ref()).expect("record input json")
    );

    let summary = lash_core::ProcessHandleSummary::new(
        "process:record",
        lash_core::ProcessHandleDescriptor::new(Some("external"), Some("External")),
        lash_core::ProcessLifecycleStatus::Completed,
    )
    .with_definition(Some(process_definition_identity("main")));
    let remote = RemoteProcessSummary::from(summary.clone());
    remote
        .validate("RemoteProcessSummary")
        .expect("valid summary");
    let core = lash_core::ProcessHandleSummary::try_from(remote).expect("core summary");
    assert_eq!(core.process_id, summary.process_id);
    assert_eq!(core.status, summary.status);

    let event = process_event("process:record");
    let remote = RemoteProcessEvent::from(event.clone());
    remote
        .validate("RemoteProcessEvent")
        .expect("valid process event");
    let core = lash_core::ProcessEvent::try_from(remote).expect("core event");
    assert_eq!(core.process_id, event.process_id);
    assert_eq!(core.event_type, event.event_type);
    assert_eq!(
        serde_json::to_value(&core.semantics).expect("core semantics json"),
        serde_json::to_value(&event.semantics).expect("event semantics json")
    );

    let observed = observed_process();
    let response =
        RemoteProcessListResponse::try_from(vec![observed.clone()]).expect("list response");
    response.validate().expect("valid list response");
    let core_observed =
        Vec::<lash_core::ObservedProcess>::try_from(response).expect("core observed");
    assert_eq!(core_observed[0].process_id, observed.process_id);

    let snapshot = lash_core::ProcessWorkSnapshot {
        session_id: "session-a".to_string(),
        visible_process_ids: vec!["process:observed".to_string()],
        items: vec![lash_core::ObservedWorkItem {
            process: observed,
            descriptor: lash_core::ProcessHandleDescriptor::new(Some("external"), Some("External")),
            events: vec![lash_core::ObservedProcessEvent {
                sequence: 1,
                event_type: "process.yield".to_string(),
                occurred_at_ms: 12,
                payload: serde_json::json!({ "text": "hi" }),
            }],
            kind: "external".to_string(),
            label: "External".to_string(),
        }],
    };
    let remote = RemoteProcessWorkSnapshot::try_from(snapshot.clone()).expect("remote snapshot");
    remote.validate().expect("valid snapshot");
    let core = lash_core::ProcessWorkSnapshot::try_from(remote).expect("core snapshot");
    assert_eq!(core.session_id, snapshot.session_id);
    assert_eq!(core.items[0].process.process_id, "process:observed");

    let start_result = RemoteProcessStartResult::try_from(process_record("process:start-result"))
        .expect("start result");
    let core = lash_core::ProcessRecord::try_from(start_result).expect("core start result");
    assert_eq!(core.id, "process:start-result");

    let cancel = RemoteProcessCancelResult::from(lash_core::ProcessCancelSummary {
        process_id: "process:cancel".to_string(),
        status: lash_core::ProcessLifecycleStatus::Cancelled,
    });
    let core = lash_core::ProcessCancelSummary::try_from(cancel).expect("core cancel summary");
    assert_eq!(core.status, lash_core::ProcessLifecycleStatus::Cancelled);

    let await_result = RemoteProcessAwaitResult::from((
        "process:await".to_string(),
        lash_core::ProcessAwaitOutput::Cancelled {
            message: "stopped".to_string(),
            raw: None,
            control: None,
        },
    ));
    let (process_id, output) =
        <(String, lash_core::ProcessAwaitOutput)>::try_from(await_result).expect("await result");
    assert_eq!(process_id, "process:await");
    assert!(matches!(
        output,
        lash_core::ProcessAwaitOutput::Cancelled { .. }
    ));

    let events_response =
        RemoteProcessEventsResponse::from(("process:record".to_string(), vec![event]));
    let (process_id, events) = <(String, Vec<lash_core::ProcessEvent>)>::try_from(events_response)
        .expect("events response");
    assert_eq!(process_id, "process:record");
    assert_eq!(events.len(), 1);
}

#[test]
fn process_list_cancel_signal_and_await_requests_convert_to_core_commands() {
    let filter = lash_core::ProcessListFilter {
        definition: Some(process_definition_identity("main")),
        status: lash_core::ProcessStatusFilter::Any,
        waiting: Some(true),
        originator_scope_id: Some("session:test".to_string()),
        identity_kind: Some("engine".to_string()),
        identity_label: Some("Main".to_string()),
        caused_by_occurrence_id: Some("occurrence-1".to_string()),
        caused_by_subscription_id: Some("subscription-1".to_string()),
        created_at_start_ms: Some(10),
        created_at_end_ms: Some(20),
    };
    let remote = RemoteProcessListFilter::from(filter.clone());
    remote.validate().expect("valid list filter");
    let core = lash_core::ProcessListFilter::try_from(remote).expect("core filter");
    assert_eq!(core.status, filter.status);
    assert_eq!(core.waiting, filter.waiting);
    assert_eq!(core.originator_scope_id, filter.originator_scope_id);
    assert_eq!(core.identity_kind, filter.identity_kind);
    assert_eq!(core.identity_label, filter.identity_label);
    assert_eq!(core.caused_by_occurrence_id, filter.caused_by_occurrence_id);
    assert_eq!(
        core.caused_by_subscription_id,
        filter.caused_by_subscription_id
    );
    assert_eq!(core.created_at_start_ms, filter.created_at_start_ms);
    assert_eq!(core.created_at_end_ms, filter.created_at_end_ms);
    assert!(core.definition.is_some());

    let cancel = RemoteProcessCancelRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:cancel".to_string(),
        reason: Some("host requested".to_string()),
    };
    cancel.validate().expect("valid cancel");
    let command = lash_core::ProcessCommand::from(cancel);
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Cancel { process_id, .. } if process_id == "process:cancel"
    ));

    let signal = RemoteProcessSignalRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:signal".to_string(),
        signal_name: "ready".to_string(),
        signal_id: "signal:1".to_string(),
        payload: serde_json::json!({ "ok": true }),
        replay_key: Some("signal-replay".to_string()),
        wake_target_scope: Some(lash_core::SessionScope::new("session-a").into()),
    };
    let append =
        lash_core::ProcessEventAppendRequest::try_from(signal.clone()).expect("append request");
    assert_eq!(append.event_type, "signal.ready");
    let command = lash_core::ProcessCommand::try_from(signal).expect("signal command");
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Signal { process_id, signal_name, signal_id, .. }
            if process_id == "process:signal"
                && signal_name == "ready"
                && signal_id == "signal:1"
    ));

    let await_request = RemoteProcessAwaitRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        process_id: "process:await".to_string(),
    };
    await_request.validate().expect("valid await");
    let command = lash_core::ProcessCommand::from(await_request);
    assert!(matches!(
        command,
        lash_core::ProcessCommand::Await { process_id } if process_id == "process:await"
    ));
}

#[test]
fn remote_turn_result_maps_core_semantics() {
    let turn = lash_core::AssembledTurn {
        state: Default::default(),
        outcome: lash_core::TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage {
            text: "done".to_string(),
        }),
        cancellation: None,
        assistant_output: lash_core::AssistantOutput {
            safe_text: "done".to_string(),
            raw_text: "done".to_string(),
            state: lash_core::OutputState::Usable,
        },
        execution: lash_core::ExecutionSummary {
            had_tool_calls: true,
            had_code_execution: false,
            started_at_ms: 1_700_000_000_000,
            duration_ms: 42,
        },
        token_usage: lash_core::TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        children_usage: vec![lash_core::TokenLedgerEntry {
            source: "subagent".to_string(),
            model: "m".to_string(),
            usage: lash_core::TokenUsage {
                input_tokens: 3,
                output_tokens: 4,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
        }],
        llm_calls: Vec::new(),
        tool_calls: vec![lash_core::ToolCallRecord {
            call_id: Some("exec-call".to_string()),
            tool: "lookup".to_string(),
            args: serde_json::json!({ "key": "value" }),
            output: lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true })),
            duration_ms: 7,
        }],
        errors: Vec::new(),
    };

    let remote = RemoteTurnResult::from_core("session", "turn", turn, []);
    remote.validate().expect("valid turn result");
    assert_eq!(remote.status, RemoteTurnStatus::Completed);
    assert_eq!(remote.usage.total.input_tokens, 4);
    assert_eq!(remote.usage.total.output_tokens, 6);
    assert_eq!(remote.execution.started_at_ms, 1_700_000_000_000);
    assert_eq!(remote.execution.duration_ms, 42);
    assert_eq!(remote.tool_calls.len(), 1);
    assert_eq!(remote.tool_calls[0].call_id.as_deref(), Some("exec-call"));
    assert_eq!(remote.tool_calls[0].tool_name, "lookup");
    assert!(matches!(
        &remote.tool_calls[0].outcome,
        RemoteToolCallOutcome::Success(value) if value == &serde_json::json!({ "ok": true })
    ));
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
fn remote_activity_preserves_semantic_fields_and_collapses_runtime_diagnostics() {
    let output = lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true }));
    let activity = lash_core::TurnActivity::new(
        lash_core::TurnActivityId::new("corr"),
        lash_core::TurnEvent::ToolCallCompleted {
            call_id: Some("call".to_string()),
            name: "demo".to_string(),
            args: serde_json::json!({ "a": 1 }),
            output,
            duration_ms: 42,
            graph_key: None,
            parent_call_id: None,
        },
    );
    let remote = RemoteTurnActivity::from_core(9, activity);
    assert_eq!(remote.sequence, 9);
    match remote.event {
        RemoteTurnEvent::ToolCallCompleted {
            call_id,
            args,
            duration_ms,
            ..
        } => {
            assert_eq!(call_id.as_deref(), Some("call"));
            assert_eq!(args, serde_json::json!({ "a": 1 }));
            assert_eq!(duration_ms, 42);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn remote_activity_preserves_model_attempt_reset_targets() {
    let activity = lash_core::TurnActivity::independent(lash_core::TurnEvent::ModelAttemptReset {
        assistant_prose_correlation_ids: vec![lash_core::TurnActivityId::new("prose")],
        reasoning_correlation_ids: vec![lash_core::TurnActivityId::new("reasoning")],
    });

    let remote = RemoteTurnActivity::from_core(4, activity);
    assert_eq!(
        remote.event,
        RemoteTurnEvent::ModelAttemptReset {
            assistant_prose_correlation_ids: vec!["prose".to_string()],
            reasoning_correlation_ids: vec!["reasoning".to_string()],
        }
    );
}

#[test]
fn remote_activity_exposes_typed_turn_input_application_without_display_text() {
    let application = lash_core::TurnInputApplication {
        input_id: "input-1".to_string(),
        source_key: Some("host:source-1".to_string()),
        turn_id: "turn-1".to_string(),
        committed_message_id: "message-1".to_string(),
        checkpoint: Some(lash_core::CheckpointKind::BeforeCompletion),
    };
    let activity =
        lash_core::TurnActivity::independent(lash_core::TurnEvent::QueuedInputAccepted {
            applications: vec![application],
        });

    let remote = RemoteTurnActivity::from_core(5, activity);
    remote.validate().expect("typed application validates");
    assert_eq!(
        remote.event,
        RemoteTurnEvent::TurnInputApplied {
            applications: vec![RemoteTurnInputApplication {
                input_id: "input-1".to_string(),
                source_key: Some("host:source-1".to_string()),
                turn_id: "turn-1".to_string(),
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
        "application evidence must not use RuntimeDiagnostic: {json}"
    );
    assert!(
        !json.to_string().contains("display") && !json.to_string().contains("text"),
        "application evidence must carry identity only: {json}"
    );
}

#[test]
fn remote_session_observation_from_core_maps_snapshot_metadata() {
    let store = lash_core::InMemoryLiveReplayStore::default();
    let event = lash_core::LiveReplayStore::append(
        &store,
        "session",
        lash_core::SessionRevision::new(4),
        lash_core::SessionObservationEventPayload::QueueChanged {
            kind: lash_core::SessionQueueEventKind::Enqueued,
            batch_ids: vec!["batch-1".to_string()],
        },
    )
    .expect("append observation event");
    let snapshot = lash_core::SessionSnapshot {
        session_id: "session".to_string(),
        turn_index: 12,
        token_usage: lash_core::TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        ..lash_core::SessionSnapshot::default()
    };
    let observation = lash_core::SessionObservation {
        read_view: lash_core::SessionReadView::from_snapshot(&snapshot),
        cursor: event.cursor.clone(),
    };

    let remote = RemoteSessionObservation::from_core(observation);
    remote.validate().expect("valid remote observation");
    assert_eq!(remote.session_id, "session");
    assert_eq!(remote.cursor, event.cursor.to_string());
    assert_eq!(remote.turn_index, 12);
    assert_eq!(remote.usage.input_tokens, 10);

    let remote_cursor = RemoteSessionCursor::from(&event.cursor);
    let core_cursor =
        lash_core::SessionCursor::try_from(remote_cursor.clone()).expect("core cursor");
    assert_eq!(core_cursor.to_string(), remote_cursor.cursor);
}

#[test]
fn remote_session_observation_from_core_maps_all_payload_variants() {
    fn event(
        payload: lash_core::SessionObservationEventPayload,
    ) -> Arc<lash_core::SessionObservationEvent> {
        let store = lash_core::InMemoryLiveReplayStore::default();
        lash_core::LiveReplayStore::append(
            &store,
            "session",
            lash_core::SessionRevision::new(4),
            payload,
        )
        .expect("append observation event")
    }

    let activity =
        lash_core::TurnActivity::independent(lash_core::TurnEvent::AssistantProseDelta {
            text: "delta".into(),
        });
    let remote = RemoteSessionObservationEvent::from_core(
        7,
        event(lash_core::SessionObservationEventPayload::TurnActivity(
            activity,
        )),
    );
    match remote.event {
        RemoteSessionObservationEventPayload::TurnActivity { activity } => {
            assert_eq!(activity.sequence, 7);
        }
        other => panic!("unexpected payload: {other:?}"),
    }

    let read_view =
        lash_core::SessionReadView::from_snapshot(&lash_core::SessionSnapshot::default());
    let remote = RemoteSessionObservationEvent::from_core(
        8,
        event(lash_core::SessionObservationEventPayload::Committed { read_view }),
    );
    assert!(matches!(
        remote.event,
        RemoteSessionObservationEventPayload::Committed
    ));

    let remote = RemoteSessionObservationEvent::from_core(
        9,
        event(
            lash_core::SessionObservationEventPayload::AgentFrameSwitched {
                frame_id: "frame-1".to_string(),
            },
        ),
    );
    assert!(matches!(
        remote.event,
        RemoteSessionObservationEventPayload::AgentFrameSwitched { frame_id }
            if frame_id == "frame-1"
    ));

    let remote = RemoteSessionObservationEvent::from_core(
        10,
        event(lash_core::SessionObservationEventPayload::QueueChanged {
            kind: lash_core::SessionQueueEventKind::Cancelled,
            batch_ids: vec!["batch-1".to_string()],
        }),
    );
    assert!(matches!(
        remote.event,
        RemoteSessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if kind == RemoteSessionQueueEventKind::Cancelled
                && batch_ids == vec!["batch-1".to_string()]
    ));

    let remote = RemoteSessionObservationEvent::from_core(
        11,
        event(lash_core::SessionObservationEventPayload::ProcessChanged {
            kind: lash_core::SessionProcessEventKind::Started,
            process_ids: vec!["process-1".to_string()],
        }),
    );
    assert!(matches!(
        remote.event,
        RemoteSessionObservationEventPayload::ProcessChanged { kind, process_ids }
            if kind == RemoteSessionProcessEventKind::Started
                && process_ids == vec!["process-1".to_string()]
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

fn process_definition_identity(process_name: &str) -> serde_json::Value {
    serde_json::json!({
        "module_ref": "lashlang:v1:sha256:module",
        "host_requirements_ref": "lashlang-host-requirements:v1:sha256:host",
        "process_ref": {
            "component": "process-component",
            "pos": 1
        },
        "process_name": process_name
    })
}

fn engine_process_input(process_name: &str, args: serde_json::Value) -> lash_core::ProcessInput {
    let _ = process_name;
    lash_core::ProcessInput::Engine {
        kind: "lashlang".to_string(),
        payload: serde_json::json!({
            "args": args
        }),
    }
}

fn engine_process_identity(process_name: &str) -> lash_core::ProcessIdentity {
    lash_core::ProcessIdentity::new("lashlang")
        .with_label(Some(process_name.to_string()))
        .with_definition(Some(process_definition_identity(process_name)))
}

fn trigger_target_identity() -> serde_json::Value {
    process_definition_identity("on_button")
}

fn trigger_input_template() -> BTreeMap<String, lash_core::TriggerInputBinding> {
    BTreeMap::from([
        ("event".to_string(), lash_core::TriggerInputBinding::Event),
        (
            "fixed".to_string(),
            lash_core::TriggerInputBinding::Fixed {
                value: serde_json::json!("blue"),
            },
        ),
    ])
}

fn trigger_subscription_draft() -> lash_core::TriggerSubscriptionDraft {
    lash_core::TriggerSubscriptionDraft {
        subscription_key: "button-watcher".to_string(),
        env_ref: lash_core::ProcessExecutionEnvRef::new(
            "process-env:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        wake_target: Some(lash_core::SessionScope::new("session-a")),
        name: Some("button watcher".to_string()),
        source_type: "ui.button.pressed".to_string(),
        source_key: "source-key".to_string(),
        source: serde_json::json!({ "button": "blue" }),
        payload_schema: lash_core::LashSchema::any(),
        target: engine_process_input("on_button", serde_json::json!({})),
        target_identity: engine_process_identity("on_button"),
        event_types: vec![process_event_type()],
        input_template: trigger_input_template(),
        target_label: Some("on_button".to_string()),
    }
}

fn trigger_subscription_record() -> lash_core::TriggerSubscriptionRecord {
    let draft = trigger_subscription_draft();
    lash_core::TriggerSubscriptionRecord {
        subscription_id: "trigger-subscription:v1:sha256:test".to_string(),
        owner_scope: lash_core::TriggerOwnerScope::session("session-a"),
        subscription_key: draft.subscription_key,
        incarnation: "incarnation-a".to_string(),
        revision: 1,
        definition_hash: "definition-hash-a".to_string(),
        registrant: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
            "session-a",
        )),
        env_ref: draft.env_ref,
        wake_target: draft.wake_target,
        name: draft.name,
        source_type: draft.source_type,
        source_key: draft.source_key,
        source: draft.source,
        payload_schema: draft.payload_schema,
        target: draft.target,
        target_identity: draft.target_identity,
        event_types: draft.event_types,
        input_template: draft.input_template,
        target_label: draft.target_label,
        enabled: true,
        tombstoned: false,
        deleted_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 2,
    }
}

fn assert_process_start_roundtrip(request: lash_core::ProcessStartRequest) {
    let before = serde_json::to_value(&request).expect("request json");
    let remote = RemoteProcessStartRequest::try_from(request).expect("remote start");
    remote.validate().expect("valid remote start");
    let core = lash_core::ProcessStartRequest::try_from(remote).expect("core start");
    assert_eq!(serde_json::to_value(&core).expect("core json"), before);
}

fn process_event_type() -> lash_core::ProcessEventType {
    lash_core::ProcessEventType {
        name: "process.completed".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec {
            terminal: Some(lash_core::ProcessTerminalSpec {
                state: lash_core::ProcessTerminalState::Completed,
                await_output: Some(lash_core::ProcessValueSelector::Pointer(
                    "/await_output".to_string(),
                )),
            }),
            wake: Some(lash_core::ProcessWakeSpec {
                when: None,
                input: lash_core::ProcessValueSelector::Pointer("/text".to_string()),
                dedupe_key: lash_core::ProcessWakeDedupeKey::Const("done".to_string()),
            }),
        },
    }
}

fn process_record(process_id: &str) -> lash_core::ProcessRecord {
    let registration = lash_core::ProcessRegistration::new(
        process_id,
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        lash_core::RecoveryDisposition::ExternallyOwned,
        lash_core::ProcessProvenance::host().with_caused_by(Some(
            lash_core::CausalRef::TriggerOccurrence {
                occurrence_id: "trigger:1".to_string(),
                subscription_id: None,
                subscription_incarnation: None,
                subscription_revision: None,
            },
        )),
    )
    .with_event_types([process_event_type()])
    .with_wake_target(Some(lash_core::SessionScope::new("session-a")));
    let mut record = lash_core::ProcessRecord::from_registration(registration);
    record.external_ref = Some(lash_core::ProcessExternalRef {
        backend: "worker".to_string(),
        id: "external:1".to_string(),
        metadata: Some(serde_json::json!({ "queue": "default" })),
    });
    record.wait = Some(lash_core::WaitState {
        kind: lash_core::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("process:{process_id}:signal.ready:1"),
            ordinal: 1,
        },
        since_ms: 10,
    });
    record
}

fn process_event(process_id: &str) -> lash_core::ProcessEvent {
    lash_core::ProcessEvent {
        process_id: process_id.to_string(),
        sequence: 1,
        event_type: "process.completed".to_string(),
        payload: serde_json::json!({ "await_output": { "type": "success", "value": true } }),
        invocation: lash_core::RuntimeInvocation::effect(
            lash_core::runtime::RuntimeScope::for_turn("session-a", "turn-a", 1, 0),
            "effect:1",
            lash_core::RuntimeEffectKind::Process,
            "replay:1",
        )
        .with_caused_by(Some(lash_core::CausalRef::Process {
            process_id: process_id.to_string(),
        })),
        semantics: lash_core::runtime::ProcessEventSemantics {
            terminal: Some(lash_core::ProcessTerminalSemantics {
                state: lash_core::ProcessTerminalState::Completed,
                await_output: lash_core::ProcessAwaitOutput::Success {
                    value: serde_json::json!(true),
                    control: None,
                },
            }),
            wake: Some(lash_core::ProcessWake {
                input: "wake".to_string(),
                dedupe_key: "dedupe".to_string(),
            }),
        },
        occurred_at: lash_core::system_time_from_epoch_ms(12),
    }
}

fn observed_process() -> lash_core::ObservedProcess {
    lash_core::ObservedProcess {
        process_id: "process:observed".to_string(),
        graph_key: "process:process:observed".to_string(),
        kind: "external".to_string(),
        identity: lash_core::ProcessIdentity::new("external")
            .with_label(Some("External".to_string())),
        lifecycle: lash_core::ProcessLifecycleStatus::Running,
        status_label: "running".to_string(),
        terminal: false,
        disposition: lash_core::RecoveryDisposition::ExternallyOwned,
        error: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        first_started: None,
        lease_holder: None,
        lease_expires_at_ms: None,
        abandon_request: None,
        input: lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "label": "External" }),
        },
        originator: lash_core::ProcessOriginator::host(),
        env_ref: None,
        wake_target: Some(lash_core::SessionScope::new("session-a")),
        caused_by: None,
        external_ref: None,
        wait: None,
        child_session_id: None,
        label: "External".to_string(),
    }
}
