use super::*;

use futures_util::StreamExt as _;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use lash_facade_tests::observations::*;

#[path = "observation_recovery.rs"]
mod recovery;

/// FIG-5548: losing ingress continuity wakes every session observer, which
/// replaces its provisional evidence from a durable snapshot exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_wide_invalidation_resnapshots_every_session_feed() -> Result<()> {
    use lash_core::LiveReplayStore as _;
    use std::time::Duration;
    let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let mut feeds = Vec::new();
    for name in ["invalidation-first", "invalidation-second"] {
        let session = created_session(&core, SessionId::from(name))
            .await
            .open()
            .await?;
        let snapshot = session.observe().snapshot().await?;
        let revision = snapshot.cursor.parse().expect("snapshot cursor").revision;
        let mut feed = session.observe().subscribe_and_recover(snapshot.cursor);
        replay
            .publish(
                &session.session_id(),
                revision,
                vec![lash_core::LiveReplayEventDraft::new(
                    None::<TurnId>,
                    lash_core::SessionObservationEventPayload::ResidentChanged,
                )],
            )
            .await
            .expect("publish before invalidation");
        assert!(matches!(
            feed.next().await.expect("initial event")?,
            lash::observe::SessionObservationStreamItem::Event(_)
        ));
        feeds.push((session, revision, feed));
    }
    replay
        .invalidate_all()
        .await
        .expect("retire every replay window");
    for (session, revision, mut feed) in feeds {
        let gap = tokio::time::timeout(Duration::from_secs(10), feed.next())
            .await
            .expect("invalidation wakes the observer")
            .expect("gap item")?;
        let lash::observe::SessionObservationStreamItem::Gap { observation, gap } = gap else {
            panic!("lost continuity requires a replacement snapshot");
        };
        assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
        assert_eq!(gap.latest_revision, revision);
        assert_eq!(observation.cursor, gap.latest_cursor);
        assert_eq!(
            observation.read_view.session_id(),
            session.session_id().as_str()
        );
        replay
            .publish(
                &session.session_id(),
                revision,
                vec![lash_core::LiveReplayEventDraft::new(
                    None::<TurnId>,
                    lash_core::SessionObservationEventPayload::ResidentChanged,
                )],
            )
            .await
            .expect("publish after resnapshot");
        let next = tokio::time::timeout(Duration::from_secs(10), feed.next())
            .await
            .expect("the feed resumes")
            .expect("next event")?;
        assert!(
            matches!(next, lash::observe::SessionObservationStreamItem::Event(_)),
            "one loss produces one gap"
        );
    }
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_reasoning_part_does_not_republish_streamed_summary() -> Result<()> {
    let streamed_reasoning = LlmOutputPart::Reasoning {
        text: "**Planning single file search step**".to_string(),
        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-streamed".to_string()),
            encrypted_content: Some("opaque-streamed".to_string()),
            summary: vec!["**Planning single file search step**".to_string()],
            ..Default::default()
        }),
    };
    let completed_only_reasoning = LlmOutputPart::Reasoning {
        text: "**Completed-only summary**".to_string(),
        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
            item_id: Some("reasoning-completed-only".to_string()),
            encrypted_content: Some("opaque-completed-only".to_string()),
            summary: vec!["**Completed-only summary**".to_string()],
            ..Default::default()
        }),
    };
    let provider = lash::testing::TestProvider::builder()
        .kind("reasoning-delta-then-completed-part")
        .requires_streaming(true)
        .complete(move |request| {
            let streamed_reasoning = streamed_reasoning.clone();
            let completed_only_reasoning = completed_only_reasoning.clone();
            async move {
                let stream = request.stream_events.expect("stream events");
                let block = lash_core::llm::types::StreamBlockIdentity::new(
                    "reasoning-streamed:summary:0",
                    0,
                )
                .with_item_id(Some("reasoning-streamed".to_string()));
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::Reasoning,
                    block: block.clone(),
                    text: "**Planning single ".to_string(),
                }));
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::Reasoning,
                    block,
                    text: "file search step**".to_string(),
                }));
                stream.send(LlmStreamEvent::Part(streamed_reasoning.clone()));
                stream.send(LlmStreamEvent::Part(completed_only_reasoning.clone()));
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "done".to_string(),
                }));
                Ok(LlmResponse {
                    parts: vec![
                        streamed_reasoning,
                        completed_only_reasoning,
                        LlmOutputPart::Text {
                            text: "done".to_string(),
                            response_meta: None,
                        },
                    ],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("reasoning-single-publication").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let output = session
        .send(TurnInput::text("run one command"))
        .output()
        .await?;

    let reasoning = output
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                text,
                ..
            }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reasoning,
        vec![
            "**Planning single ",
            "file search step**",
            "**Completed-only summary**",
        ],
        "incremental chunks stay distinct, their completed snapshot is not republished, and a completed-only summary remains visible",
    );

    // The durable head the turn committed.
    let read_view = session.observe().snapshot().await?.read_view;
    let durable_reasoning = read_view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| matches!(part.kind(), lash_core::PartKind::Reasoning))
        .map(|part| part.content())
        .collect::<Vec<_>>();
    assert_eq!(
        durable_reasoning,
        vec![
            "**Planning single file search step**",
            "**Completed-only summary**",
        ],
        "completed reasoning parts remain authoritative durable response state",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_reasoning_then_tool_does_not_repeat_reasoning() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("reasoning-tool-publication")
        .requires_streaming(true)
        .complete({
            let calls = Arc::clone(&calls);
            move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    let stream = request.stream_events.expect("stream events");
                    match call {
                        0 => {
                            let reasoning = reasoning_output_part("inspect once", "reasoning-tool");
                            let tool = LlmOutputPart::ToolCall {
                                call_id: "lookup-once".to_string(),
                                tool_name: "tools__app_lookup".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            };
                            stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                                kind: StreamBlockKind::Reasoning,
                                block: lash_core::llm::types::StreamBlockIdentity::new(
                                    "reasoning-tool:summary:0",
                                    0,
                                )
                                .with_item_id(Some("reasoning-tool".to_string())),
                                text: "inspect once".to_string(),
                            }));
                            stream.send(LlmStreamEvent::Part(reasoning.clone()));
                            stream.send(LlmStreamEvent::Part(tool.clone()));
                            Ok(LlmResponse {
                                parts: vec![reasoning, tool],
                                response_metadata: Default::default(),
                                ..LlmResponse::default()
                            })
                        }
                        1 => {
                            stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                                kind: StreamBlockKind::AssistantText,
                                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                                text: "done".to_string(),
                            }));
                            Ok(text_response("done"))
                        }
                        _ => panic!("unexpected provider call {call}"),
                    }
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(AppTools))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("reasoning-tool-publication").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let output = session
        .send(TurnInput::text("inspect with a tool"))
        .output()
        .await?;

    assert_eq!(reasoning_activities(&output), vec!["inspect once"]);
    assert_eq!(output.assistant_message(), Some("done"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_streamed_reasoning_keeps_nonstreamed_text() -> Result<()> {
    let reasoning = reasoning_output_part("reasoning once", "reasoning-before-text");
    let provider = lash::testing::TestProvider::builder()
        .kind("reasoning-then-buffered-text")
        .requires_streaming(true)
        .complete(move |request| {
            let reasoning = reasoning.clone();
            async move {
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::Reasoning,
                    block: lash_core::llm::types::StreamBlockIdentity::new(
                        "reasoning-before-text:summary:0",
                        0,
                    )
                    .with_item_id(Some("reasoning-before-text".to_string())),
                    text: "reasoning once".to_string(),
                }));
                stream.send(LlmStreamEvent::Part(reasoning.clone()));
                Ok(LlmResponse {
                    parts: vec![
                        reasoning,
                        LlmOutputPart::Text {
                            text: "buffered answer".to_string(),
                            response_meta: None,
                        },
                    ],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("reasoning-buffered-text").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let output = session
        .send(TurnInput::text("answer after reasoning"))
        .output()
        .await?;

    assert_eq!(reasoning_activities(&output), vec!["reasoning once"]);
    assert_eq!(assistant_prose(&output.activities), "buffered answer");
    assert_eq!(output.assistant_message(), Some("buffered answer"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_publication_preserves_identical_completed_reasoning_parts_and_turns() -> Result<()>
{
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("identical-reasoning-publication")
        .requires_streaming(true)
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(LlmResponse {
                        parts: vec![
                            reasoning_output_part("repeat legitimately", &format!("{call}-a")),
                            reasoning_output_part("repeat legitimately", &format!("{call}-b")),
                        ],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("identical-reasoning-publication").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    for prompt in ["first turn", "second turn"] {
        let output = session.send(TurnInput::text(prompt)).output().await?;
        assert_eq!(
            reasoning_activities(&output),
            vec!["repeat legitimately", "repeat legitimately"]
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_provider_failure_after_prose_is_not_retried_or_committed() -> Result<()> {
    const MARKER: &str = "retry observer single-copy marker";
    let transport_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                output_then_failing_rlm_prose_provider(
                    Arc::clone(&transport_calls),
                    Arc::clone(&requests),
                ),
                mock_llm_profile_spec(),
            )
            .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("rlm-provider-retry-prose").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let first = session
        .send(TurnInput::text("trigger deterministic rate limit retry"))
        .output()
        .await?;

    assert_eq!(
        transport_calls.load(Ordering::SeqCst),
        1,
        "provider output must not be re-bought after the failed attempt"
    );
    assert!(matches!(
        first.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError { .. })
    ));
    assert!(first.activities.iter().any(|activity| matches!(
        &activity.event,
        TurnEvent::StreamBlock(StreamBlockEvent::Delta { kind: StreamBlockKind::AssistantText,text, .. }) if text.contains(MARKER)
    )));
    assert!(
        first
            .activities
            .iter()
            .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
    );
    let rlm_marker_records = session
        .durable()
        .read()
        .await?
        .expect("head")
        .active_events()
        .iter()
        .filter(|record| match record {
            lash_core::SessionHistoryRecord::Conversation(message) => {
                matches!(
                    message.origin.as_ref(),
                    Some(lash_core::MessageOrigin::Plugin {
                        plugin_id,
                        transient: false,
                    }) if plugin_id == lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID
                ) && message
                    .parts
                    .iter()
                    .any(|part| part.content().contains(MARKER))
            }
            lash_core::SessionHistoryRecord::Protocol(_) => false,
        })
        .count();
    assert_eq!(
        rlm_marker_records, 0,
        "failed-attempt prose is preview output, not committed RLM history"
    );
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 1, "RLM scheduled a spurious iteration");
    }
    let first_model_calls =
        lash::testing::model_calls_for_activity(&session.durable(), &first.activities).await;
    assert_eq!(first_model_calls.len(), 1);
    assert_eq!(first_model_calls[0].attempts.len(), 1);
    let attempt = &first_model_calls[0].attempts[0];
    assert_eq!(
        attempt.protocol_position,
        lash_core::ProtocolPosition::OutputStarted
    );
    assert_eq!(
        attempt
            .retry_decision
            .as_ref()
            .map(|decision| decision.is_scheduled()),
        Some(false)
    );
    assert_eq!(
        attempt
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.denial_reason()),
        Some(lash_protocol_types::llm::types::ChargeSafetyDenialReason::GuaranteeRequired)
    );
    // The durable report is rebuilt from the store: the typed refusal is
    // the turn's committed failure evidence.
    let evidence = session
        .durable()
        .failure_evidence(None, std::num::NonZeroU32::MIN.saturating_add(7))
        .await?;
    let [settlement] = evidence.settlements.as_slice() else {
        panic!("one failed turn settles its evidence: {evidence:?}");
    };
    let [failure] = settlement.evidence.as_slice() else {
        panic!("one refused generation: {settlement:?}");
    };
    assert_eq!(
        failure.refusal.code(),
        lash::turn::TurnFailureCode::UnsafeRetryAfterOutputStarted.into()
    );
    assert_eq!(
        failure.refusal.denial_reason,
        lash_protocol_types::llm::types::ChargeSafetyDenialReason::GuaranteeRequired
    );

    let reopened = core
        .session(
            lash::SessionId::parse("rlm-provider-retry-prose").expect("nonblank host identity"),
        )
        .open()
        .await?;
    assert_eq!(
        reopened
            .read_view()
            .active_events()
            .iter()
            .filter(|record| match record {
                lash_core::SessionHistoryRecord::Conversation(message) => message
                    .parts
                    .iter()
                    .any(|part| part.content().contains(MARKER)),
                lash_core::SessionHistoryRecord::Protocol(_) => false,
            })
            .count(),
        0,
        "reloaded history retained failed-attempt prose"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rlm_natural_prose_completion_is_single_copy_in_next_request() -> Result<()> {
    const MARKER: &str = "natural completion single-copy marker";
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                natural_prose_reasoning_provider(Arc::clone(&requests)),
                mock_llm_profile_spec(),
            )
            .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("rlm-natural-prose-single-copy").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let first = session
        .send(TurnInput::text("answer naturally"))
        .output()
        .await?;
    assert_eq!(first.assistant_message(), Some(MARKER));
    Box::pin(session.admin().state().append_messages(
        vec![
            lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, MARKER)
                .with_id("workbench-assistant:natural-turn"),
        ],
        "host:observations:append_messages:766".to_string(),
    ))
    .await?
    .settle_with(
        &session.admin().commands(),
        lash::testing::admin_fixture_outcome,
    )
    .await?;

    session
        .send(TurnInput::text("check natural completion history"))
        .output()
        .await?;

    let requests = requests.lock_recover();
    assert_eq!(requests.len(), 2);
    let next_request = provider_request_text(&requests[1]);
    assert_eq!(
        next_request.matches(MARKER).count(),
        1,
        "provider-visible history duplicated natural prose: {next_request}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_observation_envelopes_scope_activity_and_commit_to_the_turn() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = created_session(
        &core,
        lash::SessionId::parse("session-observation-turn-identity")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    session
        .send(TurnInput::text("identify this turn"))
        .id(lash::TurnId::parse("observation-turn").expect("nonblank host identity"))
        .output()
        .await?;
    live.published(1).await;

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&cursor).await?
    else {
        panic!("fresh turn observation cursor should remain replayable");
    };
    let turn_activity = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(_)
            )
        })
        .collect::<Vec<_>>();
    assert!(!turn_activity.is_empty(), "turn emitted no activity");
    let lash_core::SessionObservationEventPayload::TurnActivity(first_activity) =
        &turn_activity[0].payload
    else {
        unreachable!("turn_activity contains only activity payloads");
    };
    assert!(
        matches!(
            &first_activity.event,
            TurnEvent::TurnStarted { turn_id, run_id } if turn_id == "observation-turn" && run_id.as_str() == "observation-turn"
        ),
        "replay must begin with the identity event, got {:?}",
        first_activity.event
    );
    assert!(
        turn_activity
            .iter()
            .all(|event| event.turn_id.as_deref() == Some("observation-turn")),
        "every turn activity must carry its producing turn identity"
    );
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
            )
        })
        .expect("turn commit observation");
    assert_eq!(committed.turn_id.as_deref(), Some("observation-turn"));
    Ok(())
}

/// A gap's replacement cursor sits at the durable head's commit in the
/// replay order (FIG-5322): an event published after that commit is news
/// to a consumer that rebuilt from the head, even when retention trimmed
/// the commit itself and the session's resident runtime reaches the head
/// only after the event. The turn runs through a Durable Session, so the
/// open session's resident is behind the head until the gap's snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trimmed_gap_replacement_cursor_preserves_unseen_auxiliary_event() -> Result<()> {
    let live = PublishedCommits::over(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard()
            },
        ),
    ));
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session_id =
        lash::SessionId::parse("trimmed-gap-unseen-auxiliary-event").expect("nonblank identity");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let stale_cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    core.session(session_id.clone())
        .durable()
        .await?
        .send(TurnInput::text("install replacement projection"))
        .output()
        .await?;
    let head = live.published(1).await;
    // The append notification precedes the node closing its publication
    // window. Finish that delivery before trimming the commit: while the
    // window is open a replacement cursor may still sit at the replay tail.
    // Draining joins the node without refreshing the open session's resident.
    core.drain().await.expect("the commit's publisher drained");
    lash_core::LiveReplayStore::publish(
        live.as_ref(),
        &session_id,
        head,
        vec![lash_core::LiveReplayEventDraft::new(
            None::<lash_core::TurnId>,
            lash_core::SessionObservationEventPayload::QueueChanged {
                kind: lash_core::SessionQueueEventKind::Enqueued,
                batch_ids: vec!["unseen-batch".to_string()],
            },
        )],
    )
    .await
    .expect("the auxiliary event is published");

    let SessionResume::Gap { gap, .. } =
        session.observe().resume_from_cursor(&stale_cursor).await?
    else {
        panic!("the trimmed cursor must yield a replacement gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(
        gap.latest_revision, head,
        "the replacement is the durable head"
    );

    let SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&gap.latest_cursor)
        .await?
    else {
        panic!("the replacement cursor must retain a replayable auxiliary suffix");
    };
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(
        &events[0].payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Enqueued
                && batch_ids == &["unseen-batch"]
    ));
    Ok(())
}

