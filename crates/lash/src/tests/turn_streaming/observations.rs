use super::*;

#[tokio::test]
pub(super) async fn turn_builder_stream_emits_activities_and_finishes() -> Result<()> {
    let core = standard_core();
    let session = core.session("turn-stream").open().await?;
    let mut stream = session.turn(TurnInput::text("stream me")).stream()?;

    let mut activities = Vec::new();
    while let Some(activity) = stream.next().await {
        activities.push(activity?);
    }
    let result = stream.finish().await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(assistant_prose(&activities), "echo: stream me");
    assert!(
        activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::AssistantProseDelta { .. }))
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_replays_live_activity_and_commit() -> Result<()> {
    let core = standard_core();
    let session = core.session("session-observation-replay").open().await?;
    let cursor = session.observe().current_observation().cursor;

    let output = session.turn(TurnInput::text("observe me")).run().await?;
    assert_eq!(assistant_prose(&output.activities), "echo: observe me");

    let replay = session.observe().resume_from_cursor(&cursor)?;
    let SessionResume::Replayed { events } = replay else {
        panic!("recent cursor should replay live events");
    };
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::TurnActivity(activity)
                if matches!(
                    &activity.event,
                    TurnEvent::AssistantProseDelta { text } if text.as_ref() == "echo: observe me"
                )
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::Committed { .. }
        )
    }));
    Ok(())
}

