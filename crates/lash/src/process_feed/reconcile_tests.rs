//! FIG-5549: an open feed converges on the durable process whoever
//! committed and whatever became of the commit's publication.

use super::*;

use std::time::Duration;

use lash_core::StoreSet as _;

fn committed_sequence(item: ProcessObservationStreamItem) -> u64 {
    match item {
        ProcessObservationStreamItem::Event(event) => match &event.payload {
            ProcessObservationEventPayload::Committed { event } => event.sequence,
            other => panic!("expected a committed fact, got {other:?}"),
        },
        ProcessObservationStreamItem::Gap { replacement, .. } => {
            panic!("unexpected gap {replacement:?}")
        }
    }
}

/// The last after-commit publication is dropped: the commit never reaches
/// the replay store. An idle feed, already attached, still delivers the
/// fact itself, woken by the commit's tick alone; it has no cadence.
#[tokio::test]
async fn an_idle_follower_recovers_a_commit_whose_publication_was_dropped() {
    let mut fixture = Fixture::new().await;
    fixture.reconcile.changes = fixture.commits.clone();
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    quiet(&mut feed).await;

    let dropped = fixture.commit().await;
    assert_eq!(committed_sequence(next(&mut feed).await), dropped.sequence);
    assert_eq!(held(&feed), dropped.sequence);

    // The publication arriving late after all is a redelivery, not a second
    // fact, and the next published commit follows in order.
    assert_eq!(fixture.publish(&dropped).await, 0);
    let after = fixture.commit_published().await;
    assert_eq!(committed_sequence(next(&mut feed).await), after.sequence);
}

/// Facts the feed cannot read any more are a gap with the durable process,
/// never a silent skip: the host released the events the follower missed
/// before anything woke it.
#[tokio::test]
async fn a_follower_whose_missed_facts_were_released_gets_one_gap() {
    let mut fixture = Fixture::new().await;
    // A hub only this law ticks, so the feed looks after the release.
    let wake = ProcessChangeHub::new();
    fixture.reconcile.changes = wake.clone();
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    quiet(&mut feed).await;

    fixture.commit().await;
    let last = fixture.commit().await;
    lash_core::ProcessRetention::release_process_events(
        fixture.registry.as_ref(),
        &fixture.process_id,
        last.sequence,
    )
    .await
    .expect("release the missed events");
    wake.notify(&fixture.process_id);

    let (observation, _) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::CommitUnbridged,
    );
    assert_eq!(
        observation.read_view.sequence().expect("retained").as_u64(),
        last.sequence
    );
    assert_eq!(held(&feed), last.sequence);
}

/// FIG-5624: a consumer further behind than the bridge bound rebuilds from
/// the durable process at once. The feed republishes none of the facts it
/// missed, so the window other observers fold keeps what it held.
#[tokio::test]
async fn a_follower_behind_the_bridge_bound_gaps_and_republishes_nothing() {
    let mut fixture = Fixture::new().await;
    fixture.limits.process_reconcile_bridge_events = 2;
    let wake = ProcessChangeHub::new();
    fixture.reconcile.changes = wake.clone();
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let window = snapshot.cursor.clone();
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    fixture.observe_node(1, "node kept").await;
    expect_event(next(&mut feed).await, "node kept");

    fixture.commit().await;
    fixture.commit().await;
    let last = fixture.commit().await;
    wake.notify(&fixture.process_id);

    let (replacement, _) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::CommitUnbridged,
    );
    assert_eq!(
        replacement.read_view.sequence().expect("retained").as_u64(),
        last.sequence
    );
    let lash_core::ProcessReplayOutcome::Replayed(retained) = fixture
        .replay
        .replay_after_cursor(&window)
        .await
        .expect("read the window")
    else {
        panic!("the window keeps its continuity");
    };
    let retained: Vec<_> = retained
        .iter()
        .map(|event| process_observation_label(event))
        .collect();
    assert_eq!(retained, ["node kept"], "the feed published nothing");
}

/// One node of a SQLite database file: a core of its own over a store set
/// of its own.
async fn core_on(
    database: &std::path::Path,
    node: &str,
) -> (Arc<lash_sqlite_store::SqliteStoreSet>, crate::LashCore) {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            database,
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open the file store set"),
    );
    let core =
        crate::tests::standard_core_builder_over(lash_conformance::backend_over(stores.clone()))
            .build(lash_core::LeaseOwnerIdentity::opaque(
                lash_core::LeaseOwnerId::new(node),
                lash_core::LeaseIncarnationId::new(format!("{node}-boot")),
            ))
            .expect("standard core");
    (stores, core)
}

/// Two cores serve one SQLite file as two nodes, each with a replay store of
/// its own. Core A follows a process it executes nothing of; core B commits
/// the process's terminal. A's feed delivers the terminal fact, with no host
/// timer and no local trace: a feed has no cadence, so it came by B's node
/// wake.
#[tokio::test]
async fn a_follower_on_one_core_observes_the_terminal_another_core_commits() {
    let dir = tempfile::tempdir().expect("two-core tempdir");
    let database = dir.path().join("lash.db");
    let (stores, core_a) = core_on(&database, "fig-5549-follower").await;
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let (_stores_b, core_b) = core_on(&database, "fig-5549-committer").await;
    // Both nodes listen before the commit: each holds its boot's liveness
    // lock once its listener is open.
    let node_wakes = stores.node_wakes().expect("a SQLite file has node wakes");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let boots = node_wakes.liveness().await.expect("probe liveness");
            if boots.iter().filter(|boot| boot.held).count() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both nodes listen");

    let committer = core_b.process_registry.clone();
    let process_id = committer
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::testing::held_engine_input(serde_json::Value::Null),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await
        .expect("register on core B")
        .id;

    let observed = core_a.processes().observe(&process_id);
    let snapshot = observed.snapshot().await.expect("snapshot on core A");
    let mut sequence = snapshot.read_view.sequence().expect("retained").as_u64();
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    quiet(&mut feed).await;

    let terminal = committer
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete on core B");
    loop {
        let ProcessObservationStreamItem::Event(event) = next(&mut feed).await else {
            panic!("the durable feed never gaps here");
        };
        let ProcessObservationEventPayload::Committed { event: fact } = &event.payload else {
            panic!("core A publishes no trace of the process: {event:?}");
        };
        assert_eq!(fact.sequence, sequence + 1, "every commit, in order");
        sequence = fact.sequence;
        if matches!(fact.fact, lash_core::ProcessLifecycleFact::Terminal { .. }) {
            break;
        }
    }
    assert_eq!(sequence, terminal.last_event_sequence);
}