/// A resume from a snapshot taken between a turn's commit and its
/// publication never hands back that commit (FIG-5387): the snapshot
/// already holds it, so the commit's later publication is a redelivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_resume_never_replays_a_commit_the_snapshot_holds() -> Result<()> {
    let replay_store = Arc::new(PausedCommitReplayStore::at(
        PublicationBoundary::BeforePublish,
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay_store.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id = SessionId::fixture("snapshot-resume-held-commit");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let before = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("held by the snapshot"))
            .output()
            .await
    });

    replay_store.wait_for_commit_append().await;
    let snapshot = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    let held = snapshot
        .cursor
        .parse_for_session(&session_id)
        .expect("the snapshot's cursor names its session")
        .revision;
    assert!(
        held > before
            .cursor
            .parse_for_session(&session_id)
            .expect("the first cursor names its session")
            .revision,
        "the snapshot is the durable head, which commits before it publishes"
    );
    replay_store.release_commit_install();
    turn.await.expect("join publishing turn")?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let lash_core::LiveReplayOutcome::Replayed(events) =
                lash_core::LiveReplayStore::replay_after_cursor(
                    replay_store.as_ref(),
                    &before.cursor,
                )
                .await
                .expect("replay")
                && events.iter().any(|event| {
                    matches!(
                        event.payload,
                        lash_core::SessionObservationEventPayload::Committed { .. }
                    )
                })
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the turn's commit reaches the live replay");

    let SessionResume::Replayed { events } = session
        .observe()
        .resume_from_cursor(&snapshot.cursor)
        .await?
    else {
        panic!("the snapshot's cursor continues");
    };
    let held_commits = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            ) && event.revision() <= held
        })
        .collect::<Vec<_>>();
    assert!(
        held_commits.is_empty(),
        "a resume replayed commits the snapshot holds: {held_commits:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_revision_requires_replacement_evidence() -> Result<()> {
    let replay_store = Arc::new(FailingAppendReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay_store)
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("failed-commit-observation-reconciliation")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let before = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");

    let output = session
        .send(TurnInput::text("commit despite replay failure"))
        .output()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("echo: commit despite replay failure"),
        "the durable turn must still commit"
    );

    // The turn's commit is the durable head's revision.
    let head = session
        .observe()
        .snapshot()
        .await?
        .cursor
        .parse_for_session(&session.session_id())
        .expect("the head's cursor names its session")
        .revision;
    let before_revision = before
        .cursor
        .parse_for_session(&session.session_id())
        .expect("the snapshot's cursor names its session")
        .revision;
    assert!(head > before_revision, "the turn committed a new revision");
    let SessionResume::Gap { observation, gap } =
        session.observe().resume_from_cursor(&before.cursor).await?
    else {
        panic!("a pre-commit cursor without replacement evidence must not replay cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, head);
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "the unchanged live position must still carry the new durable revision"
    );

    let SessionObservationSubscription::Gap { observation, gap } = session
        .observe()
        .subscribe_from_cursor(&before.cursor)
        .await?
    else {
        panic!("a pre-commit cursor without replacement evidence must not subscribe cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, head);
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "subscribe must adopt the revision-stamped replacement cursor"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_reconnect_after_failed_append_yields_gap_without_another_commit() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(FailingAppendReplayStore::new()))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("idle-failed-commit-observation-reconciliation")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    session
        .send(TurnInput::text("commit before becoming idle"))
        .output()
        .await?;

    let mut reconnect = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_millis(250), reconnect.next())
        .await
        .expect("an idle reconnect must not wait for a future commit")
        .expect("recovery stream remains open")?;
    assert!(matches!(
        item,
        lash::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_subscribe_has_only_two_histories() -> Result<()> {
    for boundary in [
        PublicationBoundary::BeforePublish,
        PublicationBoundary::BeforeNotification,
    ] {
        let replay_store = Arc::new(PausedCommitReplayStore::at(boundary));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(replay_store.clone())
        .build(lash_facade_tests::runtime_lease_owner())?;
        let session_id = SessionId::fixture(format!("two-histories-{boundary:?}"));
        let session = created_session(&core, session_id).await.open().await?;
        let before = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .send(TurnInput::text("exactly once across the cut"))
                .output()
                .await
        });

        replay_store.wait_for_commit_append().await;
        let batch_is_visible = match lash_core::LiveReplayStore::replay_after_cursor(
            replay_store.as_ref(),
            &before.cursor,
        )
        .await
        .expect("boundary visibility probe must read replay")
        {
            lash_core::LiveReplayOutcome::Replayed(events) => events.iter().any(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
                )
            }),
            lash_core::LiveReplayOutcome::Gap(reason) => {
                panic!("{boundary:?}: boundary probe unexpectedly gapped: {reason:?}")
            }
        };
        assert_eq!(
            batch_is_visible,
            boundary == PublicationBoundary::BeforeNotification,
            "{boundary:?}: only the pre-notification cut may expose the batch to replay"
        );
        let snapshot = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let snapshot_is_new = snapshot.read_view.messages().iter().any(|message| {
            lash_facade_tests::fixtures::role_and_text(message)
                .1
                .contains("exactly once across the cut")
        });
        assert!(
            snapshot_is_new,
            "{boundary:?}: the snapshot is the durable head, which commits before it publishes"
        );
        let mut stream = session.observe().subscribe_and_recover(snapshot.cursor);
        replay_store.release_commit_install();
        turn.await.expect("join publishing turn")?;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
                .await
                .is_err(),
            "{boundary:?}: a snapshot of the durable head must not redeliver its publication"
        );
    }
    Ok(())
}

