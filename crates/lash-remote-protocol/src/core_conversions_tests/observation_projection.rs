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
        call_id: Some("call-frame-switch".to_string()),
        name: "continue_as".to_string(),
        args: serde_json::json!({}),
        output,
        duration_ms: 12,
        graph_key: None,
        parent_call_id: None,
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
    let wire = remote.encode_json().expect("encode remote observation");
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