/// A replay store that counts the drafts handed to it and, while held,
/// keeps every publication waiting: a store slower than a task wake.
struct HeldReplay {
    inner: InMemoryProcessReplayStore,
    drafts: std::sync::atomic::AtomicUsize,
    open: tokio::sync::watch::Sender<bool>,
}

#[async_trait::async_trait]
impl ProcessReplayStore for HeldReplay {
    async fn publish(
        &self,
        process_id: &ProcessId,
        events: Vec<ProcessReplayEventDraft>,
    ) -> std::result::Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError> {
        self.drafts
            .fetch_add(events.len(), std::sync::atomic::Ordering::SeqCst);
        let _ = self.open.subscribe().wait_for(|open| *open).await;
        self.inner.publish(process_id, events).await
    }

    async fn replay_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> std::result::Result<lash_core::ProcessReplayOutcome, ProcessReplayStoreError> {
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> std::result::Result<ProcessReplaySubscribeOutcome, ProcessReplayStoreError> {
        self.inner.subscribe_after_cursor(cursor).await
    }

    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> std::result::Result<ProcessObservationCursor, ProcessReplayStoreError> {
        self.inner.earliest_cursor(process_id, sequence).await
    }

    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> std::result::Result<(), ProcessReplayStoreError> {
        self.inner.invalidate_process(process_id).await
    }

    async fn invalidate_all(&self) -> std::result::Result<(), ProcessReplayStoreError> {
        self.inner.invalidate_all().await
    }
}

/// FIG-5625: a commit made on a feed's own node is published to the replay
/// store once, however many feeds are open and however slow the store. The
/// feeds are ticked only once the dispatcher holds the commit, wait for its
/// publication and take the fact from the live tail: none of them reads the
/// durable log to publish it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_local_commit_is_published_once_however_many_feeds_are_open() {
    const FEEDS: usize = 8;
    let replay = Arc::new(HeldReplay {
        inner: InMemoryProcessReplayStore::new(InMemoryProcessReplayStoreConfig::standard()),
        drafts: std::sync::atomic::AtomicUsize::new(0),
        open: tokio::sync::watch::channel(true).0,
    });
    let stores = crate::tests::sqlite_memory_store_set().await;
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let core = crate::tests::standard_core_builder_over(lash_conformance::backend_over(stores))
        .process_replay_store(replay.clone())
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core");
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
        .await
        .expect("register the process")
        .id;

    let observed = core.processes().observe(&process_id);
    let (items, mut delivered) = tokio::sync::mpsc::unbounded_channel();
    for feed in 0..FEEDS {
        let snapshot = observed.snapshot().await.expect("snapshot");
        let mut stream = observed.subscribe_and_recover(snapshot.cursor);
        let items = items.clone();
        tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                if items.send((feed, item.expect("the feed reads"))).is_err() {
                    break;
                }
            }
        });
    }
    // Every feed is subscribed once it delivered this provisional event.
    replay
        .inner
        .publish(
            &process_id,
            vec![ProcessReplayEventDraft::language_execution(
                ProcessSequence::new(0),
                lash_core::testing::process_language_observation(&process_id, "open", "open"),
            )],
        )
        .await
        .expect("publish a provisional event");
    let mut next = async |what: &str| {
        tokio::time::timeout(Duration::from_secs(30), delivered.recv())
            .await
            .unwrap_or_else(|_| panic!("every feed delivers {what}"))
            .expect("the feeds are open")
    };
    for _ in 0..FEEDS {
        let (_, ProcessObservationStreamItem::Event(_)) = next("the provisional event").await
        else {
            panic!("an attached feed never gaps here");
        };
    }

    let held = observed
        .snapshot()
        .await
        .expect("snapshot")
        .read_view
        .sequence()
        .expect("retained process")
        .as_u64();
    let before = replay.drafts.load(std::sync::atomic::Ordering::SeqCst);
    replay.open.send_replace(false);
    let terminal = registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete the process");
    // Long enough for every ticked feed to act on a store that has not
    // published the commit yet.
    tokio::time::sleep(Duration::from_millis(300)).await;
    replay.open.send_replace(true);

    let mut finished = 0;
    while finished < FEEDS {
        let (feed, item) = next("the committed facts").await;
        let ProcessObservationStreamItem::Event(event) = item else {
            panic!("feed {feed} gapped on a commit of its own node");
        };
        if let ProcessObservationEventPayload::Committed { event: fact } = &event.payload
            && fact.sequence == terminal.last_event_sequence
        {
            finished += 1;
        }
    }
    // A republication would be queued behind the commit's own by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        replay.drafts.load(std::sync::atomic::Ordering::SeqCst) - before,
        (terminal.last_event_sequence - held) as usize,
        "each committed fact is handed to the replay store once"
    );
    core.shutdown().await.expect("stop the core");
}
