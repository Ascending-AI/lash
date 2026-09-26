//! The kernel door on the Restate server double (D1 F2, PR-S2): kernel tests
//! converted from their SQLite form onto `kernel_double`, running each turn
//! or drain on an `open_handler` controller while the test keeps its runtime
//! by `&mut`.

use super::*;

const SEED: u64 = 0x5_2d00;

/// `stream_evidence::custom_provider_can_establish_a_no_summary_response_before_execution_evidence`
/// on the double: the F2 rewrite, with a turn scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_no_summary_response_is_established_before_execution_evidence_on_the_double() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let provider = TestProvider::builder()
        .kind("no-summary-response")
        .requires_streaming(true)
        .complete(move |request| async move {
            let events = request.stream_events.expect("runtime stream event sender");
            events.send(LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                response_started: true,
                ..Default::default()
            }));
            events.send(LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    provider_response_id: Some("no-summary-response-id".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }));
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "response accepted".to_string(),
                    response_meta: None,
                }],
                terminal_reason: lash_core::LlmTerminalReason::Stop,
                ..Default::default()
            })
        })
        .build();
    let mut runtime = standard_runtime_with_transport(&backend, provider).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("no-summary-response-establishment"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("observe a response without summary metadata"),
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("completed response after establishment returns an assembled turn");
    handler.close().await.expect("close the turn's handler");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let attempt = &turn.llm_calls[0].attempts[0];
    assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::TerminalObserved
    );
}

/// A queued input drained on the double: `stream_next_queued_work` runs in an
/// open handler under a queue-drain scope, over an unbound store of the
/// double's store set.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_input_drains_in_an_open_handler_on_the_double() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "drained".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        }]),
        test_host_config(&backend),
        store.clone() as Arc<dyn lash_core::RuntimePersistence>,
    )
    .await;
    let session_id = SessionId::from(runtime.session_id().to_string());
    lash_core::store::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        lash_core::PendingTurnInputDraft::new(
            session_id.to_string(),
            lash_core::TurnInputIngress::NextTurn,
            TurnInput::text("drain me"),
        ),
    )
    .await
    .expect("enqueue an idle turn input");

    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            &session_id,
            "queued-drain-on-the-double",
        ))
        .await
        .expect("open the drain's handler");
    let drained = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), handler.scoped()))
        .await
        .expect("the queued input drains")
        .ran()
        .expect("the drain ran the queued input");
    handler.close().await.expect("close the drain's handler");
    assert_eq!(drained.assistant_output.safe_text, "drained");
}
