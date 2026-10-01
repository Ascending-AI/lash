use super::*;

fn completed_cell(error: Option<lash_core::CellFailure>) -> lash_core::TurnEvent {
    lash_core::TurnEvent::CodeBlockCompleted {
        language: "typescript".to_string(),
        output: "cell output".to_string(),

        error,
        duration_ms: 7,
        tool_call_ids: Vec::new(),
        graph_key: None,
    }
}

#[test]
fn fig4659_law_cell_failure_keeps_the_core_cause() {
    let failure = lash_core::CellFailure::new(lash_core::CellFailureKind::Program, "limit reached")
        .with_worker_limit(lash_sansio::worker_limit::WorkerLimit::Frame {
            kind: lash_sansio::worker_limit::WorkerFrameKind::Complete,
            size: 1025,
            bound: 1024,
        });
    let failure = failure.with_tool_call_limit(lash_sansio::ToolCallLimitExceeded {
        scope: lash_sansio::ToolCallLimitScope::Cell,
        limit: lash_sansio::MaxToolCalls::new(2),
        counted: 2,
        requested: 1,
    });
    let expected = serde_json::to_value(&failure).expect("encode core failure");
    let remote =
        RemoteTurnEvent::try_from(completed_cell(Some(failure))).expect("convert failed cell");
    let wire = serde_json::to_value(&remote).expect("encode failed cell");
    assert_eq!(wire["error"], expected);
    let decoded: RemoteTurnEvent = serde_json::from_value(wire).expect("decode failed cell");
    assert_eq!(decoded, remote);
}

#[test]
fn fig4659_law_cell_completion_records_success_once() {
    for error in [
        None,
        Some(lash_core::CellFailure::new(
            lash_core::CellFailureKind::Host,
            "offline",
        )),
    ] {
        let core = completed_cell(error);
        let core_wire = serde_json::to_value(&core).expect("encode core completion");
        assert!(
            core_wire.get("success").is_none(),
            "error determines success"
        );
        let remote = RemoteTurnEvent::try_from(core).expect("convert completion");
        let wire = serde_json::to_value(remote).expect("encode completion");
        assert!(
            wire.get("success").is_none(),
            "error determines wire success"
        );
    }
}

fn completed_tool(output: lash_core::ToolCallOutput) -> lash_core::TurnEvent {
    lash_core::TurnEvent::ToolCallCompleted {
        call_id: lash_core::ToolCallId::fixture("typed-output"),
        provider_call_id: Some("provider-call".to_string()),
        name: "lookup".to_string(),
        args: serde_json::json!({"id": 1}),
        output,
        duration_ms: 7,
        graph_key: None,
    }
}

#[test]
fn fig4659_law_tool_records_and_observations_share_the_typed_output() {
    let mut failure = lash_core::ToolFailure::safe_retry(
        lash_core::ToolFailureClass::Unavailable,
        "catalog_offline",
        "try again",
        Some(25),
    );
    failure.raw = Some(lash_core::ToolValue::untrusted_json(
        serde_json::json!({"upstream": 503}),
    ));
    let output = lash_core::ToolCallOutput::failure(failure)
        .with_view(lash_sansio::ToolView { blocks: Vec::new() })
        .with_projection_value(serde_json::json!({"display": "offline"}))
        .with_control(lash_core::ToolControl::Fail {
            failure: lash_core::ToolFailure::invalid_request("bad_request", "invalid input"),
        });
    let record = RemoteToolCallRecord::from(lash_core::ToolCallRecord {
        call_id: lash_core::ToolCallId::fixture("typed-output"),
        provider_call_id: Some("provider-call".to_string()),
        tool: "lookup".to_string(),
        args: serde_json::json!({"id": 1}),
        output: output.clone(),
    });
    let remote = RemoteTurnEvent::try_from(completed_tool(output)).expect("convert completion");
    let wire = serde_json::to_value(&remote).expect("encode completion");
    let record_wire = serde_json::to_value(record).expect("encode record");
    assert_eq!(record_wire["output"], wire["output"]);
    assert!(
        record_wire.get("outcome").is_none(),
        "output owns the outcome"
    );
    assert_eq!(wire["output"]["outcome"]["payload"]["class"], "unavailable");
    assert_eq!(
        wire["output"]["outcome"]["payload"]["code"],
        "catalog_offline"
    );
    assert_eq!(wire["output"]["outcome"]["payload"]["source"], "tool");
    assert_eq!(
        wire["output"]["outcome"]["payload"]["retry"],
        serde_json::json!({"type": "safe", "after_ms": 25})
    );
    assert_eq!(
        wire["output"]["outcome"]["payload"]["raw"],
        serde_json::json!({"upstream": 503})
    );
    let decoded: RemoteTurnEvent = serde_json::from_value(wire).expect("decode completion");
    assert_eq!(decoded, remote);
    for output in [
        lash_core::ToolCallOutput::success(
            serde_json::json!({"$lash_tool_value": "untrusted_json", "value": 42}),
        )
        .with_control(lash_core::ToolControl::Finish {
            value: lash_core::ToolValue::Null,
        }),
        lash_core::ToolCallOutput::cancelled(
            lash_core::ToolCancellation::runtime("stopped")
                .with_origin(lash_sansio::CancelOrigin::OperatorRequested),
        ),
    ] {
        let remote =
            RemoteTurnEvent::try_from(completed_tool(output.clone())).expect("convert outcome");
        let record = RemoteToolCallRecord::from(lash_core::ToolCallRecord {
            call_id: lash_core::ToolCallId::fixture("typed-output"),
            provider_call_id: None,
            tool: "lookup".to_string(),
            args: serde_json::json!({"id": 1}),
            output,
        });
        let wire = serde_json::to_value(&remote).expect("encode outcome");
        assert_eq!(
            serde_json::to_value(record).unwrap()["output"],
            wire["output"]
        );
        assert_eq!(
            serde_json::from_value::<RemoteTurnEvent>(wire).unwrap(),
            remote
        );
    }
}

