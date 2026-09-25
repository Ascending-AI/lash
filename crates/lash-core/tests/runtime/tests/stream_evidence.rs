use super::*;

#[tokio::test]
async fn custom_provider_can_establish_a_no_summary_response_before_execution_evidence() {
    let backend = memory_backend().await;
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

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("observe a response without summary metadata"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("no-summary-response-establishment"),
            ),
        )
        .await
        .expect("completed response after establishment returns an assembled turn");

    assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
    let attempt = &turn.llm_calls[0].attempts[0];
    assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::TerminalObserved
    );
    assert!(turn.errors.iter().all(|error| {
        error.code != Some(lash_core::TurnFailureCode::StreamEvidenceBeforeResponseStart.into())
    }));
}

#[tokio::test]
async fn attempt_reset_clears_response_establishment_before_later_evidence() {
    let backend = memory_backend().await;
    let provider = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                response_started: true,
                ..Default::default()
            }),
            LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    provider_response_id: Some("discarded-attempt".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            LlmStreamEvent::AttemptReset,
            LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                execution_evidence: Some(lash_core::ExecutionEvidence {
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
    let mut runtime = standard_runtime_with_transport(&backend, provider).await;

    let turn = runtime
        .run_turn_assembled(
            TurnInput::text("reset response evidence"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("response-establishment-attempt-reset"),
            ),
        )
        .await
        .expect("protocol evidence failure returns an assembled turn");

    assert!(matches!(turn.outcome, TurnOutcome::Stopped(_)));
    assert!(turn.errors.iter().any(|error| {
        error.code == Some(lash_core::TurnFailureCode::StreamEvidenceBeforeResponseStart.into())
    }));
}

/// Collects the host-visible `TurnEvent`s for one streamed turn driven by a
/// scripted provider call.
async fn drive_streamed_turn(
    backend: &lash_core::Backend,
    call: MockCall,
) -> (Vec<TurnActivity>, Vec<SessionStreamEvent>) {
    let mut runtime = standard_runtime_with_transport(backend, mock_provider(vec![call])).await;
    let activities = RecordingTurnEvents::default();
    let events = RecordingSink::default();
    runtime
        .stream_turn(
            TurnInput::text("drive the scripted stream"),
            TurnOptions::new(
                CancellationToken::new(),
                backend_turn_scope(
                    backend,
                    &SessionId::from("root"),
                    &TurnId::from("stream-evidence-turn"),
                ),
            )
            .with_events(&events)
            .with_turn_events(&activities),
        )
        .await
        .expect("scripted stream completes the turn");
    (activities.snapshot(), events.snapshot())
}

fn prose_deltas(activities: &[TurnActivity]) -> Vec<String> {
    activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

fn completed_block_texts(activities: &[TurnActivity]) -> Vec<String> {
    activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlockCompleted {
                kind: lash_core::llm::types::StreamBlockKind::AssistantText,
                text,
                ..
            } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

fn text_block_call(stream_events: Vec<LlmStreamEvent>, final_text: &str) -> MockCall {
    MockCall {
        stream_events,
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: final_text.to_string(),
                response_meta: None,
            }],
            ..Default::default()
        }),
    }
}

fn block(id: &str) -> lash_core::llm::types::StreamBlockIdentity {
    lash_core::llm::types::StreamBlockIdentity::new(id, 0)
}

/// Regression for FIG-3371 review: a `TextBlockEnd` whose text does not
/// extend the streamed deltas is an authoritative correction — the block
/// must seal with the provider's text, not the stale accumulated deltas.
#[tokio::test]
async fn text_block_completion_seals_authoritative_correction() {
    let backend = memory_backend().await;
    let (activities, _) = Box::pin(drive_streamed_turn(
        &backend,
        text_block_call(
            vec![
                LlmStreamEvent::TextBlockStart {
                    block: block("message:m1"),
                },
                LlmStreamEvent::Delta {
                    block: block("message:m1"),
                    text: "draft".to_string(),
                },
                LlmStreamEvent::TextBlockEnd {
                    block: block("message:m1"),
                    text: "rewritten ending".to_string(),
                },
            ],
            "rewritten ending",
        ),
    ))
    .await;

    assert_eq!(prose_deltas(&activities), ["draft"]);
    assert_eq!(completed_block_texts(&activities), ["rewritten ending"]);
}

/// A `TextBlockEnd` that extends the streamed prefix forwards only the
/// unseen tail — replaying the whole authoritative text through a stateful
/// plugin transform would double-feed the already-seen prefix.
#[tokio::test]
async fn text_block_completion_forwards_only_the_unseen_tail() {
    let backend = memory_backend().await;
    let (activities, _) = Box::pin(drive_streamed_turn(
        &backend,
        text_block_call(
            vec![
                LlmStreamEvent::TextBlockStart {
                    block: block("message:m1"),
                },
                LlmStreamEvent::Delta {
                    block: block("message:m1"),
                    text: "Hello".to_string(),
                },
                LlmStreamEvent::TextBlockEnd {
                    block: block("message:m1"),
                    text: "Hello world".to_string(),
                },
            ],
            "Hello world",
        ),
    ))
    .await;

    assert_eq!(prose_deltas(&activities), ["Hello", " world"]);
    assert_eq!(completed_block_texts(&activities), ["Hello world"]);
}

