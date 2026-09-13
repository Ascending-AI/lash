use crate::*;

#[test]
fn queued_events_preserve_typed_payloads_and_refuse_old_peers() {
    let messages = vec![lash_core::PluginMessage::text(
        lash_core::MessageRole::Event,
        "ready",
    )];
    let cases = [
        (
            lash_core::TurnEvent::QueuedWorkStarted {
                boundary: lash_core::runtime::QueuedWorkClaimBoundary::Idle,
                batch_ids: vec!["batch".into()],
                causes: vec![lash_core::TurnCause {
                    id: "cause".into(),
                    event_type: "ready".into(),
                    origin: lash_core::MessageOrigin::Plugin {
                        plugin_id: "plugin".into(),
                        transient: false,
                    },
                    text: "ready".into(),
                }],
            },
            serde_json::json!({"type":"queued_work_started","boundary":"idle","batch_ids":["batch"],"causes":[{"id":"cause","event_type":"ready","origin":{"kind":"plugin","plugin_id":"plugin"},"text":"ready"}]}),
        ),
        (
            lash_core::TurnEvent::QueuedMessagesCommitted {
                messages,
                checkpoint: lash_core::CheckpointKind::AfterWork,
            },
            serde_json::json!({"type":"queued_messages_committed","messages":[{"role":"Event","content":"ready"}],"checkpoint":"after_work"}),
        ),
        (
            lash_core::TurnEvent::PluginRuntime {
                plugin_id: "plugin".into(),
                event: lash_core::PluginRuntimeEvent::Custom {
                    name: "foreign".into(),
                    payload: serde_json::json!([1, true]),
                },
            },
            serde_json::json!({"type":"plugin_runtime","plugin_id":"plugin","event":{"kind":"custom","name":"foreign","payload":[1,true]}}),
        ),
    ];
    for (core, expected) in cases {
        let event = RemoteTurnEvent::try_from(core).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<RemoteTurnEvent>(expected.clone()).unwrap(),
            event
        );
        let activity = RemoteTurnActivity {
            sequence: 1,
            id: "activity".into(),
            correlation_id: "correlation".into(),
            event,
        };
        let wire = activity.encode_json().unwrap();
        assert_eq!(RemoteTurnActivity::decode_json(&wire).unwrap(), activity);
        let mut old: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        old["protocol_version"] = serde_json::json!(52);
        assert!(matches!(
            RemoteTurnActivity::decode_json(&serde_json::to_vec(&old).unwrap()),
            Err(RemoteProtocolError::UnsupportedProtocolVersion {
                actual: 52,
                expected: 63
            })
        ));
    }
}

#[test]
fn queued_event_closed_vocabularies_have_independent_literal_pins() {
    for (value, literal) in [
        (RemoteQueuedWorkClaimBoundary::Idle, "idle"),
        (
            RemoteQueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            "active_turn_checkpoint",
        ),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), literal);
    }
    for (value, literal) in [
        (RemoteMessageRole::User, "User"),
        (RemoteMessageRole::Assistant, "Assistant"),
        (RemoteMessageRole::System, "System"),
        (RemoteMessageRole::Event, "Event"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), literal);
    }
    for (value, literal) in [
        (RemotePartKind::Text, "Text"),
        (RemotePartKind::Attachment, "Attachment"),
        (RemotePartKind::Code, "Code"),
        (RemotePartKind::Output, "Output"),
        (RemotePartKind::Error, "Error"),
        (RemotePartKind::Prose, "Prose"),
        (RemotePartKind::ToolCall, "ToolCall"),
        (RemotePartKind::ToolResult, "ToolResult"),
        (RemotePartKind::Reasoning, "Reasoning"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), literal);
    }
    let origins = [
        (
            RemoteMessageOrigin::Plugin {
                plugin_id: "p".into(),
                transient: true,
            },
            serde_json::json!({"kind":"plugin","plugin_id":"p","transient":true}),
        ),
        (
            RemoteMessageOrigin::Process {
                process_id: "p".into(),
                event_type: "e".into(),
                sequence: 1,
                wake_id: None,
                caused_by: None,
            },
            serde_json::json!({"kind":"process","process_id":"p","event_type":"e","sequence":1}),
        ),
        (
            RemoteMessageOrigin::TurnInput {
                turn_id: "t".into(),
                input_id: None,
            },
            serde_json::json!({"kind":"turn_input","turn_id":"t"}),
        ),
        (
            RemoteMessageOrigin::TurnOutput {
                turn_id: "t".into(),
                source: RemoteTurnOutputSource::Runtime,
            },
            serde_json::json!({"kind":"turn_output","turn_id":"t","source":{"kind":"runtime"}}),
        ),
        (
            RemoteMessageOrigin::TurnOutput {
                turn_id: "t".into(),
                source: RemoteTurnOutputSource::Plugin {
                    plugin_id: "p".into(),
                },
            },
            serde_json::json!({"kind":"turn_output","turn_id":"t","source":{"kind":"plugin","plugin_id":"p"}}),
        ),
    ];
    for (origin, literal) in origins {
        assert_eq!(serde_json::to_value(&origin).unwrap(), literal);
        assert_eq!(
            serde_json::from_value::<RemoteMessageOrigin>(literal).unwrap(),
            origin
        );
    }
    for (state, literal) in [
        (RemotePruneState::Intact, serde_json::json!("Intact")),
        (RemotePruneState::Cleared, serde_json::json!("Cleared")),
        (
            RemotePruneState::Deleted {
                breadcrumb: "b".into(),
                archive_hash: "a".into(),
            },
            serde_json::json!({"Deleted":{"breadcrumb":"b","archive_hash":"a"}}),
        ),
        (
            RemotePruneState::Summarized {
                summary: "s".into(),
                archive_hash: "a".into(),
            },
            serde_json::json!({"Summarized":{"summary":"s","archive_hash":"a"}}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&state).unwrap(), literal);
        assert_eq!(
            serde_json::from_value::<RemotePruneState>(literal).unwrap(),
            state
        );
    }
}

#[test]
fn queued_message_parts_keep_the_core_payload() {
    let part = lash_core::Part::text("part".into(), "content".into(), None);
    let expected = serde_json::to_value(&part).unwrap();
    let remote = RemotePart::from(part);
    assert_eq!(serde_json::to_value(remote).unwrap(), expected);
}
