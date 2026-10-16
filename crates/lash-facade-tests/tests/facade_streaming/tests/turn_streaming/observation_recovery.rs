use super::*;

/// A one-session publisher with independent transport and replay windows.
/// A real broadcast overflow loses notifications while the production replay
/// store still retains every event. Publication and subscription share a lock
/// so the replay/live boundary cannot lose a racing publication.
struct LaggingReplay {
    retained: lash_core::facade_support::InMemoryLiveReplayStore,
    live: tokio::sync::broadcast::Sender<Arc<lash_core::SessionObservationEvent>>,
    boundary: tokio::sync::Mutex<()>,
    subscribed: StdMutex<Vec<lash_core::SessionCursor>>,
    lagged: Arc<std::sync::atomic::AtomicU64>,
}

impl LaggingReplay {
    fn new() -> Self {
        Self {
            retained: lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
            live: tokio::sync::broadcast::channel(1).0,
            boundary: tokio::sync::Mutex::new(()),
            subscribed: StdMutex::new(Vec::new()),
            lagged: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for LaggingReplay {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        let _boundary = self.boundary.lock().await;
        let events = self.retained.publish(session_id, revision, events).await?;
        for event in &events {
            let _ = self.live.send(Arc::clone(event));
        }
        Ok(events)
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.retained.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        let _boundary = self.boundary.lock().await;
        let replay = match self.retained.replay_after_cursor(cursor).await? {
            lash_core::LiveReplayOutcome::Replayed(events) => events,
            lash_core::LiveReplayOutcome::Gap(reason) => {
                return Ok(lash_core::LiveReplaySubscribeOutcome::Gap(reason));
            }
        };
        self.subscribed.lock_recover().push(cursor.clone());
        let live = futures_util::stream::unfold(
            (self.live.subscribe(), Arc::clone(&self.lagged), false),
            |(mut receiver, lagged, ended)| async move {
                if ended {
                    return None;
                }
                let item = match receiver.recv().await {
                    Ok(event) => Ok(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        lagged.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                        Err(lash_core::LiveReplayStoreError::SubscriberLagged(count))
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        Err(lash_core::LiveReplayStoreError::Closed)
                    }
                };
                let ended = item.is_err();
                Some((item, (receiver, lagged, ended)))
            },
        );
        Ok(lash_core::LiveReplaySubscribeOutcome::Subscribed(
            lash_core::LiveReplaySubscription::new(replay, live),
        ))
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.retained.current_cursor(session_id, revision)
    }

    fn earliest_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.retained.earliest_cursor(session_id, revision)
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.retained.invalidate_session(session_id).await
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.retained.invalidate_all().await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.retained.trim_session(session_id).await
    }
}

/// ADR 0002: transport lag from a retained cursor replays the entire missing
/// suffix in order, without a gap or duplication, then follows the live tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_lag_from_retained_cursor_replays_every_event_then_continues() -> Result<()> {
    use futures_util::StreamExt as _;
    use lash_core::LiveReplayStore as _;

    let replay = Arc::new(LaggingReplay::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id = SessionId::from("retained-subscriber-lag");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let cursor = session.observe().snapshot().await?.cursor;
    let revision = cursor.parse().expect("snapshot cursor").revision;
    let mut feed = session.observe().subscribe_and_recover(cursor.clone());
    assert!(futures_util::poll!(feed.next()).is_pending());

    let draft = || {
        lash_core::LiveReplayEventDraft::new(
            None::<TurnId>,
            lash_core::SessionObservationEventPayload::ResidentChanged,
        )
    };
    let first = replay
        .publish(&session_id, revision, vec![draft()])
        .await
        .expect("publish the delivered event");
    let next_cursor = |item| match item {
        lash::observe::SessionObservationStreamItem::Event(event) => event.cursor.clone(),
        lash::observe::SessionObservationStreamItem::Gap { .. } => {
            panic!("a retained cursor must recover without replacing the observation");
        }
    };
    assert_eq!(
        next_cursor(feed.next().await.expect("first event")?),
        first[0].cursor
    );

    // The subscriber stops polling while three notifications enter its
    // one-slot channel. Replay retention is untouched by that overflow.
    let suffix = replay
        .publish(&session_id, revision, vec![draft(), draft(), draft()])
        .await
        .expect("publish the lagging suffix");
    let lash_core::LiveReplayOutcome::Replayed(retained) = replay
        .replay_after_cursor(&first[0].cursor)
        .await
        .expect("read the retained suffix")
    else {
        panic!("the delivered cursor must remain replayable during transport lag");
    };
    assert_eq!(
        retained
            .iter()
            .map(|event| &event.cursor)
            .collect::<Vec<_>>(),
        suffix.iter().map(|event| &event.cursor).collect::<Vec<_>>()
    );
    for expected in &suffix {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), feed.next())
            .await
            .expect("lag recovery completes")
            .expect("the lagged feed stays open")?;
        assert_eq!(next_cursor(item), expected.cursor);
    }
    assert_eq!(replay.lagged.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        *replay.subscribed.lock_recover(),
        vec![cursor, first[0].cursor.clone()],
        "recovery resubscribes from the last delivered cursor"
    );

    let tail = replay
        .publish(&session_id, revision, vec![draft()])
        .await
        .expect("publish live continuation");
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), feed.next())
        .await
        .expect("live continuation arrives")
        .expect("the recovered feed stays open")?;
    assert_eq!(next_cursor(item), tail[0].cursor);
    assert!(
        futures_util::poll!(feed.next()).is_pending(),
        "replay and live delivery leave no duplicate"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalidated_live_observation_recovers_with_an_authoritative_snapshot() -> Result<()> {
    use futures_util::FutureExt as _;
    use lash_core::LiveReplayStore as _;
    let replay = Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
    ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id = SessionId::from("invalidated-live-observation");
    let session = created_session(&core, session_id.clone())
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
    let lash::observe::SessionObservationStreamItem::Gap { observation, gap } = recovered else {
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
        if let lash::observe::SessionObservationStreamItem::Event(event) = item
            && observation_assistant_delta(&event).as_deref() == Some("echo: live after resync")
        {
            break;
        }
    }
    Ok(())
}

const QUOTA_CODE: &str = "insufficient_quota";

fn quota_refused_provider() -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("quota-refused")
        .requires_streaming(true)
        .complete(|_request| async move {
            Err(LlmTransportError::new("You exceeded your current quota.")
                .with_kind(lash_core::ProviderFailureKind::Quota)
                .with_code(lash_core::FailureCode::provider(QUOTA_CODE))
                .with_retry_verdict(lash::provider::TransportRetryVerdict::NotRetryable))
        })
        .build()
        .into_handle()
}

