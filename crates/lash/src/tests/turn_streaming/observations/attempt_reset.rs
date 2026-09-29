#[tokio::test]
pub(super) async fn remote_reset_and_transcript_projection_agree() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
    ))
    .charge_safety(lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
        max_unsafe_retries: 2,
        max_duplicate_cost_tokens: None,
    })
    .provider(retrying_visible_stream_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("retry-visible-observation")
        .created()
        .await
        .open()
        .await?;
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
        .send(TurnInput::text("retry twice after visible output"))
        .output()
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
    use lash_remote_protocol::negotiation::{Negotiated, Negotiation, REMOTE_PROTOCOL};
    use lash_remote_protocol::{RemoteTurnActivity, RemoteTurnEvent};
    let negotiated = Negotiated::from_accept(
        REMOTE_PROTOCOL,
        &Negotiation::Accept {
            supported: REMOTE_PROTOCOL,
            selected: REMOTE_PROTOCOL.max(),
        },
    )
    .expect("negotiate the current protocol");
    let mut seen = std::collections::BTreeSet::new();
    let mut prose = std::collections::BTreeMap::<String, String>::new();
    let mut reasoning = std::collections::BTreeMap::<String, String>::new();
    prose.insert("unrelated".into(), "keep unrelated text".into());
    let mut reset_targets = Vec::new();
    for (sequence, event) in replay_events.iter().enumerate() {
        let lash_core::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            continue;
        };
        let remote = RemoteTurnActivity::from_core(sequence as u64, activity.clone())
            .expect("convert runtime activity");
        let bytes = remote
            .encode_json(&negotiated)
            .expect("encode remote activity");
        let remote = RemoteTurnActivity::decode_json(&bytes).expect("decode negotiated activity");
        let mut wrong = serde_json::from_slice::<serde_json::Value>(&bytes).expect("wire JSON");
        wrong["protocol_version"] =
            serde_json::json!(lash_remote_protocol::REMOTE_PROTOCOL_VERSION + 1);
        assert!(matches!(
            RemoteTurnActivity::decode_json(
                &serde_json::to_vec(&wrong).expect("wrong-version JSON")
            ),
            Err(lash_remote_protocol::RemoteProtocolError::Unsupported { .. })
        ));
        // Re-delivery of the same activity must not duplicate transcript text.
        for delivered in [&remote, &remote] {
            if !seen.insert(delivered.id.clone()) {
                continue;
            }
            match &delivered.event {
                RemoteTurnEvent::AssistantProseDelta { text, .. } => prose
                    .entry(delivered.correlation_id.clone())
                    .or_default()
                    .push_str(text),
                RemoteTurnEvent::ReasoningDelta { text, .. } => reasoning
                    .entry(delivered.correlation_id.clone())
                    .or_default()
                    .push_str(text),
                RemoteTurnEvent::ModelAttemptReset {
                    assistant_prose_correlation_ids,
                    reasoning_correlation_ids,
                } => {
                    reset_targets.push((
                        assistant_prose_correlation_ids.clone(),
                        reasoning_correlation_ids.clone(),
                    ));
                    for id in assistant_prose_correlation_ids {
                        prose.remove(id);
                    }
                    for id in reasoning_correlation_ids {
                        reasoning.remove(id);
                    }
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        reset_targets,
        vec![
            (vec!["text:1".to_string()], vec!["reasoning:1".to_string()]),
            (vec!["text:2".to_string()], vec!["reasoning:2".to_string()]),
        ]
    );
    assert_eq!(
        prose.remove("unrelated").as_deref(),
        Some("keep unrelated text")
    );
    assert_eq!(prose.values().cloned().collect::<String>(), "prose-3");
    assert_eq!(
        reasoning.values().cloned().collect::<String>(),
        "reasoning-3"
    );
    let assert_transcript = |records: &[lash_core::SessionHistoryRecord]| {
        let copies = records.iter().filter(|record| matches!(record,
            lash_core::SessionHistoryRecord::Conversation(message) if message.parts.iter().any(|part| part.content().contains("prose-3"))
        )).count();
        assert_eq!(
            copies, 1,
            "the accepted attempt commits one transcript message"
        );
        assert!(records.iter().all(|record| !matches!(record,
            lash_core::SessionHistoryRecord::Conversation(message) if message.parts.iter().any(|part| part.content().contains("prose-1") || part.content().contains("prose-2"))
        )));
    };
    assert_transcript(session.read_view().active_events());
    Box::pin(session.close()).await?;
    let reopened = core.session("retry-visible-observation").open().await?;
    assert_transcript(reopened.read_view().active_events());

    Ok(())
}
