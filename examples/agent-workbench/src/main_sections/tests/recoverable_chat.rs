#[test]
fn session_event_registry_isolates_channels_and_recreates_after_removal() {
    let registry = SessionEventRegistry::new(4);
    let mut session_a = registry.subscribe("session-a");
    let mut session_b = registry.subscribe("session-b");

    registry.publish("session-a", StreamItem::Done);
    assert!(matches!(
        session_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done,
            ..
        })
    ));
    assert!(matches!(
        session_b.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    registry.remove("session-a");
    assert!(!registry.contains("session-a"));
    let mut replacement_a = registry.subscribe("session-a");
    registry.publish("session-a", StreamItem::Done);
    assert!(matches!(
        replacement_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done,
            ..
        })
    ));
    assert!(matches!(
        session_a.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
}

#[test]
fn product_event_lag_resynchronizes_from_durable_ordered_snapshot() {
    let registry = SessionEventRegistry::new(1);
    let mut subscriber = registry.subscribe("lag-session");
    for sequence in 1..=3 {
        registry.publish_identified(
            "lag-session",
            format!("event-{sequence}"),
            StreamItem::Message {
                message: ChatMessage {
                    id: format!("message-{sequence}"),
                    role: "event".to_string(),
                    text: format!("event {sequence}"),
                    at: String::new(),
                },
            },
        );
    }

    assert!(matches!(
        subscriber.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(2))
    ));
    let snapshot = registry.snapshot("lag-session");
    assert_eq!(snapshot.cursor, 3);
    assert_eq!(
        snapshot
            .events
            .iter()
            .map(|event| (event.sequence, event.event_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "event-1"), (2, "event-2"), (3, "event-3")]
    );
}

#[test]
fn product_event_identity_deduplicates_live_and_canonical_rows_across_reload() {
    let data_dir = tempfile::tempdir().expect("product event tempdir");
    let path = data_dir.path().join("product-events.json");
    let registry =
        SessionEventRegistry::persistent(path.clone(), 4).expect("persistent product events");
    let message = ChatMessage {
        id: "workbench-assistant:stable-turn".to_string(),
        role: "assistant".to_string(),
        text: "one canonical answer".to_string(),
        at: String::new(),
    };
    assert!(registry.publish_identified(
        "dedup-session",
        "message:workbench-assistant:stable-turn",
        StreamItem::Message {
            message: message.clone(),
        },
    ));
    assert!(!registry.publish_identified(
        "dedup-session",
        "message:workbench-assistant:stable-turn",
        StreamItem::Message { message },
    ));

    let reopened =
        SessionEventRegistry::persistent(path, 4).expect("reopen persistent product events");
    let snapshot = reopened.snapshot("dedup-session");
    assert_eq!(snapshot.events.len(), 1);
    assert!(matches!(
        &snapshot.events[0].item,
        StreamItem::Message { message }
            if message.id == "workbench-assistant:stable-turn"
                && message.text == "one canonical answer"
    ));
}

#[test]
fn product_projection_has_no_raw_error_variant_or_runtime_diagnostic_parser() {
    let safe = serde_json::to_string(&StreamItem::Message {
        message: ChatMessage {
            id: "turn:failed".to_string(),
            role: "event".to_string(),
            text: PUBLIC_TURN_FAILURE_MESSAGE.to_string(),
            at: String::new(),
        },
    })
    .expect("serialize safe failure row");
    assert!(safe.contains(PUBLIC_TURN_FAILURE_MESSAGE));
    assert!(!safe.contains("provider rejected credentials"));
    assert!(!ui::INDEX_HTML.contains("runtime_diagnostic"));
    assert!(!ui::INDEX_HTML.contains("renderError(item.message"));
    assert!(!ui::INDEX_HTML.contains("throw new Error(await response.text())"));
    assert!(!ui::INDEX_HTML.contains("error.message || String(error)"));
    assert!(!ui::INDEX_HTML.contains("result?.message || result?.error"));
}

#[test]
fn authorization_seam_can_deny_observation_without_product_specific_auth() {
    struct DenyObservation;

    impl WorkbenchAuthorizer for DenyObservation {
        fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
            match action {
                WorkbenchAuthorizationAction::Observe { .. } => {
                    Err(AppError::forbidden("observation denied by host policy"))
                }
                _ => Ok(()),
            }
        }
    }

    let authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyObservation));
    let denied = authorization
        .authorize(WorkbenchAuthorizationAction::Observe {
            session_id: "auth-session".to_string(),
        })
        .expect_err("host policy must be able to deny observation");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    authorization
        .authorize(WorkbenchAuthorizationAction::EnqueueTurn {
            session_id: "auth-session".to_string(),
        })
        .expect("independent enqueue policy remains pluggable");
}
