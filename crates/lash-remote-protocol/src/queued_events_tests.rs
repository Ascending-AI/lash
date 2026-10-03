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
                boundary: lash_core::runtime::AdmissionBoundary::Idle,
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
            serde_json::json!({"type":"queued_messages_committed","messages":[{"role":"Event","parts":[{"id":"","kind":"Text","content":"ready"}]}],"checkpoint":"after_work"}),
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
        let wire = activity
            .encode_json(&crate::negotiation::test_negotiated())
            .unwrap();
        assert_eq!(RemoteTurnActivity::decode_json(&wire).unwrap(), activity);
        let mut old: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        old["protocol_version"] = serde_json::json!(52);
        assert!(matches!(
            RemoteTurnActivity::decode_json(&serde_json::to_vec(&old).unwrap()),
            Err(RemoteProtocolError::Unsupported { peer, local }) if peer == crate::VersionRange::exactly(52) && local == crate::REMOTE_PROTOCOL
        ));
    }
}

#[test]
fn legacy_queued_message_and_part_fields_are_rejected() {
    let current = serde_json::to_value(RemotePluginMessage::from(lash_core::PluginMessage::text(
        lash_core::MessageRole::User,
        "hello",
    )))
    .unwrap();
    for (field, value) in [
        ("content", serde_json::json!("ignored")),
        ("attachments", serde_json::json!([])),
    ] {
        let mut legacy = current.clone();
        legacy[field] = value;
        assert!(serde_json::from_value::<RemotePluginMessage>(legacy).is_err());
    }
    let mut legacy = current;
    legacy["parts"][0]["prune_state"] = serde_json::json!("Intact");
    assert!(serde_json::from_value::<RemotePluginMessage>(legacy).is_err());
}

/// The runtime's `MessageOrigin` is non-exhaustive, so a peer reads an origin
/// kind its protocol version does not model as `Unrecognized` rather than
/// refusing the message that carries it.
#[test]
fn an_origin_kind_this_version_does_not_model_reads_as_unrecognized() {
    let message: RemotePluginMessage = serde_json::from_value(serde_json::json!({
        "role": "Assistant",
        "origin": {"kind": "a_later_origin", "detail": 1},
        "parts": [],
    }))
    .expect("an unknown origin kind still decodes the message");
    assert_eq!(message.origin, Some(RemoteMessageOrigin::Unrecognized));
}
