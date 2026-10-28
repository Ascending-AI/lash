//! A turn's commit does not depend on its publication: a live-replay store
//! that refuses the commit's observation leaves the turn committed, and a
//! reader from before the turn is not shown a bridge to the commit. A
//! publication that is only late is not a gap: a reader inside the window
//! between a commit and its observation replays the commit, whichever
//! commit the head stands on when the reader looks, and a process feed
//! answers the same schedule the same way.

use super::*;
use futures_util::StreamExt as _;

/// What the store does to a turn's commit publication.
#[derive(Debug)]
enum Fault {
    /// Refuse it.
    Refuse,
    /// Hold it until the test releases it.
    Hold,
    /// Retain the commit but lose its notification to the existing reader.
    MissNotification,
    /// Retain publications while the reader's live tail hears nothing.
    SilentLive,
}

/// A law's hold on a replay store: the commit publication it holds, and
/// what the reader under test has read.
#[derive(Debug, Default)]
struct Gate {
    /// Signalled when an armed publication reached the store.
    reached: tokio::sync::Notify,
    /// Releases a held publication.
    released: tokio::sync::Notify,
    /// Signalled when a read of the reader under test answered.
    read: tokio::sync::Notify,
    /// Release the held publication after the reader's next read answered.
    release_after_read: std::sync::atomic::AtomicBool,
}

impl Gate {
    /// Hold a publication that reached the store until it is released.
    async fn hold(&self) {
        self.reached.notify_one();
        self.released.notified().await;
    }

    fn release(&self) {
        self.released.notify_one();
    }

    /// The reader under test was answered a read.
    fn answered(&self) {
        self.read.notify_one();
        if self.release_after_read.swap(false, Ordering::SeqCst) {
            self.release();
        }
    }

    /// Poll `reader` until its read was answered. It stays pending: it
    /// found its head unbridged and waits on the publication held here.
    async fn parked<T>(&self, reader: impl Future<Output = T>) {
        tokio::select! {
            biased;
            _ = reader => panic!("the reader answers only once the held publication was attempted"),
            () = self.read.notified() => {}
        }
    }
}

/// An in-memory live-replay store that meets a batch carrying a `Committed`
/// observation of committed rows, a turn's commit, with its fault: once for
/// each time it was armed.
#[derive(Debug)]
struct CommitPublicationStore {
    inner: lash_core::facade_support::InMemoryLiveReplayStore,
    fault: Fault,
    armed: std::sync::atomic::AtomicBool,
    gate: Gate,
    /// The cursor of the reader under test: only its reads pass the gate.
    reader: std::sync::Mutex<Option<lash_core::SessionCursor>>,
    missed: Arc<std::sync::atomic::AtomicBool>,
    published: tokio::sync::Notify,
    boundary: tokio::sync::Mutex<()>,
}

