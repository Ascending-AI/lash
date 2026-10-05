//! FIG-5090: the session feed's snapshot is the durable head, and a shared
//! live replay store carries every replica's commits. Two cores over one
//! store model two replicas; the backend's engine runs every turn on the
//! publisher, the core built first. Each law takes the live replay stores
//! the replicas run on, so it states the contract for any implementation.

use super::*;
use crate::recoverable_chat::{RecoverableChatSnapshot, RecoverableChatUpdate};
use lash_core::{LiveReplayStore, LiveReplayStoreError, SessionReadView};

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
        .await
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
    observer_replay
        .publish(
            &session_id,
            second.revision(),
            vec![lash_core::LiveReplayEventDraft::new(
                second.turn_id.clone(),
                second.payload.clone(),
            )],
        )
        .await
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

/// Events each replica publishes in
/// [`concurrent_writers_on_two_replicas_share_one_gap_free_order`].
const RACED_EVENTS: usize = 64;

/// Two replicas write one session at once through the store they share,
/// as the run's runtime and the replica that accepted a send do: the
/// store sequences them into one contiguous order, each replica's events
/// keep their own order, and a feed carries every event once (FIG-5099).
async fn concurrent_writers_on_two_replicas_share_one_gap_free_order(
    shared: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(Arc::clone(&shared), shared).await?;
    let session_id = SessionId::from("replica-feed-concurrent-writers");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id.clone()).open().await?;
    let opened_at = observed.observe().snapshot().await?;
    let mut feed = observed.observe().subscribe_and_recover(opened_at.cursor);

    let activity = |text: String| {
        lash_core::TurnActivity::independent(lash_core::TurnEvent::AssistantProseDelta {
            text: text.into(),
            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
        })
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let activity_writer = {
        let runtime = published.runtime.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            for index in 0..RACED_EVENTS {
                runtime
                    .record_turn_activity(None, activity(format!("activity {index}")))
                    .await;
                if index % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
    };
    let queue_writer = {
        let runtime = observed.runtime.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            barrier.wait().await;
            for index in 0..RACED_EVENTS {
                runtime
                    .record_queue_changed(
                        lash_core::SessionQueueEventKind::Enqueued,
                        vec![format!("batch {index}")],
                    )
                    .await;
                if index % 8 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
    };
    activity_writer.await.expect("join the activity writer");
    queue_writer.await.expect("join the queue writer");

    let mut positions = Vec::new();
    let mut activities = Vec::new();
    let mut batches = Vec::new();
    while positions.len() < 2 * RACED_EVENTS {
        let item = tokio::time::timeout(FEED_DEADLINE, feed.next())
            .await
            .expect("the feed delivers every raced event")
            .expect("the feed stays open")?;
        let crate::observe::SessionObservationStreamItem::Event(event) = item else {
            panic!("a feed over one shared sequence never gaps: {item:?}");
        };
        positions.push(
            event
                .cursor
                .parse_for_session(&session_id)
                .expect("a feed event names its session")
                .live_position,
        );
        match &event.payload {
            lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
                let lash_core::TurnEvent::AssistantProseDelta { text, .. } = &activity.event else {
                    panic!("only raced deltas are published");
                };
                activities.push(text.to_string());
            }
            lash_core::SessionObservationEventPayload::QueueChanged { batch_ids, .. } => {
                batches.extend(batch_ids.iter().cloned());
            }
            payload => panic!("only raced events are published, got {payload:?}"),
        }
    }
    assert!(
        positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "two replicas' writes share one contiguous position sequence: {positions:?}"
    );
    assert_eq!(
        activities,
        (0..RACED_EVENTS)
            .map(|index| format!("activity {index}"))
            .collect::<Vec<_>>(),
        "the publisher's events keep its order"
    );
    assert_eq!(
        batches,
        (0..RACED_EVENTS)
            .map(|index| format!("batch {index}"))
            .collect::<Vec<_>>(),
        "the observer replica's events keep its order"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_on_two_replicas_share_one_gap_free_order_in_memory() -> Result<()> {
    concurrent_writers_on_two_replicas_share_one_gap_free_order(in_memory()).await
}

/// A live replay store that decides a subscription only once the test lets
/// it: a remote store answering its gap after a round trip.
struct DeferredSubscribeStore {
    inner: Arc<dyn LiveReplayStore>,
    decide: tokio::sync::Semaphore,
    asked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl LiveReplayStore for DeferredSubscribeStore {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<Vec<Arc<lash_core::SessionObservationEvent>>, LiveReplayStoreError>
    {
        self.inner.publish(session_id, revision, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        self.asked.notify_one();
        self.decide
            .acquire()
            .await
            .expect("the decision gate stays open")
            .forget();
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

/// A gap the store decides asynchronously reaches the feed as a typed gap
/// whose replacement is the durable head, and the feed goes on from it
/// (FIG-5099).
#[tokio::test]
async fn a_gap_the_store_decides_asynchronously_reaches_the_feed() -> Result<()> {
    let deferred = Arc::new(DeferredSubscribeStore {
        inner: in_memory(),
        decide: tokio::sync::Semaphore::new(0),
        asked: tokio::sync::Notify::new(),
    });
    let (publisher, observer) = replicas(in_memory(), deferred.clone()).await?;
    let session_id = SessionId::from("replica-feed-deferred-gap");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id).open().await?;
    published
        .send(TurnInput::text("before the deferred decision"))
        .output()
        .await?;
    let elsewhere = snapshot(&published).await?.cursor;
    let mut feed = observed.observe().subscribe_and_recover(elsewhere);

    let mut next = feed.next();
    tokio::select! {
        biased;
        item = &mut next => panic!("the feed answered before the store decided: {item:?}"),
        () = deferred.asked.notified() => {}
    }
    deferred.decide.add_permits(1);
    let item = tokio::time::timeout(FEED_DEADLINE, next)
        .await
        .expect("the feed answers once the store decides")
        .expect("the feed stays open")?;
    let crate::observe::SessionObservationStreamItem::Gap { observation, gap } = item else {
        panic!("another store's cursor answers a typed gap, got {item:?}");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert!(
        says(&observation.read_view, "echo: before the deferred decision"),
        "the deferred gap's replacement is the durable head"
    );

    deferred.decide.add_permits(1);
    observed
        .runtime
        .record_queue_changed(
            lash_core::SessionQueueEventKind::Enqueued,
            vec!["after the deferred gap".to_string()],
        )
        .await;
    let item = tokio::time::timeout(FEED_DEADLINE, feed.next())
        .await
        .expect("the feed goes on from the gap's cursor")
        .expect("the feed stays open")?;
    let crate::observe::SessionObservationStreamItem::Event(event) = item else {
        panic!("the feed continues after its gap, got {item:?}");
    };
    assert!(matches!(
        &event.payload,
        lash_core::SessionObservationEventPayload::QueueChanged { batch_ids, .. }
            if batch_ids == &["after the deferred gap"]
    ));
    Ok(())
}

/// The raw cursor reads judge a cursor against the durable head, as the
/// feed does: an observer whose resident trails another replica's commit
/// continues a cursor from after it, a cursor past the head gaps to the
/// head, and a malformed or foreign-session cursor is refused (FIG-5099).
async fn raw_cursor_reads_judge_against_the_durable_head(
    shared: Arc<dyn LiveReplayStore>,
) -> Result<()> {
    let (publisher, observer) = replicas(Arc::clone(&shared), shared).await?;
    let session_id = SessionId::from("replica-feed-raw-reads");
    let published = publisher
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let observed = observer.session(session_id.clone()).open().await?;
    published
        .send(TurnInput::text("committed on the publisher"))
        .output()
        .await?;
    let after_commit = published.observe().snapshot().await?.cursor;
    let head = after_commit
        .parse_for_session(&session_id)
        .expect("a snapshot cursor names its session");

    assert!(
        matches!(
            observed.observe().resume_from_cursor(&after_commit).await?,
            crate::observe::SessionResume::Replayed { .. }
        ),
        "a cursor at the durable head replays, however far the observer's resident trails"
    );
    assert!(matches!(
        observed
            .observe()
            .subscribe_from_cursor(&after_commit)
            .await?,
        crate::observe::SessionObservationSubscription::Subscribed(_)
    ));

    let ahead = lash_core::SessionCursor::new(
        head.replay_incarnation_id,
        &session_id,
        lash_core::SessionRevision::new(head.revision.as_u64() + 5),
        head.live_position,
    );
    let crate::observe::SessionResume::Gap { observation, gap } =
        observed.observe().resume_from_cursor(&ahead).await?
    else {
        panic!("a cursor past the durable head gaps");
    };
    assert_eq!(gap.reason, lash_core::LiveReplayGapReason::Unavailable);
    assert_eq!(gap.latest_revision, head.revision);
    assert!(says(
        &observation.read_view,
        "echo: committed on the publisher"
    ));
    assert!(matches!(
        observed.observe().subscribe_from_cursor(&ahead).await?,
        crate::observe::SessionObservationSubscription::Gap { .. }
    ));

    let malformed: lash_core::SessionCursor =
        serde_json::from_value(serde_json::json!("not-a-session-cursor"))
            .expect("a cursor deserializes without validation");
    let foreign = lash_core::SessionCursor::new(
        head.replay_incarnation_id,
        "another-session",
        head.revision,
        head.live_position,
    );
    for refused in [&malformed, &foreign] {
        assert!(
            observed
                .observe()
                .resume_from_cursor(refused)
                .await
                .is_err()
        );
        assert!(
            observed
                .observe()
                .subscribe_from_cursor(refused)
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn raw_cursor_reads_judge_against_the_durable_head_in_memory() -> Result<()> {
    raw_cursor_reads_judge_against_the_durable_head(in_memory()).await
}