#[test]
fn fig4659_law_tool_failure_shape_refuses_untyped_payloads() {
    let remote = RemoteTurnEvent::try_from(completed_tool(lash_core::ToolCallOutput::failure(
        lash_core::ToolFailure::io("offline", "connection lost"),
    )))
    .expect("convert failure");
    let schema =
        serde_json::to_value(schemars::schema_for!(RemoteTurnEvent)).expect("event schema");
    let validator = jsonschema::validator_for(&schema).expect("compile event schema");
    let wire = serde_json::to_value(remote).expect("encode failure");
    assert!(validator.is_valid(&wire));
    let failure = &wire["output"]["outcome"]["payload"];
    let mut invalid_payloads = Vec::new();
    for field in ["class", "code", "message", "source", "retry"] {
        let mut incomplete = failure.clone();
        incomplete.as_object_mut().unwrap().remove(field);
        invalid_payloads.push(incomplete);
    }
    for (field, unknown) in [
        ("class", serde_json::json!("unknown_class")),
        ("source", serde_json::json!("unknown_source")),
        ("retry", serde_json::json!({"type": "unknown_retry"})),
    ] {
        let mut unknown_variant = failure.clone();
        unknown_variant[field] = unknown;
        invalid_payloads.push(unknown_variant);
    }
    let mut extra = failure.clone();
    extra["unexpected"] = serde_json::json!(true);
    invalid_payloads.push(extra);
    invalid_payloads.extend([
        serde_json::Value::Null,
        serde_json::json!({"message": "offline"}),
        serde_json::json!({
            "class": "unknown_class", "code": "offline", "message": "offline",
            "source": "tool", "retry": {"type": "never"}
        }),
    ]);
    for payload in invalid_payloads {
        let mut invalid = wire.clone();
        invalid["output"]["outcome"]["payload"] = payload;
        assert!(
            serde_json::from_value::<RemoteTurnEvent>(invalid.clone()).is_err(),
            "untyped failure must not decode: {invalid}"
        );
        assert!(
            !validator.is_valid(&invalid),
            "schema must refuse untyped failure: {invalid}"
        );
    }
}