impl CommitPublicationStore {
    fn new(fault: Fault) -> Arc<Self> {
        Arc::new(Self {
            inner: lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
            ),
            fault,
            armed: std::sync::atomic::AtomicBool::new(false),
            gate: Gate::default(),
            reader: std::sync::Mutex::new(None),
            missed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            published: tokio::sync::Notify::new(),
            boundary: tokio::sync::Mutex::new(()),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn read_by(&self, cursor: &lash_core::SessionCursor) {
        if self.reader.lock().expect("reader cursor").as_ref() == Some(cursor) {
            self.gate.answered();
        }
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
        session: &lash_kernel_value::identity::SessionId,
        revision: lash_core::SessionRevision,
        events: Vec<lash_core::LiveReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::SessionObservationEvent>>,
        lash_core::LiveReplayStoreError,
    > {
        if events.iter().any(|event| commits_rows(&event.payload))
            && self.armed.swap(false, Ordering::SeqCst)
        {
            match self.fault {
                Fault::Refuse => {
                    self.gate.reached.notify_one();
                    return Err(lash_core::LiveReplayStoreError::Store(
                        "injected publication failure".into(),
                    ));
                }
                Fault::Hold => self.gate.hold().await,
                Fault::MissNotification => self.missed.store(true, Ordering::SeqCst),
                Fault::SilentLive => {}
            }
        }
        let committed = events.iter().any(|event| commits_rows(&event.payload));
        let _boundary = self.boundary.lock().await;
        let result = self.inner.publish(session, revision, events).await;
        if committed && result.is_ok() {
            self.published.notify_one();
        }
        result
    }

    async fn replay_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplayOutcome, lash_core::LiveReplayStoreError> {
        let outcome = self.inner.replay_after_cursor(cursor).await;
        self.read_by(cursor);
        outcome
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::SessionCursor,
    ) -> std::result::Result<lash_core::LiveReplaySubscribeOutcome, lash_core::LiveReplayStoreError>
    {
        let _boundary = self.boundary.lock().await;
        let prefix = if matches!(self.fault, Fault::MissNotification | Fault::SilentLive)
            && self.reader.lock_recover().as_ref() == Some(cursor)
        {
            Some(self.inner.replay_after_cursor(cursor).await?)
        } else {
            None
        };
        let mut outcome = self.inner.subscribe_after_cursor(cursor).await;
        if let Some(lash_core::LiveReplayOutcome::Replayed(prefix)) = prefix
            && let Ok(lash_core::LiveReplaySubscribeOutcome::Subscribed(subscription)) = outcome
        {
            let missed = Arc::clone(&self.missed);
            let silent = matches!(self.fault, Fault::SilentLive);
            let replayed = prefix.len();
            outcome = Ok(lash_core::LiveReplaySubscribeOutcome::Subscribed(
                lash_core::LiveReplaySubscription::new(
                    prefix,
                    subscription.skip(replayed).filter_map(move |item| {
                        let drop_notification = silent
                            || item.as_ref().is_ok_and(|event| {
                                commits_rows(&event.payload) && missed.swap(false, Ordering::SeqCst)
                            });
                        std::future::ready((!drop_notification).then_some(item))
                    }),
                ),
            ));
        }
        self.read_by(cursor);
        outcome
    }

    fn current_cursor(
        &self,
        session: &lash_kernel_value::identity::SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.inner.current_cursor(session, revision)
    }

    fn tail_cursor(
        &self,
        session_id: &lash_kernel_value::identity::SessionId,
    ) -> lash_core::SessionCursor {
        self.inner.tail_cursor(session_id)
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_all().await
    }

    async fn invalidate_session(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.invalidate_session(session).await
    }

    async fn trim_session(
        &self,
        session: &lash_kernel_value::identity::SessionId,
    ) -> std::result::Result<(), lash_core::LiveReplayStoreError> {
        self.inner.trim_session(session).await
    }
}

/// The observer fails before it can install a reconnecting listener.
struct FailFirstObserver {
    inner: Arc<dyn lash_durable::NodeWakes>,
    opens: Arc<AtomicUsize>,
}

#[async_trait]
impl lash_durable::NodeWakes for FailFirstObserver {
    async fn publish(
        &self,
        batch: &lash_durable::WakeBatch,
    ) -> std::result::Result<(), lash_durable::DurableError> {
        self.inner.publish(batch).await
    }

    async fn committed(
        &self,
        batch: &lash_durable::WakeBatch,
    ) -> std::result::Result<(), lash_durable::DurableError> {
        self.inner.committed(batch).await
    }

    async fn listen(
        &self,
        lease: &lash_durable::NodeLease,
    ) -> std::result::Result<Box<dyn lash_durable::NodeWakeFeed>, lash_durable::DurableError> {
        self.inner.listen(lease).await
    }

    async fn observe(
        &self,
    ) -> std::result::Result<Box<dyn lash_durable::NodeWakeFeed>, lash_durable::DurableError> {
        if self.opens.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(lash_durable::DurableError::Store(
                lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: "injected notification-source acquisition failure".into(),
                },
            ));
        }
        self.inner.observe().await
    }

    async fn liveness(
        &self,
    ) -> std::result::Result<Vec<lash_durable::BootLiveness>, lash_durable::DurableError> {
        self.inner.liveness().await
    }

    async fn reap_released(
        &self,
        reaper: &lash_durable::NodeLease,
        boot: &lash_durable::Owner,
    ) -> std::result::Result<Vec<lash_durable::Reaped>, lash_durable::DurableError> {
        self.inner.reap_released(reaper, boot).await
    }
}

/// FIG-7538: a failed notification prerequisite yields its typed cause,
/// without retrying or advancing the cursor; a new subscription continues it.
/// An intentionally transport-free memory store still delivers local commits.
#[tokio::test]
async fn notification_acquisition_failure_errors_and_resubscribe_delivers_the_commit_once()
-> Result<()> {
    for has_transport in [true, false] {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            notification_acquisition_scenario(has_transport),
        )
        .await
        .expect("notification acquisition law deadlock watchdog")?;
    }
    Ok(())
}

