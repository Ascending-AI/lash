use super::*;
use lash_sansio::llm::types::{StreamBlockEvent, StreamBlockKind};

pub(super) fn observation_assistant_delta(
    event: &lash_core::SessionObservationEvent,
) -> Option<String> {
    match &event.payload {
        lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
            match &activity.event {
                TurnEvent::StreamBlock(StreamBlockEvent::Delta {
                    kind: StreamBlockKind::AssistantText,
                    text,
                    ..
                }) => Some(text.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
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

const QUOTA_CODE: &str = "insufficient_quota";

fn quota_refused_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("quota-refused")
        .requires_streaming(true)
        .complete(|_request| async move {
            Err(LlmTransportError::new("You exceeded your current quota.")
                .with_kind(lash_core::ProviderFailureKind::Quota)
                .with_code(lash_core::FailureCode::provider(QUOTA_CODE))
                .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::NotRetryable))
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
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from("typed-failure-activity");
    let session = core
        .session(session_id.clone())
        .created()
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