pub(super) fn retrying_visible_stream_provider() -> ProviderHandle {
    let attempts = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retrying-visible-stream")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(3)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request| {
            let attempts = Arc::clone(&attempts);
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                let stream = request.stream_events.expect("stream events");
                stream.send(LlmStreamEvent::ReasoningDelta(format!(
                    "reasoning-{attempt}"
                )));
                stream.send(LlmStreamEvent::Delta(format!("prose-{attempt}")));
                if attempt < 3 {
                    return Err(LlmTransportError::new(format!("retry attempt {attempt}"))
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                        ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "prose-3".to_string(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
pub(super) fn output_then_failing_rlm_prose_provider(
    transport_calls: Arc<AtomicUsize>,
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("retrying-rlm-prose")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request| {
            let transport_calls = Arc::clone(&transport_calls);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock_recover()
                    .push(request.clone());
                let call = transport_calls.fetch_add(1, Ordering::SeqCst);
                let stream = request.stream_events.expect("stream events");
                if call == 0 {
                    stream.send(LlmStreamEvent::Delta(
                        "retry observer single-copy marker\n<lashlang>\n".to_string(),
                    ));
                    return Err(
                        LlmTransportError::new("deterministic rate limit")
                            .with_status(429)
                            .with_output_started(true),
                    );
                }
                let text = match call {
                    1 => {
                        "retry observer single-copy marker\n<lashlang>\nretry_missing_name\n</lashlang>"
                    }
                    2 => "<lashlang>\nfinish \"provider retry succeeded\"\n</lashlang>",
                    _ => "<lashlang>\nfinish \"subsequent turn succeeded\"\n</lashlang>",
                };
                stream.send(LlmStreamEvent::Delta(text.to_string()));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
pub(super) fn natural_prose_reasoning_provider(
    requests: Arc<StdMutex<Vec<lash_core::LlmRequest>>>,
) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("natural-rlm-prose-reasoning")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let calls = Arc::clone(&calls);
            async move {
                requests.lock_recover().push(request);
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let text = match call {
                    0 => "natural completion single-copy marker",
                    1 => "subsequent natural answer",
                    other => panic!("unexpected natural RLM provider request {other}"),
                };
                let mut parts = Vec::new();
                if call == 0 {
                    parts.push(LlmOutputPart::Reasoning {
                        text: "reasoning retained for replay".to_string(),
                        replay: Some(lash_core::llm::types::ProviderReasoningReplay {
                            item_id: Some("natural-reasoning".to_string()),
                            encrypted_content: Some("opaque-natural-replay".to_string()),
                            signature: None,
                            redacted: false,
                            summary: Vec::new(),
                            ..Default::default()
                        }),
                    });
                }
                parts.push(LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                });
                Ok(LlmResponse {
                    parts,
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
pub(super) fn provider_request_text(request: &lash_core::LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_provider_failure_after_prose_is_not_retried_or_committed() -> Result<()> {
    run_async_test_on_stack_budget("rlm-provider-output-failure-test", || async {
        const MARKER: &str = "retry observer single-copy marker";
        let transport_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(output_then_failing_rlm_prose_provider(
            Arc::clone(&transport_calls),
            Arc::clone(&requests),
        ))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-provider-retry-prose").open().await?;

        let first = session
            .turn(TurnInput::text("trigger deterministic rate limit retry"))
            .run()
            .await?;

        assert_eq!(
            transport_calls.load(Ordering::SeqCst),
            1,
            "provider output must not be re-bought after the failed attempt"
        );
        assert!(matches!(
            first.result.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
        ));
        assert!(first.result.assistant_output.safe_text.is_empty());
        assert!(first.result.assistant_output.raw_text.is_empty());
        assert!(first.activities.iter().any(|activity| matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text } if text.contains(MARKER)
        )));
        assert!(
            first
                .activities
                .iter()
                .all(|activity| !matches!(activity.event, TurnEvent::ModelAttemptReset { .. }))
        );
        let rlm_marker_records = first
            .result
            .state
            .read_view()
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
                        .any(|part| part.content.contains(MARKER))
                }
                lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                    lash_protocol_rlm::decode_rlm_protocol_event(event),
                    Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                        if content.prose.contains(MARKER)
                ),
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
        assert_eq!(first.result.llm_calls.len(), 1);
        assert_eq!(first.result.llm_calls[0].attempts.len(), 1);
        let attempt = &first.result.llm_calls[0].attempts[0];
        assert_eq!(
            attempt.protocol_position,
            lash_core::ProtocolPosition::OutputStarted
        );
        assert_eq!(
            attempt
                .retry_decision
                .as_ref()
                .map(|decision| decision.scheduled),
            Some(false)
        );
        assert_eq!(
            attempt
                .retry_decision
                .as_ref()
                .and_then(|decision| decision.reason.as_deref()),
            Some("output_started_without_retry_guarantee")
        );
        let issue = first.result.errors.first().expect("typed provider issue");
        assert_eq!(
            issue.code.as_deref(),
            Some("unsafe_retry_after_output_started")
        );
        assert_eq!(issue.retryable, Some(false));

        let persisted = session.admin().state().persist_current().await?;
        session.close().await?;

        let reopened = core.session("rlm-provider-retry-prose").open().await?;
        reopened.admin().state().set_persisted(persisted).await?;
        assert_eq!(
            reopened
                .read_view()
                .active_events()
                .iter()
                .filter(|record| match record {
                    lash_core::SessionHistoryRecord::Conversation(message) => message
                        .parts
                        .iter()
                        .any(|part| part.content.contains(MARKER)),
                    lash_core::SessionHistoryRecord::Protocol(event) => matches!(
                        lash_protocol_rlm::decode_rlm_protocol_event(event),
                        Some(lash_rlm_types::RlmProtocolEvent::RlmAssistantContent(content))
                            if content.prose.contains(MARKER)
                    ),
                })
                .count(),
            0,
            "reloaded history retained failed-attempt prose"
        );
        Ok(())
    })
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_natural_prose_completion_is_single_copy_in_next_request() -> Result<()> {
    run_async_test_on_stack_budget("rlm-natural-prose-single-copy-test", || async {
        const MARKER: &str = "natural completion single-copy marker";
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            lash_core::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(natural_prose_reasoning_provider(Arc::clone(&requests)))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("rlm-natural-prose-single-copy").open().await?;

        let first = session
            .turn(TurnInput::text("answer naturally"))
            .run()
            .await?;
        assert_eq!(first.assistant_message(), Some(MARKER));
        session
            .admin()
            .state()
            .append_messages(vec![
                lash_core::PluginMessage::text(lash_core::MessageRole::Assistant, MARKER)
                    .with_id("workbench-assistant:natural-turn"),
            ])
            .await?;

        session
            .turn(TurnInput::text("check natural completion history"))
            .run()
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
    })
}

pub(super) fn render_observed_attempt_text(
    events: &[Arc<lash_core::SessionObservationEvent>],
) -> (String, String) {
    let mut prose = Vec::new();
    let mut reasoning = Vec::new();
    for event in events {
        let lash_core::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            continue;
        };
        match &activity.event {
            TurnEvent::AssistantProseDelta { text } => {
                prose.push((activity.correlation_id.clone(), text.clone()));
            }
            TurnEvent::ReasoningDelta { text } => {
                reasoning.push((activity.correlation_id.clone(), text.clone()));
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                reasoning_correlation_ids,
            } => {
                prose.retain(|(correlation_id, _)| {
                    !assistant_prose_correlation_ids.contains(correlation_id)
                });
                reasoning.retain(|(correlation_id, _)| {
                    !reasoning_correlation_ids.contains(correlation_id)
                });
            }
            _ => {}
        }
    }
    (
        prose
            .into_iter()
            .map(|(_, text)| text.to_string())
            .collect(),
        reasoning
            .into_iter()
            .map(|(_, text)| text.to_string())
            .collect(),
    )
}

pub(super) fn model_attempt_resets(events: &[Arc<lash_core::SessionObservationEvent>]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(activity)
                    if matches!(&activity.event, TurnEvent::ModelAttemptReset { .. })
            )
        })
        .count()
}

#[tokio::test]
pub(super) async fn session_observation_envelopes_scope_activity_and_commit_to_the_turn()
-> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-turn-identity")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("identify this turn"))
        .turn_id("observation-turn")
        .run()
        .await?;

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&cursor)?
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
            TurnEvent::TurnStarted { turn_id } if turn_id == "observation-turn"
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
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        })
        .expect("turn commit observation");
    assert_eq!(committed.turn_id.as_deref(), Some("observation-turn"));
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_retracts_two_retried_visible_attempts_live_and_on_replay()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(retrying_visible_stream_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("retry-visible-observation").open().await?;
    let cursor = session.observe().current_observation().cursor;
    let lash_core::facade_support::SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("fresh cursor should subscribe without a gap");
    };
    let live_collector = tokio::spawn(async move {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                futures_util::StreamExt::next(&mut subscription),
            )
            .await
            .expect("timed out waiting for live observation")
            .expect("live observation subscription closed")
            .expect("live observation event");
            let committed = matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            );
            events.push(event);
            if committed {
                break;
            }
        }
        events
    });

    let output = session
        .turn(TurnInput::text("retry twice after visible output"))
        .run()
        .await?;
    assert_eq!(output.assistant_message(), Some("prose-3"));
    let live_events = live_collector.await.expect("live collector task");

    let lash_core::facade_support::SessionResume::Replayed {
        events: replay_events,
    } = session.observe().resume_from_cursor(&cursor)?
    else {
        panic!("recent cursor should replay all attempt activity");
    };

    assert_eq!(
        render_observed_attempt_text(&live_events),
        ("prose-3".to_string(), "reasoning-3".to_string())
    );
    assert_eq!(
        render_observed_attempt_text(&replay_events),
        ("prose-3".to_string(), "reasoning-3".to_string())
    );
    assert_eq!(model_attempt_resets(&live_events), 2);
    assert_eq!(model_attempt_resets(&replay_events), 2);
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_rejects_cursor_from_another_session() -> Result<()> {
    let core = standard_core();
    let session = core.session("session-observation-a").open().await?;
    let other = core.session("session-observation-b").open().await?;
    let other_cursor = other.observe().current_observation().cursor;

    let err = session
        .observe()
        .resume_from_cursor(&other_cursor)
        .expect_err("cursor from another session should be rejected");
    assert!(
        err.to_string().contains("session-observation-b")
            && err.to_string().contains("session-observation-a"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_subscription_replays_buffered_events_before_live_events()
-> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-subscribe-replay")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("first observed"))
        .run()
        .await?;
    let SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("recent cursor should subscribe without a gap");
    };

    loop {
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut subscription),
        )
        .await
        .expect("timed out waiting for replayed event")
        .expect("replay subscription closed")
        .expect("replayed event");
        if observation_assistant_delta(&event).as_deref() == Some("echo: first observed") {
            break;
        }
    }

    session
        .turn(TurnInput::text("second observed"))
        .run()
        .await?;
    loop {
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut subscription),
        )
        .await
        .expect("timed out waiting for live event")
        .expect("live subscription closed")
        .expect("live event");
        if observation_assistant_delta(&event).as_deref() == Some("echo: second observed") {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_recovery_stream_replays_buffered_events_before_live_events()
-> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-recovered-stream")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("first recovered"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);

    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for replayed stream item")
            .expect("replayed stream should stay open")?;
        if let crate::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: first recovered")
        {
            break;
        }
    }

    session
        .turn(TurnInput::text("second recovered"))
        .run()
        .await?;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for live stream item")
            .expect("live stream should stay open")?;
        if let crate::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: second recovered")
        {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_remote_subscription_replays_dto_events() -> Result<()> {
    let core = standard_core();
    let session = core
        .session("session-observation-remote-subscribe")
        .open()
        .await?;
    let observation = session.observe().current_remote_observation();
    assert_eq!(
        observation.session_id,
        "session-observation-remote-subscribe"
    );

    session
        .turn(TurnInput::text("remote observed"))
        .run()
        .await?;
    let crate::observe::RemoteSessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_remote_cursor(
            &crate::remote::observations::RemoteSessionCursor::new(observation.cursor.clone()),
        )?
    else {
        panic!("recent remote cursor should subscribe without a gap");
    };

    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for remote replayed event")
                .expect("remote replayed event");
        if remote_observation_assistant_delta(&event).as_deref() == Some("echo: remote observed") {
            assert_eq!(event.session_id, "session-observation-remote-subscribe");
            break;
        }
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn session_observation_remote_recovery_stream_yields_dto_gap() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("session-observation-remote-gap")
        .open()
        .await?;
    let observation = session.observe().current_remote_observation();

    session
        .turn(TurnInput::text("trimmed before remote subscribe"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover_remote(
        crate::remote::observations::RemoteSessionCursor::new(observation.cursor),
    )?;
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for remote gap stream item")
        .expect("remote recovery stream should stay open")?;
    let crate::observe::RemoteSessionObservationStreamItem::Gap { observation, gap } = item else {
        panic!("trimmed remote cursor should yield a gap item");
    };

    assert_eq!(
        gap.reason,
        crate::remote::observations::RemoteLiveReplayGapReason::Trimmed
    );
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_eq!(observation.session_id, "session-observation-remote-gap");
    Ok(())
}

#[tokio::test]
pub(super) async fn capacity_and_age_trim_force_snapshot_with_matching_observation_cursor()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("session-observation-recovered-gap")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("trimmed before subscribe"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for gap stream item")
        .expect("recovery stream should stay open")?;
    let crate::observe::SessionObservationStreamItem::Gap { observation, gap } = item else {
        panic!("trimmed cursor should yield a gap item");
    };

    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, observation.cursor);
    Ok(())
}

#[tokio::test]
pub(super) async fn trimmed_gap_replacement_cursor_preserves_unseen_auxiliary_event() -> Result<()>
{
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("trimmed-gap-unseen-auxiliary-event")
        .open()
        .await?;
    let stale_cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("install replacement projection"))
        .run()
        .await?;
    let installed_projection = session.observe().current_observation();
    session.observe().runtime.record_queue_changed(
        lash_core::SessionQueueEventKind::Enqueued,
        vec!["unseen-batch".to_string()],
    );

    let SessionResume::Gap { gap, .. } = session.observe().resume_from_cursor(&stale_cursor)?
    else {
        panic!("the trimmed cursor must yield a replacement gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(
        gap.latest_cursor, installed_projection.cursor,
        "the replacement cursor must stay before auxiliary events absent from the projection"
    );

    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&gap.latest_cursor)?
    else {
        panic!("the replacement cursor must retain a replayable auxiliary suffix");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].payload,
        lash_core::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
            if *kind == lash_core::SessionQueueEventKind::Enqueued
                && batch_ids == &["unseen-batch"]
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn recoverable_chat_conformance_snapshot_subscription_and_terminal_replacement()
-> Result<()> {
    let core = standard_core();
    let session = core.session("recoverable-chat-terminal").open().await?;
    let snapshot = session.observe().recoverable_chat_snapshot();
    assert!(snapshot.read_view.messages().is_empty());
    let mut stream = session
        .observe()
        .subscribe_recoverable_chat(snapshot.cursor);

    session
        .turn(TurnInput::text("terminal replacement"))
        .turn_id("recoverable-terminal-turn")
        .run()
        .await?;

    let terminal = loop {
        let update = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("recoverable chat terminal timeout")
            .expect("recoverable chat stream stays open")?;
        if let crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
            snapshot,
            ..
        } = update
        {
            break snapshot;
        }
    };
    assert!(
        terminal
            .read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("terminal replacement")),
        "terminal replacement must carry the authoritative committed transcript"
    );
    Ok(())
}

