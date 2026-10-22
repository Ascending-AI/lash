use super::*;
use crate::TurnEvent;
use lash_vocabulary::identity::TurnId;

use futures_util::StreamExt as _;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use crate::tests::host::observations::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gap_replacement_then_continuation_after_unavailable_history() -> Result<()> {
    let session_id = "recoverable-chat-restart-cursor";
    let backend = sqlite_memory_store_backend().await;
    let bootstrap_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::tests::host::runtime_lease_owner())?;
    Box::pin(
        created_session(
            &bootstrap_core,
            lash_vocabulary::identity::SessionId::parse(session_id)
                .expect("nonblank host identity"),
        )
        .await
        .open()
        .await?
        .close(),
    )
    .await?;
    drop(bootstrap_core);

    let first_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
        ))
        .build(crate::tests::host::runtime_lease_owner())?;
    let first_session = created_session(
        &first_core,
        lash_vocabulary::identity::SessionId::parse(session_id).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let initial_cursor = first_session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    first_session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("before-restart-turn")),
            TurnActivity::independent(TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text: "before replay-store restart".into(),
                block: bid(),
            })),
        )
        .await;
    let mut first_stream = first_session
        .observe()
        .subscribe_and_recover(initial_cursor);
    let old_id = match first_stream.next().await.expect("pre-restart event")? {
        crate::observe::SessionObservationStreamItem::Event(event) => {
            crate::observe::SessionObservationEventId::of(&event)
        }
        other => panic!("expected pre-restart provisional event, got {other:?}"),
    };
    let old_cursor = first_stream.cursor().clone();
    drop(first_stream);
    drop(first_session);
    drop(first_core);

    let second_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
        ))
        .build(crate::tests::host::runtime_lease_owner())?;
    let second_session = created_session(
        &second_core,
        lash_vocabulary::identity::SessionId::parse(session_id).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let restarted_at = second_session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let mut retained_applied_ids = second_session
        .observe()
        .subscribe_and_recover(restarted_at)
        .with_applied_event_ids([old_id.clone()]);
    let mut recovered = second_session
        .observe()
        .subscribe_and_recover(old_cursor)
        .with_applied_event_ids([old_id.clone()]);
    let gap = recovered.next().await.expect("restart gap")?;
    assert!(matches!(
        gap,
        crate::observe::SessionObservationStreamItem::Gap {
            gap: lash_core::facade_support::LiveReplayGap {
                reason: lash_core::LiveReplayGapReason::Unavailable,
                ..
            },
            ..
        }
    ));

    second_session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("after-restart-turn")),
            TurnActivity::independent(TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text: "after replay-store restart".into(),
                block: bid(),
            })),
        )
        .await;
    let gap_continuation =
        tokio::time::timeout(std::time::Duration::from_millis(500), recovered.next())
            .await
            .expect("gap stream did not continue with the new event")
            .expect("recovered stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(gap_continuation_event) =
        gap_continuation
    else {
        panic!("expected post-gap provisional event");
    };
    let gap_continuation_id =
        crate::observe::SessionObservationEventId::of(&gap_continuation_event);
    let update = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        retained_applied_ids.next(),
    )
    .await
    .expect("retained pre-restart identity incorrectly suppressed the new event")
    .expect("recovered stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = update else {
        panic!("expected post-restart provisional event");
    };
    let id = crate::observe::SessionObservationEventId::of(&event);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_lag_with_trimmed_suffix_forces_gap_then_continues() -> Result<()> {
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
    .build(crate::tests::host::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse("subscriber-lag-trimmed-recovery")
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
    let mut stream = session.observe().subscribe_and_recover(cursor);
    assert!(
        futures_util::poll!(stream.next()).is_pending(),
        "the initial poll must wait for a live event"
    );

    for text in ["lag one", "lag two", "lag three"] {
        session
            .observe()
            .runtime
            .record_turn_activity(
                Some(&TurnId::from("lagged-turn")),
                TurnActivity::independent(TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    text: text.into(),
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                })),
            )
            .await;
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
    let retained = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("the retained suffix replays")
        .expect("lag recovery stream remains open")?;
    let crate::observe::SessionObservationStreamItem::Event(retained) = retained else {
        panic!("the gap's cursor must keep the retained activity past the head");
    };
    assert_eq!(
        observation_assistant_delta(&retained).as_deref(),
        Some("lag three")
    );

    session
        .observe()
        .runtime
        .record_turn_activity(
            Some(&TurnId::from("after-lag-turn")),
            TurnActivity::independent(TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                text: "after lag".into(),
                block: bid(),
            })),
        )
        .await;
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