/// The failure envelopes a feed of turn activities carries.
fn reported_failures<'a>(
    activities: impl IntoIterator<Item = &'a lash_core::TurnActivity>,
) -> Vec<&'a lash_sansio::ErrorEnvelope> {
    activities
        .into_iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::Error(failure) => failure.envelope.as_ref(),
            _ => None,
        })
        .collect()
}

/// A live replay whose publication of a reported failure is slow: the
/// stop's terminal, which its turn holds until the commit is accepted
/// (ADR 0122), reaches the replay well after the store shows the run ended.
struct SlowFailurePublication {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
}

impl SlowFailurePublication {
    const DELAY: std::time::Duration = std::time::Duration::from_millis(300);
}

#[async_trait::async_trait]
impl lash_core::LiveReplayStore for SlowFailurePublication {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        if events.iter().any(|event| {
            matches!(
                &event.payload,
                lash_core::SessionObservationEventPayload::TurnActivity(activity)
                    if matches!(activity.event, TurnEvent::Error(_))
            )
        }) {
            tokio::time::sleep(Self::DELAY).await;
        }
        self.inner.publish(session_id, revision, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    fn earliest_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.earliest_cursor(session_id, revision)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
    }

    async fn invalidate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

/// FIG-5526: a provider failure with a structured vendor code reaches a
/// host's turn-activity feed with its kind, code and retryability, and the
/// live replay hands a reopened session the same typed payload — a host
/// never parses the message to classify the failure.
///
/// FIG-5793: the failure is the stop's terminal, published only after the
/// turn's commit, so the store shows the run ended first. The replay here
/// publishes it late on purpose: the send follower still collects it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_failure_reaches_the_activity_feed_typed_and_reads_back_after_a_reopen()
-> Result<()> {
    let replay = Arc::new(SlowFailurePublication {
        inner: lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
        ),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(quota_refused_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id = SessionId::from("typed-failure-activity");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let before = session.observe().snapshot().await?;
    let output = session.send(TurnInput::text("hello")).output().await?;

    let assert_typed = |envelopes: Vec<&lash_sansio::ErrorEnvelope>, feed: &str| {
        let [envelope] = envelopes.as_slice() else {
            panic!("{feed} carries exactly one reported failure: {envelopes:?}");
        };
        assert_eq!(
            envelope.kind,
            lash_sansio::TurnFailureKind::LlmProvider,
            "{feed}"
        );
        assert_eq!(
            envelope.code,
            Some(lash_core::FailureCode::provider(QUOTA_CODE)),
            "{feed}"
        );
        assert_eq!(envelope.retryable, Some(false), "{feed}");
        assert_eq!(
            envelope.provider_failure_kind,
            Some(lash_core::ProviderFailureKind::Quota),
            "{feed}"
        );
    };
    assert_typed(
        reported_failures(&output.activities),
        "the turn's activities",
    );

    drop(session);
    let reopened = core.session(session_id).open().await?;
    let lash_core::facade_support::SessionResume::Replayed { events } = reopened
        .observe()
        .resume_from_cursor(&before.cursor)
        .await?
    else {
        panic!("the live replay continues the cursor taken before the turn");
    };
    let replayed = events
        .iter()
        .filter_map(|event| match &event.payload {
            lash_core::SessionObservationEventPayload::TurnActivity(activity) => Some(activity),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_typed(
        reported_failures(replayed),
        "the live replay after a reopen",
    );
    core.shutdown().await?;
    Ok(())
}