#[derive(Debug)]
pub(super) struct PausedCommitReplayStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    boundary: PublicationBoundary,
    pause: Arc<PublicationPause>,
}

#[derive(Debug)]
pub(super) struct PublicationPause {
    boundary_reached: std::sync::atomic::AtomicBool,
    release_boundary: std::sync::atomic::AtomicBool,
    pause_lock: StdMutex<()>,
    pause_changed: std::sync::Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicationBoundary {
    BeforeReservation,
    AfterReservation,
    AfterInstall,
    BeforeNotification,
}

#[derive(Debug)]
pub(super) struct NoopTurnPhaseProbe;

impl lash_core::runtime::RuntimeTurnPhaseProbe for NoopTurnPhaseProbe {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

#[derive(Debug)]
pub(super) struct FailingAppendReplayStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
}

impl FailingAppendReplayStore {
    fn new() -> Self {
        Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::default(),
        }
    }
}

impl lash_core::LiveReplayStore for FailingAppendReplayStore {
    fn prepare_publication(
        &self,
        _session_id: &SessionId,
        _revision: lash_core::SessionRevision,
        _events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        lash_core::PreparedLiveReplayPublication,
        lash_core::LiveReplayStoreError,
    > {
        Err(lash_core::LiveReplayStoreError::Store(
            "injected live-replay append failure".to_string(),
        ))
    }

    fn publish_prepared(
        &self,
        _prepared: lash_core::PreparedLiveReplayPublication,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        unreachable!("failed preparations cannot be published")
    }

    fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor)
    }

    fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor)
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id)
    }
}

