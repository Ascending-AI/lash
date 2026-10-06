use super::*;
use lash::SessionId;
use lash::TurnId;

#[test]
fn attachment_urls_percent_encode_the_id_path_segment() {
    let attachment = ChatAttachment::from_id("sha256:folder/image 1.png");
    assert_eq!(
        attachment.retrieve_url,
        "/api/attachments/sha256%3Afolder%2Fimage%201%2Epng"
    );
}

#[test]
fn session_event_registry_isolates_channels_and_recreates_after_removal() {
    let registry = SessionEventRegistry::new(4);
    let mut session_a = registry.subscribe(&SessionId::from("session-a"));
    let mut session_b = registry.subscribe(&SessionId::from("session-b"));

    registry.publish(
        &SessionId::from("session-a"),
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        session_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_b.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));

    registry.remove(&SessionId::from("session-a"));
    assert!(!registry.contains(&SessionId::from("session-a")));
    let mut replacement_a = registry.subscribe(&SessionId::from("session-a"));
    registry.publish(
        &SessionId::from("session-a"),
        StreamItem::Done {
            turn_id: None,
            outcome: TurnDoneOutcome::Completed,
        },
    );
    assert!(matches!(
        replacement_a.try_recv(),
        Ok(ProductEvent {
            item: StreamItem::Done { .. },
            ..
        })
    ));
    assert!(matches!(
        session_a.try_recv(),
        Err(broadcast::error::TryRecvError::Closed)
    ));
}

#[test]
fn settled_product_reconciliation_keeps_the_cursor_monotonic() {
    let registry = SessionEventRegistry::new(4);
    let session_id = SessionId::from("reconciled-session");
    let committed_id = format!("fixture-user:{}", TurnId::from("reconciled-turn"));
    registry.publish_identified(
        &session_id,
        "provisional-message",
        StreamItem::Message {
            message: ChatMessage {
                id: committed_id.to_string(),
                role: "user".to_string(),
                text: "settled prompt".to_string(),
                at: String::new(),
                attachments: Vec::new(),
                provenance: Some(ChatMessageProvenance::TurnInput {
                    turn_id: TurnId::from("reconciled-turn"),
                }),
                client_nonce: None,
            },
        },
    );
    let expected_record = lash::remote::llm::RemoteLlmCallRecord {
        call_id: "call-1".to_string(),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![
            lash::remote::llm::RemoteAttemptRecord {
                ordinal: 1,
                outcome: lash::remote::llm::RemoteAttemptOutcome::Failed,
                protocol_position: lash::remote::llm::RemoteProtocolPosition::NoResponse,
                retry_budget_consumed: true,
                retry_decision: Some(lash::remote::llm::RemoteRetryDecision::Scheduled { delay_ms: 1, wait: lash::remote::llm::RemoteRetryWait::Backoff, class: lash::remote::llm::RemoteRetryClass::NoResponse }),
                error: Some(lash::remote::llm::RemoteNormalizedError {
                    class: lash::remote::llm::RemoteProviderFailureKind::Transport,
                    code: Some(lash::provider::FailureCode::provider("connection_reset")),
                    http_status: Some(503),
                    provider_request_id: Some("request-1".to_string()),
                    retry_after_ms: Some(25),
                }),
                evidence: Some(lash::remote::llm::RemoteExecutionEvidence {
                    collection_interruption: Some(
                        lash::remote::llm::RemoteExecutionEvidenceCollectionInterruption::ProtocolAbort,
                    ),
                    ..Default::default()
                }),
                generation_disposition: Some(lash::remote::llm::RemoteGenerationReceipt {
                    output_token_cap:
                        lash::remote::llm::RemoteGenerationOptionOutcome::ClampedToCapacity,
                    temperature: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
                    seed: lash::remote::llm::RemoteGenerationOptionOutcome::NotRequested,
                    stop_sequences:
                        lash::remote::llm::RemoteGenerationOptionOutcome::SuppressedProtocolOwned,
                    cache: lash::remote::llm::RemoteGenerationOptionOutcome::OmittedUnsupported,
                    ..Default::default()
                }),
                usage: Some(lash::remote::usage::RemoteUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_read_input_tokens: 3,
                    cache_write_input_tokens: 2,
                    reasoning_output_tokens: 5,
                }),
                usage_disposition: Default::default(),
            },
            lash::remote::llm::RemoteAttemptRecord {
                ordinal: 2,
                outcome: lash::remote::llm::RemoteAttemptOutcome::Completed,
                protocol_position: lash::remote::llm::RemoteProtocolPosition::TerminalObserved,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: None,
                generation_disposition: None,
                usage: None,
                usage_disposition: Default::default(),
            },
        ],
    };
    registry.publish_identified(
        &session_id,
        "model-call",
        StreamItem::ModelCallRecorded {
            record: expected_record.clone(),
        },
    );
    registry.publish_identified(
        &session_id,
        "turn-done",
        StreamItem::Done {
            turn_id: Some(TurnId::from("reconciled-turn")),
            outcome: TurnDoneOutcome::Completed,
        },
    );

    registry.reconcile_settled(
        &session_id,
        &BTreeSet::new(),
        &BTreeSet::from([TurnId::from("reconciled-turn")]),
        &BTreeSet::new(),
    );
    let reconciled = registry.snapshot(&session_id);
    assert_eq!(reconciled.cursor, 3);
    assert_eq!(reconciled.events.len(), 2);
    assert!(matches!(
        &reconciled.events[0].item,
        StreamItem::Message { message }
            if message.id == format!("fixture-user:{}", TurnId::from("reconciled-turn"))
    ));
    let StreamItem::ModelCallRecorded { record } = &reconciled.events[1].item else {
        panic!("reconciliation must retain the model-call record");
    };
    assert_eq!(record, &expected_record);
    assert!(
        !registry.publish_identified(
            &session_id,
            "turn-done",
            StreamItem::Done {
                turn_id: Some(TurnId::from("reconciled-turn")),
                outcome: TurnDoneOutcome::Completed,
            },
        ),
        "compaction must retain event identity for idempotent workflow replay"
    );
    assert_eq!(registry.snapshot(&session_id).cursor, 3);

    registry.publish_identified(
        &session_id,
        "host-only-event",
        StreamItem::Message {
            message: ChatMessage {
                id: "host-only".to_string(),
                role: "event".to_string(),
                text: "host event".to_string(),
                at: String::new(),
                attachments: Vec::new(),
                provenance: None,
                client_nonce: None,
            },
        },
    );
    assert_eq!(
        registry.snapshot(&session_id).events[2].sequence,
        4,
        "compaction must not reuse a cursor already observed by a client"
    );
}
