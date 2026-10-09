//! A turn's commit does not depend on its publication: a live-replay store
//! that refuses the commit's observation leaves the turn committed, and a
//! reader from before the turn is not shown a bridge to the commit.

use super::*;

/// An in-memory live-replay store that, once armed, refuses the first batch
/// carrying a `Committed` observation of committed rows: a turn's commit.
#[derive(Debug)]
struct PublicationFailureStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    armed: std::sync::atomic::AtomicBool,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl lash_core::LiveReplayStore for PublicationFailureStore {
    async fn publish(
        &self,
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        if self.armed.load(Ordering::SeqCst)
            && events.iter().any(|event| {
                matches!(
                    &event.payload,
                    lash_core::SessionObservationEventPayload::Committed { entries: rows, .. }
                        if !rows.is_empty()
                )
            })
            && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(lash_core::LiveReplayStoreError::Store(
                "injected publication failure".into(),
            ));
        }
        self.inner.publish(session, revision, events).await
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
        session: &lash_core::SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session, revision)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
    }

    async fn invalidate_session(
        &self,
        session: &lash_core::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session).await
    }

    async fn trim_session(
        &self,
        session: &lash_core::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn publication_failure_preserves_committed_turn_and_exposes_gap() -> Result<()> {
    let replay = Arc::new(PublicationFailureStore {
        inner: lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
        ),
        armed: std::sync::atomic::AtomicBool::new(false),
        failed: std::sync::atomic::AtomicBool::new(false),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        queued_text_provider(vec!["committed answer despite publication failure"]),
        mock_llm_profile_spec(),
    )
    .live_replay_store(replay.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("publication-failure").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let cursor = session.observe().snapshot().await?.cursor;
    replay.armed.store(true, Ordering::SeqCst);

    let output = session
        .send(TurnInput::text("answer once"))
        .output()
        .await?;

    assert_eq!(
        output.assistant_message(),
        Some("committed answer despite publication failure")
    );
    assert!(
        replay.failed.load(Ordering::SeqCst),
        "the commit's publication was refused"
    );
    assert_eq!(
        committed(&session).await.turn_index(),
        1,
        "the turn committed"
    );
    // The failed publication took no position, so nothing after the
    // pre-turn cursor bridges to the commit.
    let lash_core::LiveReplayOutcome::Replayed(events) =
        lash_core::LiveReplayStore::replay_after_cursor(replay.as_ref(), &cursor)
            .await
            .expect("replay after the pre-turn cursor")
    else {
        panic!("the pre-turn cursor stays inside the window");
    };
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            lash_core::SessionObservationEventPayload::Committed { entries: rows, .. } if !rows.is_empty()
        )),
        "{events:#?}"
    );
    Ok(())
}
