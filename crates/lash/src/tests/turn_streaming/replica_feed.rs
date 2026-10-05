//! FIG-5090: the session feed's snapshot is the durable head, and a shared
//! live replay store carries every replica's commits. Two cores over one
//! store model two replicas; the backend's engine runs every turn on the
//! publisher, the core built first. Each law takes the live replay stores
//! the replicas run on, so it states the contract for any implementation.

use super::*;
use crate::recoverable_chat::{RecoverableChatSnapshot, RecoverableChatUpdate};
use lash_core::{LiveReplayStore, SessionReadView};

const FEED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

fn in_memory() -> Arc<dyn LiveReplayStore> {
    Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::default())
}

/// The publisher, which runs every turn, and an observer replica over the
/// same session store, each on its live replay store.
async fn replicas(
    publisher_replay: Arc<dyn LiveReplayStore>,
    observer_replay: Arc<dyn LiveReplayStore>,
) -> Result<(LashCore, LashCore)> {
    let backend = double_backend().await;
    let replica = |name: &str, replay: Arc<dyn LiveReplayStore>| {
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .live_replay_store(replay)
            .build(lash_core::LeaseOwnerIdentity::opaque(
                format!("replica-{name}"),
                format!("replica-{name}-boot"),
            ))
    };
    let publisher = replica("publisher", publisher_replay)?;
    let observer = replica("observer", observer_replay)?;
    Ok((publisher, observer))
}

async fn snapshot(session: &crate::LashSession) -> Result<RecoverableChatSnapshot> {
    session.observe().recoverable_chat_snapshot().await
}

fn row_ids(view: &SessionReadView) -> Vec<lash_core::transcript::RowId> {
    view.transcript()
        .expect("committed transcript")
        .into_records()
        .into_iter()
        .map(|row| row.row_id)
        .collect()
}

fn says(view: &SessionReadView, text: &str) -> bool {
    rows_say(
        &view
            .transcript()
            .expect("committed transcript")
            .into_records(),
        text,
    )
}

fn rows_say(rows: &[lash_core::transcript::TranscriptRowRecord], text: &str) -> bool {
    format!("{rows:?}").contains(text)
}

/// The next terminal replacement on `feed`, past provisional events: the
/// rows its commit added.
async fn next_commit(
    feed: &mut crate::recoverable_chat::RecoverableChatSubscription,
) -> Result<Vec<lash_core::transcript::TranscriptRowRecord>> {
    loop {
        let update = tokio::time::timeout(FEED_DEADLINE, feed.next())
            .await
            .expect("the feed delivers the commit")
            .expect("the feed stays open")?;
        match update {
            RecoverableChatUpdate::TerminalReplacement { event, .. } => {
                let lash_core::SessionObservationEventPayload::Committed { rows, .. } =
                    &event.payload
                else {
                    panic!("a terminal replacement carries a commit");
                };
                return Ok(rows.clone());
            }
            RecoverableChatUpdate::ReplayGap { gap, .. } => {
                panic!("a cursor this feed minted gapped: {gap:?}")
            }
            RecoverableChatUpdate::Event { .. }
            | RecoverableChatUpdate::ResidentReplacement { .. } => {}
        }
    }
}

#[tokio::test]
async fn a_snapshot_is_the_durable_head_after_another_replica_commits() -> Result<()> {
    let (publisher, observer) = replicas(in_memory(), in_memory()).await?;
    let session_id = SessionId::from("replica-feed-snapshot");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id).open().await?;

    published
        .send(TurnInput::text("committed on the publisher"))
        .output()
        .await?;

    let at = snapshot(&observed).await?;
    let head = published.durable().read().await?.expect("durable head");
    assert!(
        says(&at.read_view, "echo: committed on the publisher"),
        "the observer's snapshot is the durable head, not its trailing resident"
    );
    assert_eq!(row_ids(&at.read_view), row_ids(&head));
    Ok(())
}