#[test]
fn tool_call_completed_observation_projects_frame_switch_without_seed_bodies() {
    const MESSAGE_SEED_BODY: &str = "message seed body must stay local";
    const PLUGIN_SEED_BODY: &str = "plugin seed body must stay local";
    let output = lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true }))
        .with_control(lash_core::ToolControl::SwitchAgentFrame {
            frame_key: lash_core::FrameKey::from_caller_material("remote-observation-test")
                .expect("non-empty caller material"),
            initial_nodes: vec![
                lash_core::SessionAppendNode::message(lash_core::PluginMessage::text(
                    lash_core::MessageRole::User,
                    MESSAGE_SEED_BODY,
                )),
                lash_core::SessionAppendNode::plugin(
                    "test.seed",
                    serde_json::json!({ "secret": PLUGIN_SEED_BODY }),
                ),
            ],
            task: Some("continue safely".to_string()),
        });
    let activity = lash_core::TurnActivity::independent(lash_core::TurnEvent::ToolCallCompleted {
        call_id: lash_core::ToolCallId::fixture("call-frame-switch"),
        provider_call_id: None,
        name: "continue_as".to_string(),
        args: serde_json::json!({}),
        output,
        duration_ms: 12,
        graph_key: None,
    });
    let store = lash_core::facade_support::InMemoryLiveReplayStore::default();
    let prepared = lash_core::LiveReplayStore::prepare_publication(
        &store,
        &SessionId::from("session"),
        lash_core::SessionRevision::new(1),
        vec![lash_core::LiveReplayEventDraft::new(
            Some(&TurnId::from("turn")),
            lash_core::SessionObservationEventPayload::TurnActivity(activity),
        )],
    )
    .expect("prepare observation");
    let event = lash_core::LiveReplayStore::publish_prepared(&store, prepared)
        .expect("publish observation")
        .remove(0);

    let remote =
        RemoteSessionObservationEvent::from_core(1, event).expect("project remote observation");
    let wire = remote
        .encode_json(&crate::negotiation::test_negotiated())
        .expect("encode remote observation");
    let wire_text = String::from_utf8(wire.clone()).expect("JSON is UTF-8");
    assert!(!wire_text.contains(MESSAGE_SEED_BODY));
    assert!(!wire_text.contains(PLUGIN_SEED_BODY));
    let encoded: serde_json::Value = serde_json::from_slice(&wire).expect("decode projected JSON");
    assert_eq!(
        encoded["activity"]["output"]["control"],
        serde_json::json!({
            "type": "switch_agent_frame",
            "frame_key": lash_core::FrameKey::from_caller_material("remote-observation-test")
                .expect("non-empty caller material")
                .as_str(),
            "task": "continue safely",
            "seed_count": 2,
        })
    );
}

#[test]
fn remote_observation_and_turn_input_exclude_reconnect_state() {
    let store = lash_core::facade_support::InMemoryLiveReplayStore::default();
    let prepared = lash_core::LiveReplayStore::prepare_publication(
        &store,
        &SessionId::from("session"),
        lash_core::SessionRevision::new(4),
        vec![lash_core::LiveReplayEventDraft::new(
            None::<String>,
            lash_core::SessionObservationEventPayload::QueueChanged {
                kind: lash_core::SessionQueueEventKind::Enqueued,
                batch_ids: vec!["batch-1".to_string()],
            },
        )],
    )
    .expect("prepare observation event");
    let event = lash_core::LiveReplayStore::publish_prepared(&store, prepared)
        .expect("publish observation event")
        .remove(0);
    let snapshot = lash_core::SessionSnapshot {
        session_id: SessionId::from("session"),
        turn_index: 12,
        token_usage: lash_core::TokenUsage {
            input_tokens: 10,
            output_tokens: 4,
            cache_read_input_tokens: 2,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 1,
        },
        ..lash_core::SessionSnapshot::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    let observation = lash_core::facade_support::SessionObservation {
        read_view: lash_core::SessionReadView::from_snapshot(&snapshot),
        cursor: event.cursor.clone(),
    };

    let remote = RemoteSessionObservation::from_core(observation);
    remote.validate().expect("valid remote observation");
    assert_eq!(remote.session_id, "session");
    assert_eq!(remote.cursor, event.cursor.to_string());
    assert_eq!(remote.turn_index, 12);
    assert_eq!(remote.usage.input_tokens, 10);

    let wire = serde_json::to_value(&remote).unwrap();
    let keys = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from(["session_id", "cursor", "turn_index", "usage"])
    );
    let request = crate::RemoteTurnRequest {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("turn"),
        input: crate::RemoteTurnInput::text("hello"),
        protocol_turn_options: None,
        tool_grants: Vec::new(),
        metadata: Default::default(),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({
            "session_id": "session", "turn_id": "turn", "input": {"items": [{"type": "text", "text": "hello"}]}
        })
    );
    let schema = serde_json::to_value(schemars::schema_for!(crate::RemoteTurnRequest)).unwrap();
    let properties = schema["properties"].as_object().unwrap();
    assert!(!properties.contains_key("activity_cursor"));
    assert!(!properties.contains_key("cursor"));
    assert!(!properties.contains_key("read_view"));

    let remote_cursor = RemoteSessionCursor::from(&event.cursor);
    let core_cursor =
        lash_core::SessionCursor::try_from(remote_cursor.clone()).expect("core cursor");
    assert_eq!(core_cursor.to_string(), remote_cursor.cursor);
}
