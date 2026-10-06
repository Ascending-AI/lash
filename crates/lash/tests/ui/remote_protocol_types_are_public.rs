use lash::TurnId;
use lash::{ProcessId, SessionId};

fn main() {
    let local = lash::remote::REMOTE_PROTOCOL;
    let accept = lash::remote::answer(
        local,
        &lash::remote::Negotiation::Hello { supported: local },
    );
    let negotiated = lash::remote::Negotiated::from_accept(local, &accept).unwrap();
    let input = lash::remote::turn_input::RemoteTurnInput::text("hello");
    let request = lash::remote::Envelope::at(
        &negotiated,
        lash::remote::turn_input::RemoteTurnRequest {
            session_id: SessionId::from("session"),
            turn_id: TurnId::from("turn"),
            input,
            protocol_turn_options: None,
            tool_grants: Vec::new(),
            metadata: std::collections::HashMap::new(),
        },
    );

    assert_eq!(
        request.protocol_version(),
        lash::remote::REMOTE_PROTOCOL_VERSION
    );
    request.body.validate().unwrap();

    let trigger = lash::remote::triggers::RemoteTriggerOccurrenceRequest::new(
        "ui.button.pressed",
        "source-key",
        serde_json::json!({ "button": "Blue" }),
        "button-blue-1",
    );
    trigger.validate().unwrap();

    let filter = lash::remote::triggers::RemoteTriggerSubscriptionFilter::for_source_type(
        "ui.button.pressed",
    );
    filter.validate().unwrap();

    let report = lash::remote::triggers::RemoteTriggerEmitReport {
        occurrence_id: "occurrence:1".to_string(),
        deliveries: vec![lash::remote::triggers::RemoteTriggerDeliveryEmitReceipt {
            occurrence_id: "occurrence:1".to_string(),
            subscription_id: "subscription:1".to_string(),
            outcome: lash::remote::triggers::RemoteTriggerDeliveryEmitOutcome::Started {
                process_id: ProcessId::parse("p_0192a3b4c5d670008000000000000001").unwrap(),
            },
        }],
    };
    report.validate().unwrap();
    let _failure = lash::remote::triggers::RemoteTriggerDeliveryEmitOutcome::Failed {
        code: lash::remote::triggers::RemoteTriggerDeliveryFailureCode::TriggerRouteRevoked,
        reason: "grant withdrawn".into(),
        value_mismatch: None,
    };

    let _cause = lash::remote::turn_result::RemoteCausalRef::TriggerOccurrence {
        occurrence_id: "occurrence:1".to_string(),
        subscription_id: None,
        subscription_incarnation: None,
        subscription_revision: None,
    };

    let _queue = lash::remote::observations::RemoteSessionObservationEventPayload::QueueChanged {
        kind: lash::remote::observations::RemoteSessionQueueEventKind::Enqueued,
        batch_ids: vec!["batch".to_string()],
    };
    let _application = lash::remote::observations::RemoteTurnInputApplication {
        input_id: "input".to_string(),
        source_key: Some("source".to_string()),
        turn_id: TurnId::from("turn"),
        committed_message_id: "message".to_string(),
        checkpoint: Some(lash::remote::observations::RemoteTurnInputCheckpoint::BeforeCompletion),
    };
    let observation = lash::remote::observations::RemoteSessionObservation {
        session_id: SessionId::from("session"),
        cursor: "lashsc2:replay-incarnation:0:0:session".to_string(),
        turn_index: 0,
        usage: lash::remote::usage::RemoteUsage::default(),
    };
    observation.validate().unwrap();
    let _remote_stream_item = lash::observe::RemoteSessionObservationStreamItem::Gap {
        observation,
        gap: lash::remote::observations::RemoteLiveReplayGap {
            session_id: SessionId::from("session"),
            requested_cursor: "lashsc2:replay-incarnation:0:0:session".to_string(),
            latest_cursor: "lashsc2:replay-incarnation:0:0:session".to_string(),
            latest_revision: 0,
            reason: lash::remote::observations::RemoteLiveReplayGapReason::Unavailable,
        },
    };
    let _process =
        lash::remote::observations::RemoteSessionObservationEventPayload::ProcessChanged {
            kind: lash::remote::observations::RemoteSessionProcessEventKind::Started {
                sequence: 1,
            },
            process_ids: vec![ProcessId::parse("p_0192a3b4c5d670008000000000000001").unwrap()],
        };

    let process_start = lash::remote::processes::RemoteProcessStartRequest {
        start_key: Some("start-key".to_string()),
        input: lash::remote::processes::RemoteProcessInput::Engine {
            kind: "job".to_string(),
            payload: serde_json::json!({}),
        }
        .into(),
        env_ref: Some(
            lash::remote::processes::RemoteProcessExecutionEnvRef::parse(format!(
                "process-env:v6:blake3:{}",
                "a".repeat(64)
            ))
            .expect("environment digest"),
        ),
        originator: lash::remote::processes::RemoteProcessOriginator::Host { scope: None },
        identity: None,
        wake_session_id: None,
        observers: Vec::new(),
        event_types: Vec::new(),
        lifetime: lash::remote::processes::RemoteStartLifetime::Detached,
        trace_cause: Default::default(),
    };
    process_start.validate().unwrap();

    let disposition = lash::remote::llm::RemoteGenerationReceipt {
        output_token_cap: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
        temperature: lash::remote::llm::RemoteGenerationOptionOutcome::ClampedToCapacity,
        seed: lash::remote::llm::RemoteGenerationOptionOutcome::OmittedUnsupported,
        stop_sequences: lash::remote::llm::RemoteGenerationOptionOutcome::NotRequested,
        cache: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
        reasoning: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
        reasoning_retention: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
        parallel_tool_calls: lash::remote::llm::RemoteGenerationOptionOutcome::NotRequested,
        thinking_summary: lash::remote::llm::RemoteGenerationOptionOutcome::NotRequested,
        thinking_visibility: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
        passthrough: lash::remote::llm::RemoteGenerationOptionOutcome::Applied,
    };
    assert_ne!(
        disposition.seed,
        lash::remote::llm::RemoteGenerationOptionOutcome::NotRequested
    );
}

fn inspect_send(outcome: lash::remote::turn_result::RemoteSendOutcome) {
    use lash::remote::turn_result::{RemoteParkedTurn, RemoteSendOutcome, RemoteStalledDelivery};
    match outcome {
        RemoteSendOutcome::OperationSettled {
            run, outcome, gaps, ..
        } => {
            let _ = (run, outcome, gaps);
        }
        RemoteSendOutcome::Settled { report, .. } => {
            let _ = report;
        }
        RemoteSendOutcome::Parked {
            parked: RemoteParkedTurn { run, .. },
            ..
        } => {
            let _ = run;
        }
        RemoteSendOutcome::Stalled {
            stalled: RemoteStalledDelivery { reason, .. },
            ..
        } => {
            let _ = reason;
        }
        RemoteSendOutcome::Refused { run, refusal, .. } => {
            let _ = (run, refusal);
        }
        RemoteSendOutcome::Withdrawn { gaps, .. } | RemoteSendOutcome::NotAccepted { gaps, .. } => {
            let _ = gaps;
        }
    }
}
