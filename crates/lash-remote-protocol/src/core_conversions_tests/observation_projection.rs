use super::*;

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
