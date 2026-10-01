use super::*;

pub(in crate::tests::turn_streaming) fn observation_assistant_delta(
    event: &lash_core::SessionObservationEvent,
) -> Option<String> {
    match &event.payload {
        lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
            match &activity.event {
                TurnEvent::AssistantProseDelta { text, .. } => Some(text.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

#[tokio::test]
async fn invalidated_live_observation_recovers_with_an_authoritative_snapshot() -> Result<()> {
    use futures_util::FutureExt as _;
    use lash_core::LiveReplayStore as _;
    let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .live_replay_store(replay.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("invalidated-live-observation");
    let session = core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("durable before the gap"))
        .output()
        .await?;
    let before = session.observe().current_observation();
    let mut stream = session
        .observe()
        .subscribe_and_recover(before.cursor.clone());
    assert!(
        stream.next().now_or_never().is_none(),
        "subscription is active before invalidation"
    );
    replay
        .invalidate_session(&session_id)
        .expect("invalidate replay continuity");
    let recovered = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("active stream recovers promptly")
        .expect("active stream stays open")?;
    let crate::observe::SessionObservationStreamItem::Gap { observation, gap } = recovered else {
        panic!("invalidation must recover with a typed gap and a snapshot");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_cursor, observation.cursor);
    let recovered_revision = observation
        .cursor
        .parse_for_session(&session_id)
        .expect("recovered cursor parses")
        .revision;
    let previous_revision = before
        .cursor
        .parse_for_session(&session_id)
        .expect("previous cursor parses")
        .revision;
    assert_eq!(recovered_revision, previous_revision);
    assert_ne!(observation.cursor, before.cursor);
    assert!(recovered_revision > lash_core::SessionRevision::new(0));
    session
        .send(TurnInput::text("live after resync"))
        .output()
        .await?;
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("resynced stream receives live events")
            .expect("resynced stream stays open")?;
        if let crate::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: live after resync")
        {
            break;
        }
    }
    Ok(())
}
