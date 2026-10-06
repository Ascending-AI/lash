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
    let mut failure = lash_core::ToolFailure::with_suggested_delay(
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
        wire["output"]["outcome"]["payload"]["suggested_delay_ms"],
        serde_json::json!(25)
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
    for field in ["class", "code", "message", "source"] {
        let mut incomplete = failure.clone();
        incomplete.as_object_mut().unwrap().remove(field);
        invalid_payloads.push(incomplete);
    }
    for (field, unknown) in [
        ("class", serde_json::json!("unknown_class")),
        ("source", serde_json::json!("unknown_source")),
        ("suggested_delay_ms", serde_json::json!(-1)),
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
            "source": "tool"
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
    let event = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(lash_core::LiveReplayStore::publish(
            &store,
            &SessionId::from("session"),
            lash_core::SessionRevision::new(1),
            vec![lash_core::LiveReplayEventDraft::new(
                Some(&TurnId::from("turn")),
                lash_core::SessionObservationEventPayload::TurnActivity(activity),
            )],
        ))
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
