use super::*;

#[tokio::test]
async fn custom_provider_can_establish_a_no_summary_response_before_execution_evidence() {
    let response_established = Arc::new(tokio::sync::Notify::new());
    let provider_response_established = Arc::clone(&response_established);
    let provider = TestProvider::builder()
        .kind("no-summary-response")
        .requires_streaming(true)
        .complete(move |request| {
            let provider_response_established = Arc::clone(&provider_response_established);
            async move {
                let events = request.stream_events.expect("runtime stream event sender");
                events.send(LlmStreamEvent::Evidence(crate::LlmStreamEvidence {
                    response_started: true,
                    ..Default::default()
                }));
                events.send(LlmStreamEvent::Evidence(crate::LlmStreamEvidence {
                    execution_evidence: Some(crate::ExecutionEvidence {
                        provider_response_id: Some("no-summary-response-id".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }));
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                provider_response_established.notify_one();
                std::future::pending::<Result<LlmResponse, LlmTransportError>>().await
            }
        })
        .build();
    let mut runtime = standard_runtime_with_transport(provider).await;
    let cancellation = CancellationToken::new();
    let cancel_after_establishment = cancellation.clone();
    let canceller = crate::task::spawn(async move {
        response_established.notified().await;
        cancel_after_establishment.cancel();
    });

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("observe a response without summary metadata"),
            cancellation,
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("no-summary-response-establishment"),
            ),
        )
        .await
        .expect("cancellation after response establishment returns an assembled turn");
    canceller.await.expect("canceller task");

    assert!(matches!(
        turn.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    let attempt = &turn.llm_calls[0].attempts[0];
    assert_eq!(attempt.outcome, crate::AttemptOutcome::Aborted);
    assert_eq!(
        attempt.protocol_position,
        crate::ProtocolPosition::ResponseObserved
    );
    assert!(
        turn.errors.iter().all(|error| {
            error.code.as_deref() != Some("stream_evidence_before_response_start")
        })
    );
}

#[tokio::test]
async fn attempt_reset_clears_response_establishment_before_later_evidence() {
    let provider = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Evidence(crate::LlmStreamEvidence {
                response_started: true,
                ..Default::default()
            }),
            LlmStreamEvent::Evidence(crate::LlmStreamEvidence {
                execution_evidence: Some(crate::ExecutionEvidence {
                    provider_response_id: Some("discarded-attempt".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            LlmStreamEvent::AttemptReset,
            LlmStreamEvent::Evidence(crate::LlmStreamEvidence {
                execution_evidence: Some(crate::ExecutionEvidence {
                    provider_response_id: Some("too-early-after-reset".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "must not commit".to_string(),
                response_meta: None,
            }],
            ..Default::default()
        }),
    }]);
    let mut runtime = standard_runtime_with_transport(provider).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("reset response evidence"),
            CancellationToken::new(),
            named_turn_scope(
                &SessionId::from("root"),
                &TurnId::from("response-establishment-attempt-reset"),
            ),
        )
        .await
        .expect("protocol evidence failure returns an assembled turn");

    assert!(matches!(turn.outcome, TurnOutcome::Stopped(_)));
    assert!(
        turn.errors.iter().any(|error| {
            error.code.as_deref() == Some("stream_evidence_before_response_start")
        })
    );
}