/// A zero-delta block (started and immediately ended with the full text,
/// the OpenAI final-message reconciliation shape) publishes the whole
/// authoritative text as one delta before sealing.
#[tokio::test]
async fn text_block_completion_without_deltas_publishes_full_text() {
    let backend = memory_backend().await;
    let (activities, _) = Box::pin(drive_streamed_turn(
        &backend,
        text_block_call(
            vec![
                LlmStreamEvent::TextBlockStart {
                    block: block("message:m1"),
                },
                LlmStreamEvent::TextBlockEnd {
                    block: block("message:m1"),
                    text: "whole answer".to_string(),
                },
            ],
            "whole answer",
        ),
    ))
    .await;

    assert_eq!(prose_deltas(&activities), ["whole answer"]);
    assert_eq!(completed_block_texts(&activities), ["whole answer"]);
}

/// Regression for FIG-3371 review: reasoning that stayed in the durable
/// response but never streamed must not republish to the host when the
/// provider policy hides thinking.
#[tokio::test]
async fn unstreamed_reasoning_is_not_republished_while_thinking_is_hidden() {
    let backend = memory_backend().await;
    let (activities, events) = Box::pin(drive_streamed_turn(
        &backend,
        MockCall {
            stream_events: vec![],
            response: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Reasoning {
                        text: "private chain".to_string(),
                        replay: None,
                    },
                    LlmOutputPart::Text {
                        text: "public answer".to_string(),
                        response_meta: None,
                    },
                ],
                expose_thinking: Some(false),
                ..Default::default()
            }),
        },
    ))
    .await;

    assert!(
        activities.iter().all(|activity| !matches!(
            activity.event,
            TurnEvent::ReasoningDelta { .. }
                | TurnEvent::StreamBlockStarted {
                    kind: lash_core::llm::types::StreamBlockKind::Reasoning,
                    ..
                }
                | TurnEvent::StreamBlockCompleted {
                    kind: lash_core::llm::types::StreamBlockKind::Reasoning,
                    ..
                }
        )),
        "hidden reasoning must not reach the host: {activities:?}"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            SessionStreamEvent::ReasoningDelta { .. }
                | SessionStreamEvent::StreamBlockStarted {
                    kind: lash_core::llm::types::StreamBlockKind::Reasoning,
                    ..
                }
                | SessionStreamEvent::StreamBlockCompleted {
                    kind: lash_core::llm::types::StreamBlockKind::Reasoning,
                    ..
                }
        )),
        "hidden reasoning must not reach session events: {events:?}"
    );
    assert_eq!(prose_deltas(&activities), ["public answer"]);
}

/// The same reasoning-only payload republishes as a complete block when the
/// provider policy exposes thinking — the gate is `LlmResponse::expose_thinking`,
/// not the absence of reasoning parts.
#[tokio::test]
async fn unstreamed_reasoning_republishes_when_thinking_is_exposed() {
    let backend = memory_backend().await;
    let (activities, _) = Box::pin(drive_streamed_turn(
        &backend,
        MockCall {
            stream_events: vec![],
            response: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Reasoning {
                        text: "visible reasoning".to_string(),
                        replay: None,
                    },
                    LlmOutputPart::Text {
                        text: "public answer".to_string(),
                        response_meta: None,
                    },
                ],
                expose_thinking: Some(true),
                ..Default::default()
            }),
        },
    ))
    .await;

    let reasoning_deltas = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::ReasoningDelta { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasoning_deltas, ["visible reasoning"]);
    assert!(
        activities.iter().any(|activity| matches!(
            activity.event,
            TurnEvent::StreamBlockCompleted {
                kind: lash_core::llm::types::StreamBlockKind::Reasoning,
                ..
            }
        )),
        "republished reasoning must seal its block: {activities:?}"
    );
}

/// Regression for FIG-3371 review: the session lane (unstreamed buffered
/// completion) emits the same Started/Delta/Completed lifecycle and
/// per-part identities as the streaming lane — one bare `TextDelta` with a
/// shared `completed:{iter}:text` block fused every part into one
/// anonymous block.
#[tokio::test]
async fn unstreamed_response_publishes_block_lifecycle_per_text_part() {
    let backend = memory_backend().await;
    let (_, events) = Box::pin(drive_streamed_turn(
        &backend,
        MockCall {
            stream_events: vec![],
            response: Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "first part".to_string(),
                        response_meta: None,
                    },
                    LlmOutputPart::Text {
                        text: "second part".to_string(),
                        response_meta: None,
                    },
                ],
                ..Default::default()
            }),
        },
    ))
    .await;

    let started_ids = events
        .iter()
        .filter_map(|event| match event {
            SessionStreamEvent::StreamBlockStarted {
                kind: lash_core::llm::types::StreamBlockKind::AssistantText,
                block,
            } => Some(block.id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completed_ids = events
        .iter()
        .filter_map(|event| match event {
            SessionStreamEvent::StreamBlockCompleted {
                kind: lash_core::llm::types::StreamBlockKind::AssistantText,
                block,
                ..
            } => Some(block.id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(started_ids.len(), 2, "each text part opens its own block");
    assert_eq!(
        started_ids, completed_ids,
        "every opened block seals with the same identity"
    );
    assert_ne!(started_ids[0], started_ids[1], "parts never share a block");
    for event in &events {
        if let SessionStreamEvent::TextDelta { block, .. } = event {
            assert!(
                started_ids.contains(&block.id),
                "deltas ride a block opened by this lane: {block:?}"
            );
        }
    }
}