#[tokio::test]
pub(super) async fn durable_revision_requires_replacement_evidence() -> Result<()> {
    let replay_store = Arc::new(FailingAppendReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(replay_store)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("failed-commit-observation-reconciliation")
        .open()
        .await?;
    let before = session.observe().current_observation();

    let output = session
        .turn(TurnInput::text("commit despite replay failure"))
        .run()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("echo: commit despite replay failure"),
        "the durable turn must still commit"
    );

    let SessionResume::Gap { observation, gap } =
        session.observe().resume_from_cursor(&before.cursor)?
    else {
        panic!("a pre-commit cursor without replacement evidence must not replay cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, lash_core::SessionRevision::new(1));
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "the unchanged live position must still carry the new durable revision"
    );

    let SessionObservationSubscription::Gap { observation, gap } =
        session.observe().subscribe_from_cursor(&before.cursor)?
    else {
        panic!("a pre-commit cursor without replacement evidence must not subscribe cleanly");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, lash_core::SessionRevision::new(1));
    assert_eq!(gap.requested_cursor, before.cursor);
    assert_eq!(gap.latest_cursor, observation.cursor);
    assert_ne!(
        gap.latest_cursor, before.cursor,
        "subscribe must adopt the revision-stamped replacement cursor"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn idle_session_reconnect_after_failed_append_yields_gap_without_another_commit()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(FailingAppendReplayStore::new()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("idle-failed-commit-observation-reconciliation")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;

    session
        .turn(TurnInput::text("commit before becoming idle"))
        .run()
        .await?;

    let mut reconnect = session.observe().subscribe_and_recover(cursor);
    let item = tokio::time::timeout(std::time::Duration::from_millis(250), reconnect.next())
        .await
        .expect("an idle reconnect must not wait for a future commit")
        .expect("recovery stream remains open")?;
    assert!(matches!(
        item,
        crate::observe::SessionObservationStreamItem::Gap {
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
pub(super) async fn snapshot_subscribe_has_only_two_histories() -> Result<()> {
    for boundary in [
        PublicationBoundary::BeforeReservation,
        PublicationBoundary::AfterReservation,
        PublicationBoundary::AfterInstall,
        PublicationBoundary::BeforeNotification,
    ] {
        let replay_store = Arc::new(PausedCommitReplayStore::at(boundary));
        let core =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(mock_provider())
                .model(mock_model_spec())
                .live_replay_store(replay_store.clone())
                .build(crate::testing::runtime_lease_owner())?;
        let session_id = SessionId::from(format!("two-histories-{boundary:?}"));
        let session = core.session(session_id).open().await?;
        let before = session.observe().recoverable_chat_snapshot();
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .turn(TurnInput::text("exactly once across the cut"))
                .run()
                .await
        });

        replay_store.wait_for_commit_append().await;
        let batch_is_visible = match lash_core::LiveReplayStore::replay_after_cursor(
            replay_store.as_ref(),
            &before.cursor,
        )
        .expect("boundary visibility probe must read replay")
        {
            lash_core::LiveReplayOutcome::Replayed(events) => events.iter().any(|event| {
                matches!(
                    event.payload,
                    lash_core::SessionObservationEventPayload::Committed { .. }
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
        let snapshot = session.observe().recoverable_chat_snapshot();
        let snapshot_is_new =
            snapshot.read_view.messages().iter().any(|message| {
                crate::message_text(message).contains("exactly once across the cut")
            });
        assert_eq!(
            snapshot_is_new,
            matches!(
                boundary,
                PublicationBoundary::AfterInstall | PublicationBoundary::BeforeNotification
            ),
            "{boundary:?}: projection installation must divide the two allowed histories"
        );
        let mut stream = session
            .observe()
            .subscribe_recoverable_chat(snapshot.cursor);
        replay_store.release_commit_install();
        turn.await.expect("join publishing turn")?;

        if snapshot_is_new {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
                    .await
                    .is_err(),
                "{boundary:?}: a new snapshot must not redeliver its reserved publication"
            );
        } else {
            let mut replacements = 0;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while let Some(update) = stream.next().await {
                    if matches!(
                        update?,
                        crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { .. }
                    ) {
                        replacements += 1;
                        break;
                    }
                }
                Ok::<_, crate::EmbedError>(())
            })
            .await
            .expect("old snapshot did not receive its complete publication")?;
            assert_eq!(
                replacements, 1,
                "{boundary:?}: an old snapshot must receive the batch exactly once"
            );
        }
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn incarnation_change_invalidates_cursor() {
    let original = crate::observe::InMemoryLiveReplayStore::default();
    let preserved = crate::observe::InMemoryLiveReplayStore::reopen_preserving_history(&original);
    lash_conformance::incarnation_change_invalidates_cursor(
        Arc::new(original),
        Arc::new(crate::observe::InMemoryLiveReplayStore::default()),
        Arc::new(preserved),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn notification_observes_installed_projection() -> Result<()> {
    let replay_store = Arc::new(PausedCommitReplayStore::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(replay_store.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("notification-observes-installed-projection")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;
    let SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor)?
    else {
        panic!("a fresh cursor must subscribe without a gap");
    };

    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("projection before notification"))
            .run()
            .await
    });
    replay_store.wait_for_commit_append().await;
    let installed_before_notification = session.observe().current_observation();
    let committed_escaped = tokio::time::timeout(std::time::Duration::from_millis(25), async {
        loop {
            let event = subscription
                .next()
                .await
                .expect("notification subscription remains open")
                .expect("notification before committed publication");
            if matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            ) {
                return;
            }
        }
    })
    .await
    .is_ok();
    replay_store.release_commit_install();
    let notification = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
            .await
            .expect("timed out waiting for committed notification")
            .expect("notification subscription remains open")
            .expect("committed notification");
        if matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::Committed { .. }
        ) {
            break event;
        }
    };
    let projection_at_notification = session.observe().current_observation();
    turn.await.expect("join publishing turn")?;

    assert_eq!(installed_before_notification.read_view.turn_index(), 1);
    assert!(
        installed_before_notification
            .read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("projection before notification")),
        "the authoritative projection must be installed before publication enters notify"
    );
    assert!(
        !committed_escaped,
        "no committed notification may escape while publish_prepared is gated"
    );
    assert_eq!(
        projection_at_notification.cursor, notification.cursor,
        "a Committed notification must not be observable before its authoritative projection is installed"
    );

    replay_store.arm_pause();
    let resident_cursor = projection_at_notification.cursor.clone();
    let SessionObservationSubscription::Subscribed(mut resident_subscription) =
        session.observe().subscribe_from_cursor(&resident_cursor)?
    else {
        panic!("the committed cursor must remain subscribable");
    };
    let resident_session = session.clone();
    let resident = tokio::spawn(async move {
        resident_session
            .set_turn_phase_probe(Arc::new(NoopTurnPhaseProbe))
            .await;
    });
    replay_store.wait_for_commit_append().await;
    let installed_resident = session.observe().current_observation();
    assert_eq!(
        installed_resident.read_view.turn_index(),
        projection_at_notification.read_view.turn_index(),
        "resident publication must not claim a durable revision transition"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            resident_subscription.next(),
        )
        .await
        .is_err(),
        "no resident notification may escape before its projection is installed"
    );
    replay_store.release_commit_install();
    let resident_event = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        resident_subscription.next(),
    )
    .await
    .expect("resident notification timeout")
    .expect("resident subscription stays open")
    .expect("resident notification");
    resident.await.expect("join resident publication");
    assert!(matches!(
        resident_event.payload,
        lash_core::SessionObservationEventPayload::ResidentChanged { .. }
    ));
    assert_eq!(
        session.observe().current_observation().cursor,
        resident_event.cursor,
        "a resident notification must observe its installed projection"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn payload_authority_matches_revision_transition() -> Result<()> {
    let core = standard_core();
    let session = core.session("payload-authority-transition").open().await?;
    let initial = session.observe().current_observation();

    session
        .turn(TurnInput::text("durable transition"))
        .run()
        .await?;
    let committed = session.observe().resume_from_cursor(&initial.cursor)?;
    let SessionResume::Replayed { events } = committed else {
        panic!("durable transition must replay its committed evidence");
    };
    let committed = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        })
        .expect("durable transition emitted Committed");
    assert_eq!(committed.revision(), lash_core::SessionRevision::new(1));
    let lash_core::SessionObservationEventPayload::Committed { read_view } = &committed.payload
    else {
        unreachable!()
    };
    assert_eq!(read_view.turn_index(), 1);

    let committed_cursor = committed.cursor.clone();
    let probe: Arc<dyn lash_core::runtime::RuntimeTurnPhaseProbe> = Arc::new(NoopTurnPhaseProbe);
    session.set_turn_phase_probe(Arc::clone(&probe)).await;
    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&committed_cursor)?
    else {
        panic!("resident transition must remain replayable");
    };
    assert!(matches!(
        events.as_slice(),
        [event]
            if event.revision() == lash_core::SessionRevision::new(1)
                && matches!(
                    event.payload,
                    lash_core::SessionObservationEventPayload::ResidentChanged { .. }
                )
    ));

    let resident_cursor = events[0].cursor.clone();
    session.set_turn_phase_probe(probe).await;
    let SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&resident_cursor)?
    else {
        panic!("a no-op publication must preserve clean continuity");
    };
    assert!(events.is_empty(), "a no-op publication must emit no event");
    Ok(())
}

