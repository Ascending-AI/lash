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
        session: &lash_core::SessionId,
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
            }
        }
        self.inner.publish(session, revision, events).await
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
        let outcome = self.inner.subscribe_after_cursor(cursor).await;
        self.read_by(cursor);
        outcome
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
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("publication-window").expect("nonblank host identity"))
        .created()
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
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(name).expect("nonblank host identity"))
        .created()
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
        process_id: &lash_core::ProcessId,
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
        process_id: &lash_core::ProcessId,
        sequence: lash_core::ProcessSequence,
    ) -> std::result::Result<lash_core::ProcessObservationCursor, lash_core::ProcessReplayStoreError>
    {
        self.inner.earliest_cursor(process_id, sequence).await
    }

    async fn invalidate_process(
        &self,
        process_id: &lash_core::ProcessId,
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
    let core = standard_core_builder_over(lash_conformance::backend_over(
        sqlite_memory_store_set().await,
    ))
    .process_replay_store(replay.clone())
    .build(crate::testing::runtime_lease_owner())?;
    let registry = core.process_registry.clone();
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await?
        .id;
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "two-windows-invocation",
    )
    .bind_attempt(1);
    let observed = core.processes().observe(&process_id);
    let snapshot = observed.snapshot().await?;
    let base = snapshot.read_view.sequence().expect("retained").as_u64();
    let published_before = replay.facts.load(Ordering::SeqCst);
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);

    // The first commit, its publication held.
    replay
        .armed
        .lock()
        .expect("armed sequences")
        .extend([base + 1, base + 2]);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority
                .invocation_started()
                .expect("the authority is bound to attempt one"),
            &authority,
        )
        .await?;
    replay.gate.reached.notified().await;
    replay.gate.parked(feed.next()).await;

    // The first publication lands; the second commit is durable with its
    // publication held before the feed reads the process again.
    let wait = lash_core::WaitState {
        since_ms: 0,
        kind: lash_core::WaitKind::Call {
            call_id: lash_core::ToolCallId::fixture("two-windows-call"),
            tool_id: lash_core::ToolId::from("two-windows-fixture"),
        },
        site: None,
    };
    registry
        .append_event_with_authority(
            &process_id,
            lash_core::ProcessEventAppendRequest::wait_entered(&process_id, &wait),
            &authority,
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
