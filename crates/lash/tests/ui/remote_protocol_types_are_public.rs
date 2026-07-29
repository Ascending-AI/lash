fn main() {
    let input = lash::remote::turn_input::RemoteTurnInput::text("hello");
    let request = lash::remote::turn_input::RemoteTurnRequest {
        protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        idempotency_key: Some("session:turn".to_string()),
        input,
        tool_grants: Vec::new(),
        metadata: std::collections::HashMap::new(),
    };

    request.validate().unwrap();

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
        protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
        occurrence_id: "occurrence:1".to_string(),
        deliveries: vec![lash::remote::triggers::RemoteTriggerDeliveryEmitReport {
            occurrence_id: "occurrence:1".to_string(),
            subscription_id: "subscription:1".to_string(),
            process_id: "process:1".to_string(),
            outcome: lash::remote::triggers::RemoteTriggerDeliveryEmitOutcome::Started,
        }],
    };
    report.validate().unwrap();

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
        turn_id: "turn".to_string(),
        committed_message_id: "message".to_string(),
        checkpoint: Some(
            lash::remote::observations::RemoteTurnInputCheckpoint::BeforeCompletion,
        ),
    };
    let observation = lash::remote::observations::RemoteSessionObservation {
        protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
        session_id: "session".to_string(),
        cursor: "lashsc1:0:0:session".to_string(),
        turn_index: 0,
        usage: lash::remote::usage::RemoteUsage::default(),
    };
    observation.validate().unwrap();
    let _remote_stream_item = lash::observe::RemoteSessionObservationStreamItem::Gap {
        observation,
        gap: lash::remote::observations::RemoteLiveReplayGap {
            protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
            session_id: "session".to_string(),
            requested_cursor: "lashsc1:0:0:session".to_string(),
            latest_cursor: "lashsc1:0:0:session".to_string(),
            latest_revision: 0,
            reason: lash::remote::observations::RemoteLiveReplayGapReason::Unavailable,
        },
    };
    let _process =
        lash::remote::observations::RemoteSessionObservationEventPayload::ProcessChanged {
            kind: lash::remote::observations::RemoteSessionProcessEventKind::Started,
            process_ids: vec!["process".to_string()],
        };

    let process_start = lash::remote::processes::RemoteProcessStartRequest {
        protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
        id: "process".to_string(),
        input: lash::remote::processes::RemoteProcessInput::External {
            metadata: serde_json::json!({}),
        },
        disposition: lash::remote::processes::RemoteRecoveryDisposition::ExternallyOwned,
        max_attempts: None,
        env_spec: Some(lash::remote::processes::RemoteProcessExecutionEnvSpec {
            plugin_options: lash::remote::processes::RemoteProcessPluginOptions::default(),
            policy: lash::remote::processes::RemoteProcessExecutionPolicy {
                provider_id: "provider".to_string(),
                model: lash::remote::processes::RemoteProcessModelSpec {
                    id: "model".to_string(),
                    variant: Default::default(),
                    capability: Default::default(),
                    limits: lash::remote::processes::RemoteProcessModelLimits {
                        context_window_tokens: 10,
                        output_token_capacity: Some(1),
                    },
                },
                generation: lash::remote::llm::RemoteGenerationOptions {
                    output_token_cap: Some(256),
                    temperature: None,
                    seed: Some(7),
                },
                ..Default::default()
            },
        }),
        originator: lash::remote::processes::RemoteProcessOriginator::Host { scope: None },
        identity: None,
        wake_session_id: None,
        observers: Vec::new(),
        event_types: Vec::new(),
    };
    process_start.validate().unwrap();

    let disposition = lash::remote::llm::RemoteGenerationDisposition {
        output_token_cap: lash::remote::llm::RemoteGenerationOptionDisposition::Applied,
        temperature: lash::remote::llm::RemoteGenerationOptionDisposition::OmittedSamplingPinned,
        seed: lash::remote::llm::RemoteGenerationOptionDisposition::OmittedUnsupported,
    };
    assert_ne!(
        disposition.seed,
        lash::remote::llm::RemoteGenerationOptionDisposition::NotRequested
    );
}