impl PausedCommitReplayStore {
    fn new() -> Self {
        Self::at(PublicationBoundary::AfterInstall)
    }

    fn at(boundary: PublicationBoundary) -> Self {
        let pause = Arc::new(PublicationPause {
            boundary_reached: std::sync::atomic::AtomicBool::new(false),
            release_boundary: std::sync::atomic::AtomicBool::new(false),
            pause_lock: StdMutex::new(()),
            pause_changed: std::sync::Condvar::new(),
        });
        let inner = if boundary == PublicationBoundary::BeforeNotification {
            let notification_pause = Arc::clone(&pause);
            lash_core::facade_support::InMemoryLiveReplayStore::with_before_notification_gate_for_testing(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
                move |events| {
                    if Self::is_authoritative_events(events) {
                        notification_pause.pause();
                    }
                },
            )
        } else {
            lash_core::facade_support::InMemoryLiveReplayStore::default()
        };
        Self {
            inner,
            boundary,
            pause,
        }
    }

    fn is_authoritative_events(events: &[Arc<lash_core::SessionObservationEvent>]) -> bool {
        events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
                    | lash_core::SessionObservationEventPayload::ResidentChanged { .. }
            )
        })
    }

    fn arm_pause(&self) {
        self.pause
            .release_boundary
            .store(false, std::sync::atomic::Ordering::Release);
        self.pause
            .boundary_reached
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn pause(&self) {
        self.pause.pause();
    }

    async fn wait_for_commit_append(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !self
                .pause
                .boundary_reached
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("turn never reached the post-append observation-install seam");
    }

    fn release_commit_install(&self) {
        self.pause
            .release_boundary
            .store(true, std::sync::atomic::Ordering::Release);
        self.pause.pause_changed.notify_all();
    }
}

