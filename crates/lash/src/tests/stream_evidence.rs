//! What a turn publishes from its model call's stream (FIG-3371): response
//! establishment before execution evidence, a text block's authoritative
//! completion, and the reasoning policy, on facade turns the session actor
//! runs.

use super::*;
use crate::support::TurnOutcome;
use lash_core::TurnEvent;
use lash_core::llm::types::{StreamBlockIdentity, StreamBlockKind};

/// One turn whose single model call streams `stream` and answers
/// `response`: its report and every activity it published.
async fn streamed_turn(
    session_id: &str,
    stream: Vec<LlmStreamEvent>,
    response: LlmResponse,
) -> Result<(crate::TurnReport, Vec<TurnActivity>)> {
    let script = Arc::new(StdMutex::new(Some((stream, response))));
    let provider = crate::testing::TestProvider::builder()
        .kind("stream-evidence")
        .requires_streaming(true)
        .complete(move |request| {
            let script = script.lock_recover().take();
            async move {
                let (stream, response) = script.expect("one model call");
                let events = request.stream_events.expect("runtime stream event sender");
                for event in stream {
                    events.send(event);
                }
                Ok(response)
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(session_id).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();
    let output = session
        .send(TurnInput::text("shift the scripted stream"))
        .output_into(&events)
        .await?;
    Ok((output, events.snapshot().await))
}

fn text_response_with(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        ..Default::default()
    }
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
                kind: StreamBlockKind::AssistantText,
                text,
                ..
            } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

fn block(id: &str) -> StreamBlockIdentity {
    StreamBlockIdentity::new(id, 0)
}

fn text(text: &str) -> LlmOutputPart {
    LlmOutputPart::Text {
        text: text.to_string(),
        response_meta: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_provider_can_establish_a_no_summary_response_before_execution_evidence()
-> Result<()> {
    let (report, _) = streamed_turn(
        "no-summary-response-establishment",
        vec![
            LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                response_started: true,
                ..Default::default()
            }),
            LlmStreamEvent::Evidence(lash_core::LlmStreamEvidence {
                execution_evidence: Some(lash_core::ExecutionEvidence {
                    provider_response_id: Some("no-summary-response-id".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ],
        LlmResponse {
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            ..text_response_with(vec![text("response accepted")])
        },
    )
    .await?;

    assert!(
        matches!(report.outcome, TurnOutcome::Finished(_)),
        "{:?}",
        report.outcome
    );
    let attempt = &report.llm_calls[0].attempts[0];
    assert_eq!(attempt.outcome, lash_core::AttemptOutcome::Completed);
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::TerminalObserved
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5334: a stopped turn's report carries no typed failure"]
async fn attempt_reset_clears_response_establishment_before_later_evidence() -> Result<()> {
    let (report, activities) = streamed_turn(
        "response-establishment-attempt-reset",
        vec![
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
        text_response_with(vec![text("must not commit")]),
    )
    .await?;

    assert!(
        matches!(report.outcome, TurnOutcome::Stopped(_)),
        "{:?}",
        report.outcome
    );
    assert!(
        report.errors.iter().any(|error| error.code
            == Some(lash_core::TurnFailureCode::StreamEvidenceBeforeResponseStart.into())),
        "the turn stops on evidence before its response started: {:?} {activities:?}",
        report.errors
    );
    Ok(())
}

/// A `TextBlockEnd` whose text does not extend the streamed deltas is an
/// authoritative correction: the block seals with the provider's text, not
/// the stale accumulated deltas.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_block_completion_seals_authoritative_correction() -> Result<()> {
    let (_, activities) = streamed_turn(
        "text-block-correction",
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
        text_response_with(vec![text("rewritten ending")]),
    )
    .await?;

    assert_eq!(prose_deltas(&activities), ["draft"]);
    assert_eq!(completed_block_texts(&activities), ["rewritten ending"]);
    Ok(())
}

/// A `TextBlockEnd` that extends the streamed prefix forwards only the
/// unseen tail: replaying the whole authoritative text through a stateful
/// plugin transform would double-feed the already-seen prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_block_completion_forwards_only_the_unseen_tail() -> Result<()> {
    let (_, activities) = streamed_turn(
        "text-block-tail",
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
        text_response_with(vec![text("Hello world")]),
    )
    .await?;

    assert_eq!(prose_deltas(&activities), ["Hello", " world"]);
    assert_eq!(completed_block_texts(&activities), ["Hello world"]);
    Ok(())
}

/// A zero-delta block (started and at once ended with the full text, the
/// OpenAI final-message reconciliation shape) publishes the whole
/// authoritative text as one delta before sealing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_block_completion_without_deltas_publishes_full_text() -> Result<()> {
    let (_, activities) = streamed_turn(
        "text-block-whole",
        vec![
            LlmStreamEvent::TextBlockStart {
                block: block("message:m1"),
            },
            LlmStreamEvent::TextBlockEnd {
                block: block("message:m1"),
                text: "whole answer".to_string(),
            },
        ],
        text_response_with(vec![text("whole answer")]),
    )
    .await?;

    assert_eq!(prose_deltas(&activities), ["whole answer"]);
    assert_eq!(completed_block_texts(&activities), ["whole answer"]);
    Ok(())
}

/// Reasoning that stayed in the response but never streamed is not
/// republished to the host when the provider policy hides thinking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstreamed_reasoning_is_not_republished_while_thinking_is_hidden() -> Result<()> {
    let (_, activities) = streamed_turn(
        "hidden-reasoning",
        Vec::new(),
        LlmResponse {
            expose_thinking: Some(false),
            ..text_response_with(vec![
                LlmOutputPart::Reasoning {
                    text: "private chain".to_string(),
                    replay: None,
                },
                text("public answer"),
            ])
        },
    )
    .await?;

    assert!(
        activities.iter().all(|activity| !matches!(
            activity.event,
            TurnEvent::ReasoningDelta { .. }
                | TurnEvent::StreamBlockStarted {
                    kind: StreamBlockKind::Reasoning,
                    ..
                }
                | TurnEvent::StreamBlockCompleted {
                    kind: StreamBlockKind::Reasoning,
                    ..
                }
        )),
        "hidden reasoning must not reach the host: {activities:?}"
    );
    assert_eq!(prose_deltas(&activities), ["public answer"]);
    Ok(())
}

/// The same reasoning republishes as a complete block when the provider
/// policy exposes thinking: the gate is `LlmResponse::expose_thinking`, not
/// the absence of reasoning parts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstreamed_reasoning_republishes_when_thinking_is_exposed() -> Result<()> {
    let (_, activities) = streamed_turn(
        "exposed-reasoning",
        Vec::new(),
        LlmResponse {
            expose_thinking: Some(true),
            ..text_response_with(vec![
                LlmOutputPart::Reasoning {
                    text: "visible reasoning".to_string(),
                    replay: None,
                },
                text("public answer"),
            ])
        },
    )
    .await?;

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
                kind: StreamBlockKind::Reasoning,
                ..
            }
        )),
        "republished reasoning must seal its block: {activities:?}"
    );
    Ok(())
}

/// An unstreamed response publishes the same Started/Delta/Completed
/// lifecycle and per-part identities as a streamed one: each text part
/// opens its own block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstreamed_response_publishes_block_lifecycle_per_text_part() -> Result<()> {
    let (_, activities) = streamed_turn(
        "unstreamed-block-lifecycle",
        Vec::new(),
        text_response_with(vec![text("first part"), text("second part")]),
    )
    .await?;

    let started_ids = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlockStarted {
                kind: StreamBlockKind::AssistantText,
                block,
            } => Some(block.id.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let completed_ids = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlockCompleted {
                kind: StreamBlockKind::AssistantText,
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
    for activity in &activities {
        if let TurnEvent::AssistantProseDelta { block, .. } = &activity.event {
            assert!(
                started_ids.contains(&block.id),
                "deltas ride a block opened by this lane: {block:?}"
            );
        }
    }
    Ok(())
}