/// The same cut over a commit that switches frames (FIG-5532): the snapshot
/// holds the frame the commit switched to, so the commit's publication,
/// its `AgentFrameSwitched` included, is no news to its subscriber.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_subscribe_has_only_two_histories_across_a_frame_switch() -> Result<()> {
    for boundary in [
        PublicationBoundary::BeforePublish,
        PublicationBoundary::BeforeNotification,
    ] {
        // Publication is a protocol-independent commit guarantee. Use a
        // native tool so the publication deadline excludes kernel lowering.
        struct SwitchFrame;
        #[async_trait::async_trait]
        impl lash::tools::StaticToolExecute for SwitchFrame {
            async fn execute(
                &self,
                call: lash_core::ToolCall<'_>,
            ) -> lash_core::ToolAttemptOutcome {
                lash_core::ToolOutcome::turn_control(lash_core::TurnControl::SwitchAgentFrame {
                    frame_key: lash_core::FrameKey::from_call_site(
                        call.context.session_id().expect("session"),
                        call.context.agent_frame_id().expect("frame"),
                        call.context.call_id(),
                    ),
                    initial_nodes: Vec::new(),
                    task: "finish in a fresh frame".into(),
                    next_run: call
                        .context
                        .run_spec()
                        .expect("a tool body runs inside a turn")
                        .next_run()
                        .expect("the turn's spec encodes"),
                })
                .into()
            }
        }
        let definition = lash_core::ToolDefinition::control(
        "switch_frame",
        "tools.switch_frame",
        "Switch frames",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        lash_core::TurnControls::switch_agent_frame(),
    ).expect("tool schemas").with_execution(std::time::Duration::from_secs(120));
        let provider = lash::testing::TestProvider::builder()
            .kind("snapshot-frame-switch")
            .complete(|request| async move {
                if last_user_text(&request).contains("finish in a fresh frame") {
                    return Ok(text_response("done after frame switch"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "snapshot-switch".into(),
                        tool_name: "tools__switch_frame".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            })
            .build()
            .into_handle();
        let replay_store = Arc::new(PausedCommitReplayStore::at(boundary));
        let backend = sqlite_memory_store_backend().await;
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(
                lash::tool_provider_key!("tools"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    vec![definition],
                    SwitchFrame,
                )),
            )
            .live_replay_store(replay_store.clone())
            .build(lash_facade_tests::runtime_lease_owner())?;
        streaming_registered(&backend).await;
        let session_id = SessionId::fixture(format!("two-histories-switch-{boundary:?}"));
        let session = created_session(&core, session_id.clone())
            .await
            .open()
            .await?;
        let before = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .send(TurnInput::text("switch frames"))
                .output()
                .await
        });

        streaming_boundary(
            &backend,
            &session_id,
            &format!("{boundary:?}"),
            replay_store.wait_for_commit_append(),
        )
        .await;
        let snapshot = session
            .observe()
            .snapshot()
            .await
            .expect("durable snapshot");
        let revision_of = |cursor: &lash_core::SessionCursor| {
            cursor
                .parse_for_session(&session_id)
                .expect("the session's cursor")
                .revision
        };
        let held = revision_of(&snapshot.cursor);
        assert!(
            held > revision_of(&before.cursor),
            "{boundary:?}: the snapshot is the durable head, which commits before it publishes"
        );
        let mut stream = session.observe().subscribe_and_recover(snapshot.cursor);
        replay_store.release_commit_install();
        let switched = turn.await.expect("join publishing turn")?;
        let lash_core::facade_support::TurnOutcome::AgentFrameSwitch { frame_key, .. } =
            &switched.outcome
        else {
            panic!("the switch answers its send: {:?}", switched.outcome);
        };
        // The frame's task runs next as its own run (FIG-5232); its commit
        // is news to the subscriber, and switches nothing.
        session
            .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                frame_key,
            ))
            .output()
            .await?;

        let lash_core::LiveReplayOutcome::Replayed(published) =
            lash_core::LiveReplayStore::replay_after_cursor(replay_store.as_ref(), &before.cursor)
                .await
                .expect("read the published history")
        else {
            panic!("{boundary:?}: the published history is retained");
        };
        let switch = published
            .iter()
            .find_map(|event| match &event.payload {
                lash_core::SessionObservationEventPayload::AgentFrameSwitched {
                    commit, ..
                } => Some((*commit, event.revision())),
                _ => None,
            })
            .expect("the switching commit publishes its frame switch");
        assert_eq!(
            switch.0,
            Some(switch.1),
            "{boundary:?}: a commit's frame switch names the commit"
        );
        assert!(
            switch.1 <= held,
            "{boundary:?}: the snapshot holds the switching commit"
        );

        while let Ok(Some(item)) =
            tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await
        {
            if let lash::observe::SessionObservationStreamItem::Event(event) = item? {
                assert!(
                    !matches!(
                        &event.payload,
                        lash_core::SessionObservationEventPayload::AgentFrameSwitched { .. }
                    ),
                    "{boundary:?}: a snapshot of the durable head must not redeliver its frame switch"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incarnation_change_invalidates_cursor() {
    let original = lash::observe::InMemoryLiveReplayStore::new(
        lash::observe::InMemoryLiveReplayStoreConfig::standard(),
    );
    let preserved = lash::observe::InMemoryLiveReplayStore::reopen_preserving_history(&original);
    lash_conformance::incarnation_change_invalidates_cursor(
        Arc::new(original),
        Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig::standard(),
        )),
        Arc::new(preserved),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_authority_matches_revision_transition() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = created_session(
        &core,
        lash::SessionId::parse("payload-authority-transition").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let initial = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");

    session
        .send(TurnInput::text("durable transition"))
        .output()
        .await?;
    live.published(1).await;
    let committed = session
        .observe()
        .resume_from_cursor(&initial.cursor)
        .await?;
    let SessionResume::Replayed { events } = committed else {
        panic!("durable transition must replay its committed evidence");
    };
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
            )
        })
        .expect("durable transition emitted Committed");
    let lash_core::SessionObservationEventPayload::Committed {
        base_revision,
        entries: rows,
    } = &committed.payload
    else {
        unreachable!()
    };
    assert_eq!(
        *base_revision,
        initial
            .cursor
            .parse_for_session(&session.session_id())
            .expect("the snapshot's cursor names its session")
            .revision,
        "the commit's rows extend the revision the snapshot held"
    );
    assert!(!rows.is_empty(), "the commit carries the rows it added");
    assert!(
        committed.revision() > *base_revision,
        "the commit's revision follows the one its rows extend"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_chat_conformance_deduplicates_redelivery_identity() -> Result<()> {
    let live = PublishedCommits::unbounded();
    let core = core_publishing_to(Arc::clone(&live)).await?;
    let session = created_session(
        &core,
        lash::SessionId::parse("recoverable-chat-redelivery").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    session
        .send(TurnInput::text("redelivery identity"))
        .id(lash::TurnId::parse("recoverable-redelivery-turn").expect("nonblank host identity"))
        .output()
        .await?;
    live.published(1).await;

    let mut first_delivery = session.observe().subscribe_and_recover(cursor.clone());
    let first_id = match first_delivery.next().await.expect("first replay event")? {
        lash::observe::SessionObservationStreamItem::Event(event) => {
            lash::observe::SessionObservationEventId::of(&event)
        }
        lash::observe::SessionObservationStreamItem::Gap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };

    let mut redelivery = session
        .observe()
        .subscribe_and_recover(cursor.clone())
        .with_applied_event_ids([first_id.clone()]);
    let next_id = match redelivery.next().await.expect("next replay event")? {
        lash::observe::SessionObservationStreamItem::Event(event) => {
            lash::observe::SessionObservationEventId::of(&event)
        }
        lash::observe::SessionObservationStreamItem::Gap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };
    assert_ne!(
        next_id, first_id,
        "an already-applied event identity must not be delivered twice"
    );
    let mut unlimited_redelivery = session
        .observe()
        .subscribe_and_recover(cursor)
        .with_work_limits(lash::tracing::ObservationWorkLimits {
            session_dedup_ids: 0,
            ..Default::default()
        })
        .with_applied_event_ids([first_id.clone()]);
    let lash::observe::SessionObservationStreamItem::Event(event) =
        unlimited_redelivery.next().await.unwrap()?
    else {
        panic!("unexpected gap")
    };
    assert_eq!(
        lash::observe::SessionObservationEventId::of(&event),
        first_id
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_replacement_then_continuation_after_trimmed_history() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard()
            },
        ),
    ))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("recoverable-chat-gap").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    session
        .send(TurnInput::text("trim the initial cursor"))
        .output()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    let update = stream.next().await.expect("gap update")?;
    let lash::observe::SessionObservationStreamItem::Gap {
        observation: snapshot,
        gap,
    } = update
    else {
        panic!("trimmed cursor must be forwarded as a recoverable gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, snapshot.cursor);

    session
        .send(TurnInput::text("live after gap"))
        .output()
        .await?;
    let continued = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match stream.next().await.expect("post-gap live update")? {
                lash::observe::SessionObservationStreamItem::Gap {
                    observation: snapshot,
                    ..
                } => {
                    break Ok::<_, lash::EmbedError>(
                        snapshot
                            .read_view
                            .messages()
                            .iter()
                            .map(|message| lash_facade_tests::fixtures::role_and_text(message).1)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                lash::observe::SessionObservationStreamItem::Event(event) => {
                    if let lash_core::SessionObservationEventPayload::Committed {
                        entries: rows,
                        ..
                    } = &event.payload
                    {
                        break Ok(format!("{rows:?}"));
                    }
                }
            }
        }
    })
    .await
    .expect("post-gap continuation timeout")?;
    assert!(
        continued.contains("live after gap"),
        "continued recovery must carry the next turn"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recoverable_chat_conformance_disconnect_does_not_cancel_server_work() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = lash::testing::TestProvider::builder()
        .kind("recoverable-chat-disconnect")
        .complete({
            let entered_tx = Arc::clone(&entered_tx);
            let release = Arc::clone(&release);
            move |_request| {
                let entered_tx = Arc::clone(&entered_tx);
                let release = Arc::clone(&release);
                async move {
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    release.notified().await;
                    Ok(text_response("completed after observer disconnect"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("recoverable-chat-disconnect").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let stream = session.observe().subscribe_and_recover(cursor);
    let run_session = session.clone();
    let mut turn = tokio::spawn(async move {
        run_session
            .send(TurnInput::text("keep running"))
            .id(lash::TurnId::parse("disconnect-is-not-cancel").expect("nonblank host identity"))
            .output()
            .await
    });
    entered_rx.await.expect("provider entered");
    drop(stream);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut turn)
            .await
            .is_err(),
        "disconnecting observation must not cancel server work"
    );
    release.notify_one();
    let result = turn.await.expect("join turn")?;
    assert!(matches!(result.outcome, TurnOutcome::Finished(_)));
    Ok(())
}