impl PublicationPause {
    fn pause(&self) {
        self.boundary_reached
            .store(true, std::sync::atomic::Ordering::Release);
        let mut guard = self.pause_lock.lock_recover();
        while !self
            .release_boundary
            .load(std::sync::atomic::Ordering::Acquire)
        {
            guard = self.pause_changed.wait(guard).recover();
        }
    }
}

impl lash_core::LiveReplayStore for PausedCommitReplayStore {
    fn prepare_publication(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        lash_core::PreparedLiveReplayPublication,
        lash_core::LiveReplayStoreError,
    > {
        let authoritative = events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
                    | lash_core::SessionObservationEventPayload::ResidentChanged { .. }
            )
        });
        if authoritative && self.boundary == PublicationBoundary::BeforeReservation {
            self.pause();
        }
        let prepared = self
            .inner
            .prepare_publication(session_id, revision, events)?;
        if authoritative && self.boundary == PublicationBoundary::AfterReservation {
            self.pause();
        }
        Ok(prepared)
    }

    fn publish_prepared(
        &self,
        prepared: lash_core::PreparedLiveReplayPublication,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        let pause = Self::is_authoritative_events(prepared.events());
        if pause && self.boundary == PublicationBoundary::AfterInstall {
            self.pause();
        }
        self.inner.publish_prepared(prepared)
    }

    fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor)
    }

    fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor)
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id)
    }
}