async fn notification_acquisition_scenario(has_transport: bool) -> Result<()> {
    use crate::observe::{SessionObservationEventPayload, SessionObservationStreamItem};
    use futures_util::FutureExt as _;
    use lash_execution_contracts::backend::StoreSet;

    let directory = tempfile::tempdir().expect("private feed database");
    let open_file = || async {
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(
                directory.path().join("feed.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("open SQLite file stores"),
        ) as Arc<dyn StoreSet>
    };
    let writer_stores: Arc<dyn StoreSet> = if has_transport {
        open_file().await
    } else {
        sqlite_memory_store_set().await
    };
    let opens = Arc::new(AtomicUsize::new(0));
    let reader_stores = if has_transport {
        let opens = Arc::clone(&opens);
        lash_core::testing::runtime_helpers::LayeredStores::over(open_file().await)
            .map_node_wakes(move |inner| Arc::new(FailFirstObserver { inner, opens }))
            .into_store_set()
    } else {
        writer_stores.clone()
    };
    assert_eq!(reader_stores.node_wakes().is_some(), has_transport);
    let replay = CommitPublicationStore::new(if has_transport {
        Fault::SilentLive
    } else {
        Fault::Hold
    });
    let writer = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(writer_stores),
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .build(crate::tests::host::runtime_lease_owner())?;
    let reader = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(reader_stores),
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(replay.clone())
    .serve_sessions(false)
    .build(crate::tests::host::runtime_lease_owner())?;
    let session_id = SessionId::from("failed-feed-prerequisite");
    let session = created_session(&writer, session_id.clone())
        .await
        .open()
        .await?;
    let state = lash_core::store::load_session_window_state(
        &session.binding.store(),
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("created durable head")
    .state;
    let observed = reader.session(session_id).observe_with_state(state).await?;
    let (snapshot, mut feed) = observed.observe().follow(crate::FeedStart::Head).await?;
    let cursor = snapshot.cursor;
    *replay.reader.lock_recover() = Some(cursor.clone());
    tokio::time::pause();
    let acquisition_time = tokio::time::Instant::now();
    let mut first = None;
    for _ in 0..256 {
        if first.is_none() {
            first = feed.next().now_or_never().flatten();
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(tokio::time::Instant::now(), acquisition_time);
    let answered_without_time = first.is_some();
    tokio::time::resume();
    // The unfixed feed reaches live tailing after losing the error;
    // the fixed feed returns it before any subscription read.
    if first.is_none() {
        first = tokio::select! {
            biased;
            item = feed.next() => Some(item.expect("one error item")),
            () = replay.gate.read.notified() => None,
        };
    }
    let handle = session
        .send(TurnInput::text("retained across failed acquisition"))
        .await?;
    replay.published.notified().await;
    tokio::time::pause();
    let frozen = tokio::time::Instant::now();
    let mut commits = Vec::new();
    for _ in 0..256 {
        if first.is_none() {
            first = feed.next().now_or_never().flatten();
        }
        if !has_transport && let Some(item) = first.take() {
            match item? {
                SessionObservationStreamItem::Event(event)
                    if matches!(
                        &event.payload,
                        SessionObservationEventPayload::Committed { .. }
                    ) =>
                {
                    commits.push(event)
                }
                SessionObservationStreamItem::Event(_) => {}
                SessionObservationStreamItem::Gap { gap, .. } => {
                    panic!("local retained commit gapped: {gap:?}")
                }
            }
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(tokio::time::Instant::now(), frozen, "no fallback timer ran");
    tokio::time::resume();
    if has_transport {
        assert!(
            answered_without_time,
            "acquisition must fail without a timer"
        );
        let error = first
            .expect("failed acquisition must yield an error, not remain pending")
            .expect_err("notification prerequisite failure must not become a live subscription");
        assert!(
            matches!(error, EmbedError::Durable(lash_durable::DurableError::Store(ref cause))
            if cause.kind == lash_durable::StoreFailureKind::Unavailable
            && cause.message == "injected notification-source acquisition failure"),
            "typed acquisition cause: {error:?}"
        );
        assert_eq!(feed.cursor(), &cursor, "failure consumes no cursor");
        assert!(
            matches!(feed.next().now_or_never(), Some(None)),
            "the failed stream ends without retry"
        );
        assert_eq!(
            opens.load(Ordering::SeqCst),
            1,
            "no internal acquisition retry"
        );
        feed = observed.observe().subscribe_and_recover(cursor);
        while commits.is_empty() {
            match feed.next().await.expect("resubscribed feed stays open")? {
                SessionObservationStreamItem::Event(event)
                    if matches!(
                        &event.payload,
                        SessionObservationEventPayload::Committed { .. }
                    ) =>
                {
                    commits.push(event)
                }
                SessionObservationStreamItem::Event(_) => {}
                SessionObservationStreamItem::Gap { gap, .. } => {
                    panic!("retained recovery gapped: {gap:?}")
                }
            }
        }
        assert_eq!(
            opens.load(Ordering::SeqCst),
            2,
            "the host resubscription opens the observer"
        );
    }
    let outcome = handle.outcome().await?;
    let run = outcome.run().expect("committed run").as_str();
    let [commit] = commits.as_slice() else {
        panic!("one commit, transport={has_transport}: {commits:?}");
    };
    let SessionObservationEventPayload::Committed { entries, .. } = &commit.payload else {
        unreachable!()
    };
    let replies = entries
        .iter()
        .filter(|entry| entry.provenance.is_turn_reply)
        .map(|entry| {
            entry
                .provenance
                .turn_id
                .as_ref()
                .expect("reply turn")
                .as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(replies, vec![run]);
    while let Some(item) = feed.next().now_or_never() {
        match item.expect("feed stays open")? {
            SessionObservationStreamItem::Event(event) => assert!(
                !matches!(
                    &event.payload,
                    SessionObservationEventPayload::Committed { .. }
                ),
                "commit delivered twice"
            ),
            SessionObservationStreamItem::Gap { gap, .. } => panic!("continuation gapped: {gap:?}"),
        }
    }
    reader.shutdown().await?;
    writer.shutdown().await?;
    Ok(())
}

/// FIG-7531: a reader opened from Current follows two adjacent turns without
/// a timer, even when B's retained commit loses its live notification.
#[tokio::test]
async fn queued_turn_commits_reach_the_reader_without_a_live_notification_or_timer() -> Result<()> {
    use crate::observe::{SessionObservationEventPayload, SessionObservationStreamItem};
    use futures_util::FutureExt as _;
    use lash_core::LiveReplayStore as _;

    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for miss_notification in [false, true] {
            let replay = CommitPublicationStore::new(if miss_notification {
                Fault::MissNotification
            } else {
                Fault::Hold
            });
            let reached = Arc::new(tokio::sync::Notify::new());
            let released = Arc::new(tokio::sync::Notify::new());
            let provider = crate::testing::TestProvider::builder()
                .kind("queued-feed-commit")
                .complete({
                    let reached = Arc::clone(&reached);
                    let released = Arc::clone(&released);
                    move |request| {
                        let reached = Arc::clone(&reached);
                        let released = Arc::clone(&released);
                        async move {
                            let answer = last_user_text(&request);
                            reached.notify_one();
                            released.notified().await;
                            Ok(text_response(&answer))
                        }
                    }
                })
                .build()
                .into_handle();
            let backend = sqlite_memory_store_backend().await;
            let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .live_replay_store(replay.clone())
                .build(crate::tests::host::runtime_lease_owner())?;
            let session_id = SessionId::from("queued-feed-commit");
            let writer = created_session(&core, session_id.clone())
                .await
                .open()
                .await?;
            let state = lash_core::store::load_session_window_state(
                &writer.binding.store(),
                lash_core::store::WindowSelector::Current,
            )
            .await?
            .expect("created head")
            .state;
            let reader = core
                .session(session_id.clone())
                .observe_with_state(state)
                .await?;
            let (snapshot, mut feed) = reader.observe().follow(crate::FeedStart::Head).await?;
            *replay.reader.lock_recover() = Some(snapshot.cursor.clone());

            tokio::select! {
                biased;
                _ = feed.next() => panic!("the initial feed has no events"),
                () = replay.gate.read.notified() => {},
            }
            let a = writer.send(TurnInput::text("answer A")).await?;
            reached.notified().await;
            let b = writer.send(TurnInput::text("answer B")).await?;
            released.notify_one();
            reached.notified().await;
            assert_ne!(a.input_id(), b.input_id(), "B is a separately queued input");

            let mut commits = Vec::new();
            let record = |item: SessionObservationStreamItem, commits: &mut Vec<String>| match item
            {
                SessionObservationStreamItem::Gap { gap, .. } => {
                    panic!("retained commits have no gap: {gap:?}, missed={miss_notification}")
                }
                SessionObservationStreamItem::Event(event) => {
                    if let SessionObservationEventPayload::Committed { entries, .. } =
                        &event.payload
                    {
                        commits.extend(
                            entries
                                .iter()
                                .filter(|entry| entry.provenance.is_turn_reply)
                                .map(|entry| {
                                    entry
                                        .provenance
                                        .turn_id
                                        .as_ref()
                                        .expect("reply turn")
                                        .as_str()
                                        .to_owned()
                                }),
                        );
                    }
                }
            };
            // B is held at its final model reply while A's commit is consumed.
            replay.published.notified().await;
            while commits.is_empty() {
                record(
                    feed.next().await.expect("the feed stays open")?,
                    &mut commits,
                );
            }
            while let Some(item) = feed.next().now_or_never() {
                record(item.expect("the feed stays open")?, &mut commits);
            }
            let a_run = a.outcome().await?.run().cloned().expect("A committed");
            if miss_notification {
                replay.arm();
            }
            released.notify_one();
            let b_run = b.outcome().await?.run().cloned().expect("B committed");
            replay.published.notified().await;
            let head = replay
                .inner
                .tail_cursor(&session_id)
                .parse()
                .expect("retained head")
                .revision;
            replay
                .inner
                .publish(
                    &session_id,
                    head,
                    vec![lash_core::LiveReplayEventDraft::new(
                        None::<lash_vocabulary::identity::TurnId>,
                        SessionObservationEventPayload::ResidentChanged,
                    )],
                )
                .await
                .expect("auxiliary traffic after B's commit");
            tokio::time::pause();
            let frozen = tokio::time::Instant::now();
            for _ in 0..256 {
                while let Some(item) = feed.next().now_or_never() {
                    record(item.expect("the feed stays open")?, &mut commits);
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(tokio::time::Instant::now(), frozen, "no fallback timer ran");
            assert_eq!(
                commits,
                vec![a_run.as_str(), b_run.as_str()],
                "ordered and once, missed={miss_notification}"
            );
            tokio::time::resume();
            core.shutdown().await?;
        }
        Ok(())
    })
    .await
    .expect("queued-turn feed law deadlock watchdog")
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
    .build(crate::tests::host::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::parse("publication-failure")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session.observe().snapshot().await?.cursor;
    replay.arm();

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
    replay.gate.reached.notified().await;
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
    .build(crate::tests::host::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::parse("publication-window")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session.observe().snapshot().await?.cursor;
    replay.arm();

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
    replay.gate.reached.notified().await;
    *replay.reader.lock().expect("reader cursor") = Some(cursor.clone());
    replay.gate.release_after_read.store(true, Ordering::SeqCst);

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

/// A session whose two turns' commit publications the store holds, and the
/// cursor a reader took before either.
async fn two_commit_session(
    name: &str,
) -> Result<(
    Arc<CommitPublicationStore>,
    LashCore,
    crate::LashSession,
    lash_core::SessionCursor,
)> {
    let replay = CommitPublicationStore::new(Fault::Hold);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        queued_text_provider(vec!["the first commit", "the second commit"]),
        mock_llm_profile_spec(),
    )
    .live_replay_store(replay.clone())
    .build(crate::tests::host::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_kernel_value::identity::SessionId::parse(name).expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let cursor = session.observe().snapshot().await?.cursor;
    Ok((replay, core, session, cursor))
}

/// Commit the session's first turn and hold its publication. Reads from
/// `reader` pass the gate from here on: the turn's own are over.
async fn first_commit_held(
    replay: &CommitPublicationStore,
    session: &crate::LashSession,
    reader: &lash_core::SessionCursor,
) {
    replay.arm();
    session
        .send(TurnInput::text("first"))
        .output()
        .await
        .expect("the first turn commits");
    replay.gate.reached.notified().await;
    *replay.reader.lock().expect("reader cursor") = Some(reader.clone());
}

/// Publish the first commit, and commit the second with its publication
/// held: the head a parked reader reads next is the second commit's. The
/// second publication is released once that reader's read answered.
async fn second_commit_held(replay: &CommitPublicationStore, session: &crate::LashSession) {
    replay.arm();
    replay.gate.release();
    let (second, ()) = tokio::join!(
        session.send(TurnInput::text("second")).output(),
        replay.gate.reached.notified(),
    );
    second.expect("the second turn commits");
    replay.gate.release_after_read.store(true, Ordering::SeqCst);
}

/// The settlement of a publication window is that of one commit. A reader
/// waits out the first commit's publication; before it reads the head again
/// the same node commits a second time and is still publishing it. The head
/// the reader finds is the second commit's, whose publication it waits out
/// in turn: neither commit is a gap (FIG-5626).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_reader_between_two_commits_publications_replays_both() -> Result<()> {
    let (replay, _core, session, cursor) = two_commit_session("two-publication-windows").await?;
    first_commit_held(&replay, &session, &cursor).await;

    let observed = session.observe();
    let mut resume = std::pin::pin!(observed.resume_from_cursor(&cursor));
    replay.gate.parked(&mut resume).await;
    second_commit_held(&replay, &session).await;

    let events = match resume.await? {
        lash_core::facade_support::SessionResume::Replayed { events } => events,
        lash_core::facade_support::SessionResume::Gap { gap, .. } => {
            panic!("a commit still being published is not a gap: {gap:?}")
        }
    };
    assert_eq!(
        events
            .iter()
            .filter(|event| commits_rows(&event.payload))
            .count(),
        2,
        "the replay carries both commits: {events:#?}"
    );
    Ok(())
}

/// What a feed showed its consumer of the durable subject.
#[derive(Debug, PartialEq, Eq)]
enum Shown {
    Commit,
    Gap,
}

/// An in-memory process replay store that holds the publication of the
/// committed fact at each armed sequence.
struct FactPublicationStore {
    inner: lash_core::InMemoryProcessReplayStore,
    armed: std::sync::Mutex<Vec<u64>>,
    gate: Gate,
    /// How many committed facts were handed to the store.
    facts: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl lash_core::ProcessReplayStore for FactPublicationStore {
    async fn publish(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
        events: Vec<lash_core::ProcessReplayEventDraft>,
    ) -> std::result::Result<
        Vec<Arc<lash_core::ProcessObservationEvent>>,
        lash_core::ProcessReplayStoreError,
    > {
        let facts = events
            .iter()
            .filter(|draft| {
                matches!(
                    draft.payload(),
                    lash_core::ProcessObservationEventPayload::Committed { .. }
                )
            })
            .map(|draft| draft.sequence().as_u64())
            .collect::<Vec<_>>();
        self.facts.fetch_add(facts.len(), Ordering::SeqCst);
        let held = {
            let mut armed = self.armed.lock().expect("armed sequences");
            let before = armed.len();
            armed.retain(|sequence| !facts.contains(sequence));
            armed.len() != before
        };
        if held {
            self.gate.hold().await;
        }
        self.inner.publish(process_id, events).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &lash_core::ProcessObservationCursor,
    ) -> std::result::Result<
        lash_core::ProcessReplaySubscribeOutcome,
        lash_core::ProcessReplayStoreError,
    > {
        let outcome = self.inner.subscribe_after_cursor(cursor).await;
        self.gate.answered();
        outcome
    }

    async fn earliest_cursor(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
        sequence: lash_core::ProcessSequence,
    ) -> std::result::Result<lash_core::ProcessObservationCursor, lash_core::ProcessReplayStoreError>
    {
        self.inner.earliest_cursor(process_id, sequence).await
    }

    async fn invalidate_process(
        &self,
        process_id: &lash_vocabulary::identity::ProcessId,
    ) -> std::result::Result<(), lash_core::ProcessReplayStoreError> {
        self.inner.invalidate_process(process_id).await
    }

    async fn invalidate_all(&self) -> std::result::Result<(), lash_core::ProcessReplayStoreError> {
        self.inner.invalidate_all().await
    }
}

/// The session feed across two commits whose publications are each held
/// while the feed judges the head they made.
async fn session_feed_across_two_publication_windows() -> Result<Vec<Shown>> {
    let (replay, _core, session, cursor) = two_commit_session("two-windows-session-feed").await?;
    first_commit_held(&replay, &session, &cursor).await;

    let mut feed = session.observe().subscribe_and_recover(cursor);
    replay.gate.parked(feed.next()).await;
    second_commit_held(&replay, &session).await;

    let mut shown = Vec::new();
    while shown.len() < 2 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(30), feed.next())
            .await
            .expect("the session feed delivers")
            .expect("the session feed stays open")?;
        match item {
            crate::observe::SessionObservationStreamItem::Gap { .. } => {
                shown.push(Shown::Gap);
                break;
            }
            crate::observe::SessionObservationStreamItem::Event(event)
                if commits_rows(&event.payload) =>
            {
                shown.push(Shown::Commit);
            }
            crate::observe::SessionObservationStreamItem::Event(_) => {}
        }
    }
    Ok(shown)
}

/// The process feed across the same schedule, and how many committed facts
/// reached its replay store.
async fn process_feed_across_two_publication_windows() -> Result<(Vec<Shown>, usize)> {
    let replay = Arc::new(FactPublicationStore {
        inner: lash_core::InMemoryProcessReplayStore::new(
            lash_core::InMemoryProcessReplayStoreConfig::standard(),
        ),
        armed: std::sync::Mutex::new(Vec::new()),
        gate: Gate::default(),
        facts: std::sync::atomic::AtomicUsize::new(0),
    });
    // The feed looks at the durable process only when a commit ticks it:
    // nothing but the schedule decides what it reads.
    let stores = sqlite_memory_store_set().await;
    let seeding: Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry> =
        stores.process_registry();
    let core = standard_core_builder_over(lash_conformance::backend_over(stores))
        .process_replay_store(replay.clone())
        .build(crate::tests::host::runtime_lease_owner())?;
    let watched = core.process_watch().clone();
    let registry = core.process_registry.clone();
    let process_id = registry
        .register_process(lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::testing::held_engine_input(serde_json::Value::Null),
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await?
        .id;
    let observed = core.processes().observe(&process_id);
    let (snapshot, mut feed) = observed
        .follow(crate::process::ProcessFeedStart::Retained)
        .await?;
    let base = snapshot.read_view.sequence().expect("retained").as_u64();
    let published_before = replay.facts.load(Ordering::SeqCst);

    // The first commit, its publication held.
    replay
        .armed
        .lock()
        .expect("armed sequences")
        .extend([base + 1, base + 2]);
    let wait = lash_execution_contracts::process::WaitState {
        since_ms: 0,
        kind: lash_execution_contracts::process::WaitKind::Call {
            resume_at_ms: None,
            call_id: lash_core::ToolCallId::fixture("two-windows-call"),
            tool_id: lash_vocabulary::tool_contract::ToolId::from("two-windows-fixture"),
        },
        site: None,
    };
    // Each commit is the owner's: durable first, then published by its node
    // while the test holds and releases that publication.
    let commit = async |request: lash_execution_contracts::process::ProcessEventAppendRequest| {
        let event = seeding
            .append_event_for_testing(&process_id, request)
            .await?
            .event;
        let watched = watched.clone();
        let process_id = process_id.clone();
        tokio::spawn(async move {
            watched
                .publish_committed(&process_id, event.sequence - 1)
                .await
        });
        Ok::<_, lash_execution_contracts::plugin_error::PluginError>(())
    };
    commit(
        lash_execution_contracts::process::ProcessEventAppendRequest::wait_entered(
            &process_id,
            &wait,
        ),
    )
    .await?;
    replay.gate.reached.notified().await;
    replay.gate.parked(feed.next()).await;

    // The first publication lands; the second commit is durable with its
    // publication held before the feed reads the process again.
    commit(
        lash_execution_contracts::process::ProcessEventAppendRequest::wait_cleared(
            &process_id,
            &wait,
        ),
    )
    .await?;
    replay.gate.release();
    replay.gate.reached.notified().await;
    replay.gate.release_after_read.store(true, Ordering::SeqCst);

    let mut shown = Vec::new();
    while shown.len() < 2 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(30), feed.next())
            .await
            .expect("the process feed delivers")
            .expect("the process feed stays open")?;
        match item {
            crate::process_feed::ProcessObservationStreamItem::Gap { .. } => {
                shown.push(Shown::Gap);
                break;
            }
            crate::process_feed::ProcessObservationStreamItem::Event(event)
                if matches!(
                    event.payload,
                    lash_core::ProcessObservationEventPayload::Committed { .. }
                ) =>
            {
                shown.push(Shown::Commit);
            }
            crate::process_feed::ProcessObservationStreamItem::Event(_) => {}
        }
    }
    Ok((
        shown,
        replay.facts.load(Ordering::SeqCst) - published_before,
    ))
}

/// One rule closes the window between a commit and its publication for both
/// feeds: a feed that finds its head unbridged waits on the publication its
/// node holds for that head. Across two commits, each still being published
/// when the feed reads the head it made, a session feed and a process feed
/// both show the two commits and no gap, and the process feed publishes
/// nothing itself: it reconciles only a head no publication is held for
/// (FIG-5626).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn session_and_process_feeds_wait_out_the_same_publication_windows() -> Result<()>
{
    let session = session_feed_across_two_publication_windows().await?;
    let (process, published) = process_feed_across_two_publication_windows().await?;
    assert_eq!(session, [Shown::Commit, Shown::Commit]);
    assert_eq!(process, session);
    assert_eq!(
        published, 2,
        "each fact reached the replay store once, from its commit's own publication"
    );
    Ok(())
}