/// A cursor another replay store minted answers a typed gap whose
/// replacement is the durable head.
async fn a_replay_gap_rebuilds_from_the_durable_head(
    make: impl Fn() -> Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(make(), make()).await?;
    let session_id = SessionId::from("replica-feed-gap");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id.clone()).open().await?;

    published
        .send(TurnInput::text("before the observer reconnects"))
        .output()
        .await?;
    let elsewhere = snapshot(&published).await?.cursor;
    let mut feed = observed.observe().subscribe_recoverable_chat(elsewhere);

    let update = tokio::time::timeout(FEED_DEADLINE, feed.next())
        .await
        .expect("the feed answers the foreign cursor")
        .expect("the feed stays open")?;
    let RecoverableChatUpdate::ReplayGap { snapshot, gap } = update else {
        panic!("another publisher's cursor answers a typed gap, got {update:?}");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert!(
        says(&snapshot.read_view, "echo: before the observer reconnects"),
        "the gap's replacement is the durable head, not the observer's resident"
    );
    let head = published.durable().read().await?.expect("durable head");
    assert_eq!(row_ids(&snapshot.read_view), row_ids(&head));
    Ok(())
}

#[tokio::test]
async fn a_replay_gap_rebuilds_from_the_durable_head_in_memory() -> Result<()> {
    a_replay_gap_rebuilds_from_the_durable_head(in_memory).await
}

/// A commit one replica makes reaches a feed another replica opened,
/// through the live replay store they share.
async fn a_commit_reaches_another_replicas_feed_through_a_shared_store(
    shared: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(Arc::clone(&shared), shared).await?;
    let session_id = SessionId::from("replica-feed-commit");
    let observed = observer
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let opened_at = snapshot(&observed).await?;
    let mut feed = observed
        .observe()
        .subscribe_recoverable_chat(opened_at.cursor);

    publisher
        .session(session_id)
        .durable()
        .await?
        .send(TurnInput::text("made on the publisher"))
        .output()
        .await?;

    let committed = next_commit(&mut feed).await?;
    assert!(
        rows_say(&committed, "echo: made on the publisher"),
        "the observer's feed delivers the publisher's commit"
    );
    Ok(())
}

#[tokio::test]
async fn a_commit_reaches_another_replicas_feed_through_a_shared_in_memory_store() -> Result<()> {
    a_commit_reaches_another_replicas_feed_through_a_shared_store(in_memory()).await
}