#[tokio::test]
pub(super) async fn recoverable_chat_conformance_deduplicates_redelivery_identity() -> Result<()> {
    let core = standard_core();
    let session = core.session("recoverable-chat-redelivery").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    session
        .turn(TurnInput::text("redelivery identity"))
        .turn_id("recoverable-redelivery-turn")
        .run()
        .await?;

    let mut first_delivery = session.observe().subscribe_recoverable_chat(cursor.clone());
    let first_id = match first_delivery.next().await.expect("first replay event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement { id, .. } => id,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };

    let mut redelivery = session
        .observe()
        .subscribe_recoverable_chat(cursor)
        .with_applied_event_ids([first_id.clone()]);
    let next_id = match redelivery.next().await.expect("next replay event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement { id, .. }
        | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement { id, .. } => id,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { .. } => {
            panic!("fresh cursor unexpectedly gapped")
        }
    };
    assert_ne!(
        next_id, first_id,
        "an already-applied event identity must not be delivered twice"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn gap_replacement_then_continuation_after_unavailable_history() -> Result<()> {
    let session_id = "recoverable-chat-restart-cursor";
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let bootstrap_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory.clone())
            .build(crate::testing::runtime_lease_owner())?;
    bootstrap_core
        .session(session_id)
        .open()
        .await?
        .close()
        .await?;
    drop(bootstrap_core);

    let first_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory.clone())
            .live_replay_store(Arc::new(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
            ))
            .build(crate::testing::runtime_lease_owner())?;
    let first_session = first_core.session(session_id).open().await?;
    let initial_cursor = first_session.observe().recoverable_chat_snapshot().cursor;
    first_session.observe().runtime.record_turn_activity(
        Some(&TurnId::from("before-restart-turn")),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "before replay-store restart".into(),
        }),
    );
    let mut first_stream = first_session
        .observe()
        .subscribe_recoverable_chat(initial_cursor);
    let old_id = match first_stream.next().await.expect("pre-restart event")? {
        crate::recoverable_chat::RecoverableChatUpdate::Event { id, .. } => id,
        other => panic!("expected pre-restart provisional event, got {other:?}"),
    };
    let old_cursor = first_stream.cursor().clone();
    drop(first_stream);
    drop(first_session);
    drop(first_core);

    let second_core =
        explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
            .provider(mock_provider())
            .model(mock_model_spec())
            .store_factory(store_factory)
            .live_replay_store(Arc::new(
                lash_core::facade_support::InMemoryLiveReplayStore::default(),
            ))
            .build(crate::testing::runtime_lease_owner())?;
    let second_session = second_core.session(session_id).open().await?;
    let restarted_at = second_session.observe().recoverable_chat_snapshot().cursor;
    let mut retained_applied_ids = second_session
        .observe()
        .subscribe_recoverable_chat(restarted_at)
        .with_applied_event_ids([old_id.clone()]);
    let mut recovered = second_session
        .observe()
        .subscribe_recoverable_chat(old_cursor)
        .with_applied_event_ids([old_id.clone()]);
    let gap = recovered.next().await.expect("restart gap")?;
    assert!(matches!(
        gap,
        crate::recoverable_chat::RecoverableChatUpdate::ReplayGap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));

    second_session.observe().runtime.record_turn_activity(
        Some(&TurnId::from("after-restart-turn")),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "after replay-store restart".into(),
        }),
    );
    let gap_continuation =
        tokio::time::timeout(std::time::Duration::from_millis(500), recovered.next())
            .await
            .expect("gap stream did not continue with the new event")
            .expect("recovered stream remains open")?;
    let crate::recoverable_chat::RecoverableChatUpdate::Event {
        id: gap_continuation_id,
        event: gap_continuation_event,
    } = gap_continuation
    else {
        panic!("expected post-gap provisional event");
    };
    let update = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        retained_applied_ids.next(),
    )
    .await
    .expect("retained pre-restart identity incorrectly suppressed the new event")
    .expect("recovered stream remains open")?;
    let crate::recoverable_chat::RecoverableChatUpdate::Event { id, event } = update else {
        panic!("expected post-restart provisional event");
    };
    assert_ne!(
        id.cursor, old_id.cursor,
        "a fresh replay-store incarnation must change the opaque cursor even at the same numeric position"
    );
    assert_ne!(
        id, old_id,
        "a fresh replay-store incarnation must distinguish a reused cursor without relying on gap clearing"
    );
    assert_eq!(gap_continuation_id, id);
    assert_eq!(
        observation_assistant_delta(&gap_continuation_event).as_deref(),
        Some("after replay-store restart")
    );
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after replay-store restart")
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn gap_replacement_then_continuation_after_trimmed_history() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("recoverable-chat-gap").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    session
        .turn(TurnInput::text("trim the initial cursor"))
        .run()
        .await?;
    let mut stream = session.observe().subscribe_recoverable_chat(cursor);
    let update = stream.next().await.expect("gap update")?;
    let crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { snapshot, gap } = update else {
        panic!("trimmed cursor must be forwarded as a recoverable gap");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Trimmed);
    assert_eq!(gap.latest_cursor, snapshot.cursor);

    session
        .turn(TurnInput::text("live after gap"))
        .run()
        .await?;
    let read_view = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match stream.next().await.expect("post-gap live update")? {
                crate::recoverable_chat::RecoverableChatUpdate::ReplayGap { snapshot, .. }
                | crate::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                    snapshot,
                    ..
                }
                | crate::recoverable_chat::RecoverableChatUpdate::ResidentReplacement {
                    snapshot,
                    ..
                } => break Ok::<_, crate::EmbedError>(snapshot.read_view),
                crate::recoverable_chat::RecoverableChatUpdate::Event { .. } => {}
            }
        }
    })
    .await
    .expect("post-gap continuation timeout")?;
    assert!(
        read_view
            .messages()
            .iter()
            .any(|message| crate::message_text(message).contains("live after gap")),
        "continued recovery must replace from a snapshot containing the next turn"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn subscriber_lag_with_trimmed_suffix_forces_gap_then_continues() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("subscriber-lag-trimmed-recovery")
        .open()
        .await?;
    let cursor = session.observe().current_observation().cursor;
    let mut stream = session.observe().subscribe_and_recover(cursor);
    assert!(
        futures_util::poll!(stream.next()).is_pending(),
        "the initial poll must wait for a live event"
    );
    assert!(
        stream.live_receiver_installed(),
        "live receiver installation acknowledged"
    );

    for text in ["lag one", "lag two", "lag three"] {
        session.observe().runtime.record_turn_activity(
            Some(&TurnId::from("lagged-turn")),
            TurnActivity::independent(TurnEvent::AssistantProseDelta { text: text.into() }),
        );
    }

    let gap = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("lag recovery timed out")
        .expect("lag recovery stream remains open")?;
    assert!(matches!(
        gap,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Trimmed,
                ..
            },
            ..
        }
    ));

    session.observe().runtime.record_turn_activity(
        Some(&TurnId::from("after-lag-turn")),
        TurnActivity::independent(TurnEvent::AssistantProseDelta {
            text: "after lag".into(),
        }),
    );
    let continued = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("post-lag continuation timed out")
        .expect("post-lag stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = continued else {
        panic!("lag recovery must continue with the next live event");
    };
    assert_eq!(
        observation_assistant_delta(&event).as_deref(),
        Some("after lag")
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn recoverable_chat_conformance_disconnect_does_not_cancel_server_work()
-> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(StdMutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = crate::testing::TestProvider::builder()
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("recoverable-chat-disconnect").open().await?;
    let cursor = session.observe().recoverable_chat_snapshot().cursor;
    let stream = session.observe().subscribe_recoverable_chat(cursor);
    let run_session = session.clone();
    let mut turn = tokio::spawn(async move {
        run_session
            .turn(TurnInput::text("keep running"))
            .turn_id("disconnect-is-not-cancel")
            .run()
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
    assert!(matches!(result.result.outcome, TurnOutcome::Finished(_)));
    Ok(())
}

pub(super) fn observation_assistant_delta(
    event: &lash_core::SessionObservationEvent,
) -> Option<String> {
    match &event.payload {
        lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
            match &activity.event {
                TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn remote_observation_assistant_delta(
    event: &crate::remote::observations::RemoteSessionObservationEvent,
) -> Option<String> {
    match &event.event {
        crate::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
            activity,
        } => match &activity.event {
            crate::remote::usage::RemoteTurnEvent::AssistantProseDelta { text } => {
                Some(text.clone())
            }
            _ => None,
        },
        _ => None,
    }
}
