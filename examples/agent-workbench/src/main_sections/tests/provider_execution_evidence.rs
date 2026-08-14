async fn provider_execution_evidence_events(
) -> Vec<lash::remote::observations::RemoteSessionObservationEvent> {
    let mut evidence_events = Vec::new();
    for (provider_kind, model, call_id, response_id, served_model, finish) in [
        (
            lash_sim::runtime_providers::GOOGLE_OAUTH,
            "gemini-3.1-pro-preview",
            "google-call",
            "google-evidence-1",
            "gemini-3.1-pro-served",
            "STOP",
        ),
        (
            lash_sim::runtime_providers::ANTHROPIC,
            "claude-sonnet-4-20250514",
            "anthropic-call",
            "msg_anthropic_evidence_1",
            "claude-sonnet-4-20250514-served",
            "end_turn",
        ),
    ] {
        let script = lash_sim::runtime_providers::runtime_script_for_text(
            provider_kind,
            &format!("{provider_kind} evidence"),
        )
        .expect("provider evidence fixture");
        let transport = Arc::new(lash_sim::ScriptedLlmHttpTransport::new(script));
        let (mut provider, _, _) = lash_sim::runtime_providers::runtime_provider_components(
            provider_kind,
            &transport,
        )
        .expect("provider fixture components");
        let completion = provider
            .complete(lash_core::LlmRequest {
                model: model.to_string(),
                messages: vec![lash_core::llm::types::LlmMessage::text(
                    lash_core::llm::types::LlmRole::User,
                    "answer directly",
                )],
                attachments: Vec::new(),
                resolved_stored: Default::default(),
                tools: Arc::new(Vec::new()),
                tool_choice: lash_core::llm::types::LlmToolChoice::Auto,
                model_variant: Default::default(),
                model_capability: Default::default(),
                generation: Default::default(),
                scope: lash_core::LlmRequestScope::new(
                    "scorecard-session",
                    "scorecard-frame",
                    format!("scorecard-{provider_kind}"),
                ),
                output_spec: None,
                stream_events: Some(lash_core::llm::types::LlmEventSender::new(|_| {})),
                provider_trace: None,
            })
            .await
            .expect("provider fixture completion");
        let response_evidence = completion
            .response
            .execution_evidence
            .as_ref()
            .expect("fixture response has typed execution evidence");
        assert_eq!(
            response_evidence.provider_response_id.as_deref(),
            Some(response_id)
        );
        assert_eq!(response_evidence.served_model.as_deref(), Some(served_model));
        assert_eq!(response_evidence.provider_finish_reason.as_deref(), Some(finish));
        assert_eq!(response_evidence.reasoning_output_tokens, Some(0));

        let mut record = completion.call_record;
        record.call_id = lash_core::LlmCallId(call_id.to_string());
        let activity = lash::remote::usage::RemoteTurnActivity::from_core(
            evidence_events.len() as u64,
            lash_core::TurnActivity::independent(lash_core::TurnEvent::ModelCallRecorded {
                record,
            }),
        );
        let lash::remote::usage::RemoteTurnEvent::ModelCallRecorded {
            record: remote_record,
        } = &activity.event
        else {
            panic!("core model-call record must become typed remote activity");
        };
        assert_eq!(remote_record.call_id, call_id);
        let event = lash::remote::observations::RemoteSessionObservationEvent {
            protocol_version: lash::remote::REMOTE_PROTOCOL_VERSION,
            session_id: "scorecard-session".to_string(),
            replay_incarnation_id: "scorecard-incarnation".to_string(),
            turn_id: Some("scorecard-turn".to_string()),
            revision: evidence_events.len() as u64 + 1,
            cursor: format!("scorecard-cursor-{}", evidence_events.len() + 1),
            event: lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
                activity: Box::new(activity),
            },
        };
        event.validate().expect("provider evidence remote event");
        evidence_events.push(event);
    }
    evidence_events
}
