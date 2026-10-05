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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double_backend().await))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
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
    let before = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot");
    let mut stream = session
        .observe()
        .subscribe_and_recover(before.cursor.clone());
    assert!(
        stream.next().now_or_never().is_none(),
        "subscription is active before invalidation"
    );
    replay
        .invalidate_session(&session_id)
        .await
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

#[tokio::test]
async fn a_disconnected_host_reconciles_a_failed_turn_after_live_replay_trims() -> Result<()> {
    use lash_core::LiveReplayStore as _;
    use lash_core::store::{
        TurnChangeCursor, TurnChangeKind, TurnCommitFailureCause, TurnCommitOutcome,
    };
    let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
        lash_core::facade_support::InMemoryLiveReplayStoreConfig {
            max_events_per_session: 1,
            ..Default::default()
        },
    ));
    let provider = crate::testing::TestProvider::builder()
        .kind("unattended-failure")
        .complete(|_| async {
            Err(LlmTransportError::new("unattended failure").with_output_started(true))
        })
        .build()
        .into_handle();
    let double = restate_double(0x3095).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .live_replay_store(replay.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("disconnected-turn-feed").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let live_cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let result = session
        .send(TurnInput::text("fail unattended"))
        .output()
        .await?;
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::ProviderError)
    ));
    assert!(matches!(
        replay
            .replay_after_cursor(&live_cursor)
            .await
            .expect("replay read"),
        lash_core::LiveReplayOutcome::Gap(lash_core::LiveReplayGapReason::Trimmed)
    ));
    let page = core
        .turns_changed_since(TurnChangeCursor::initial(), std::num::NonZeroUsize::MAX)
        .await?;
    assert_eq!(page.changes.len(), 1);
    assert_eq!(
        page.changes[0].session_id.as_str(),
        "disconnected-turn-feed"
    );
    assert!(matches!(
        page.changes[0].kind,
        TurnChangeKind::Committed {
            outcome: TurnCommitOutcome::Failed(TurnCommitFailureCause::ProviderError),
            ..
        }
    ));
    assert!(
        core.turns_changed_since(page.next, std::num::NonZeroUsize::MAX)
            .await?
            .changes
            .is_empty()
    );
    Ok(())
}