/// Every replica's feed, from its snapshot, holds each committed row once,
/// in order, across several commits.
async fn a_feed_snapshot_and_its_tail_hold_every_committed_row_once(
    shared: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(Arc::clone(&shared), shared).await?;
    let session_id = SessionId::from("replica-feed-rows");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id.clone()).open().await?;
    let mut feeds = Vec::new();
    for session in [&published, &observed] {
        let at = snapshot(session).await?;
        let feed = session.observe().subscribe_recoverable_chat(at.cursor);
        feeds.push((row_ids(&at.read_view), feed));
    }

    for text in ["first", "second", "third"] {
        published.send(TurnInput::text(text)).output().await?;
    }
    let head = published
        .durable()
        .read()
        .await?
        .expect("the session has a durable head");
    let head_rows = row_ids(&head);

    for (mut seen, mut feed) in feeds {
        while seen.len() < head_rows.len() {
            let rows = next_commit(&mut feed).await?;
            seen.extend(rows.into_iter().map(|row| row.row_id));
        }
        assert_eq!(
            seen, head_rows,
            "a snapshot and its commits hold each committed row once, in order"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_feed_snapshot_and_its_tail_hold_every_committed_row_once_in_memory() -> Result<()> {
    a_feed_snapshot_and_its_tail_hold_every_committed_row_once(in_memory()).await
}

/// An observer whose feed advanced past a commit its own handle never
/// adopted reconnects from that cursor without a gap: the cursor is judged
/// against the durable head, not the observing handle's revision.
async fn an_observer_that_lags_across_a_commit_continues_without_a_gap(
    shared: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(Arc::clone(&shared), shared).await?;
    let session_id = SessionId::from("replica-feed-lag");
    let observed = observer
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let published = publisher.session(session_id).durable().await?;
    let opened_at = snapshot(&observed).await?;
    let mut feed = observed
        .observe()
        .subscribe_recoverable_chat(opened_at.cursor);
    published
        .send(TurnInput::text("committed while the observer streams"))
        .output()
        .await?;
    next_commit(&mut feed).await?;
    let advanced = feed.cursor().clone();
    drop(feed);

    let mut reconnected = observed.observe().subscribe_recoverable_chat(advanced);
    published
        .send(TurnInput::text("committed after the reconnect"))
        .output()
        .await?;
    let committed = next_commit(&mut reconnected).await?;
    assert!(
        rows_say(&committed, "echo: committed after the reconnect"),
        "the reconnected feed continues with the next commit"
    );
    Ok(())
}

#[tokio::test]
async fn an_observer_that_lags_across_a_commit_continues_without_a_gap_in_memory() -> Result<()> {
    an_observer_that_lags_across_a_commit_continues_without_a_gap(in_memory()).await
}

/// A commit carries its rows delta, not the session's read view (FIG-5100):
/// a feed whose consumer holds a revision the delta does not extend
/// rebuilds from the durable head instead of delivering rows that would
/// leave out the commit the consumer never received.
///
/// The observer's store receives the publisher's second commit but never
/// its first, as a shared store does when one replica's publication fails.
async fn a_commit_that_extends_a_revision_the_consumer_lacks_resyncs_from_the_head(
    publisher_replay: Arc<dyn LiveReplayStore>,
    observer_replay: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) =
        replicas(Arc::clone(&publisher_replay), Arc::clone(&observer_replay)).await?;
    let session_id = SessionId::from("replica-feed-diverged");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id.clone()).open().await?;
    let published_at = snapshot(&published).await?.cursor;
    let held = snapshot(&observed).await?;
    let mut feed = observed.observe().subscribe_recoverable_chat(held.cursor);

    for text in ["first", "second"] {
        published.send(TurnInput::text(text)).output().await?;
    }
    let lash_core::LiveReplayOutcome::Replayed(events) = publisher_replay
        .replay_after_cursor(&published_at)
        .map_err(crate::observation_feed::live_replay_error)?
    else {
        panic!("the publisher's replay holds both commits");
    };
    let second = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.payload,
                lash_core::SessionObservationEventPayload::Committed { .. }
            )
        })
        .expect("the second commit was published");
    let lash_core::SessionObservationEventPayload::Committed { base_revision, .. } =
        &second.payload
    else {
        unreachable!("the search found a commit");
    };
    assert_ne!(
        *base_revision,
        lash_core::SessionRevision::new(0),
        "the second commit extends the first, which the consumer lacks"
    );
    let prepared = observer_replay
        .prepare_publication(
            &session_id,
            second.revision(),
            vec![lash_core::LiveReplayEventDraft::new(
                second.turn_id.clone(),
                second.payload.clone(),
            )],
        )
        .map_err(crate::observation_feed::live_replay_error)?;
    observer_replay
        .publish_prepared(prepared)
        .map_err(crate::observation_feed::live_replay_error)?;

    let update = tokio::time::timeout(FEED_DEADLINE, feed.next())
        .await
        .expect("the feed answers the diverged commit")
        .expect("the feed stays open")?;
    let RecoverableChatUpdate::ReplayGap { snapshot, gap } = update else {
        panic!("a commit extending a revision the consumer lacks resyncs, got {update:?}");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, second.revision());
    assert!(
        says(&snapshot.read_view, "echo: first") && says(&snapshot.read_view, "echo: second"),
        "the resync is the durable head, holding the commit the delta left out"
    );
    let head = published.durable().read().await?.expect("durable head");
    assert_eq!(row_ids(&snapshot.read_view), row_ids(&head));
    Ok(())
}

#[tokio::test]
async fn a_commit_that_extends_a_revision_the_consumer_lacks_resyncs_from_the_head_in_memory()
-> Result<()> {
    a_commit_that_extends_a_revision_the_consumer_lacks_resyncs_from_the_head(
        in_memory(),
        in_memory(),
    )
    .await
}
