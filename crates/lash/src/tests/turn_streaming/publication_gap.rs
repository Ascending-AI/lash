//! A turn's commit does not depend on its publication: a live-replay store
//! that refuses the commit's observation leaves the turn committed, and a
//! reader from before the turn is not shown a bridge to the commit. A
//! publication that is only late is not a gap: a reader inside the window
//! between the commit and its observation replays the commit.

use super::*;

/// What the store does to a turn's commit publication.
#[derive(Debug)]
enum Fault {
    /// Refuse it.
    Refuse,
    /// Hold it until the test releases it.
    Hold,
}

/// An in-memory live-replay store that, once armed, meets the first batch
/// carrying a `Committed` observation of committed rows, a turn's commit,
/// with its fault.
#[derive(Debug)]
struct CommitPublicationStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    fault: Fault,
    armed: std::sync::atomic::AtomicBool,
    met: std::sync::atomic::AtomicBool,
    /// Signalled when the commit's publication reached the store.
    reached: tokio::sync::Notify,
    /// Releases a held publication.
    released: tokio::sync::Notify,
    /// Release the held publication after the next replay read answered.
    release_after_replay: std::sync::atomic::AtomicBool,
}

impl CommitPublicationStore {
    fn new(fault: Fault) -> Arc<Self> {
        Arc::new(Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
            fault,
            armed: std::sync::atomic::AtomicBool::new(false),
            met: std::sync::atomic::AtomicBool::new(false),
            reached: tokio::sync::Notify::new(),
            released: tokio::sync::Notify::new(),
            release_after_replay: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

fn commits_rows(event: &lash_core::SessionObservationEventPayload) -> bool {
    matches!(
        event,
        lash_core::SessionObservationEventPayload::Committed { entries: rows, .. }
            if !rows.is_empty()
    )
}

#[async_trait]
impl lash_core::LiveReplayStore for CommitPublicationStore {
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
            && events.iter().any(|event| commits_rows(&event.payload))
            && !self.met.swap(true, Ordering::SeqCst)
        {
            self.reached.notify_one();
            match self.fault {
                Fault::Refuse => {
                    return Err(lash_core::LiveReplayStoreError::Store(
                        "injected publication failure".into(),
                    ));
                }
                Fault::Hold => self.released.notified().await,
            }
        }
        self.inner.publish(session, revision, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        let outcome = self.inner.replay_after_cursor(cursor).await;
        if self.release_after_replay.swap(false, Ordering::SeqCst) {
            self.released.notify_one();
        }
        outcome
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
    let replay = CommitPublicationStore::new(Fault::Refuse);
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
    // The run answers from its durable terminal, ahead of its commit's
    // publication (FIG-5507): wait for the store to refuse it.
    replay.reached.notified().await;
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
        !events.iter().any(|event| commits_rows(&event.payload)),
        "{events:#?}"
    );
    Ok(())
}

/// A turn's commit is durable before its `Committed` observation is
/// published, and its run answers from the durable terminal. A reader that
/// resumes from its pre-turn cursor inside that window finds the head ahead
/// of everything the replay holds; it is answered once the publication was
/// attempted, with the replayed commit, and never with a gap (FIG-5605).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_reader_inside_a_commits_publication_window_replays_the_commit() -> Result<()>
{
    let replay = CommitPublicationStore::new(Fault::Hold);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        queued_text_provider(vec!["answered inside the publication window"]),
        mock_llm_profile_spec(),
    )
    .live_replay_store(replay.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("publication-window").expect("nonblank host identity"))
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
        Some("answered inside the publication window")
    );
    // The commit is durable and its publication is held. It is released
    // only after the reader's replay read answered without it.
    replay.reached.notified().await;
    replay.release_after_replay.store(true, Ordering::SeqCst);

    let events = match session.observe().resume_from_cursor(&cursor).await? {
        lash_core::facade_support::SessionResume::Replayed { events } => events,
        lash_core::facade_support::SessionResume::Gap { gap, .. } => {
            panic!("a commit still being published is not a gap: {gap:?}")
        }
    };
    assert!(
        events.iter().any(|event| commits_rows(&event.payload)),
        "the replay bridges to the commit: {events:#?}"
    );
    Ok(())
}
