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
        ProcessObservationStreamItem::Gap { gap, .. } => panic!("unexpected gap {gap:?}"),
    }
}

/// The last after-commit publication is dropped: the commit never reaches
/// the replay store. An idle feed, already attached, still delivers the
/// fact itself, woken by the commit's tick alone; its cadence is an hour.
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

/// The wake is lost as well: nothing ticks the feed. It finds the commit on
/// its reconcile cadence.
#[tokio::test]
async fn an_idle_follower_recovers_on_its_cadence_when_the_wake_is_lost_too() {
    let mut fixture = Fixture::new().await;
    fixture.reconcile.pacing =
        PollPacing::new(Duration::from_millis(10), Duration::from_millis(10)).expect("pacing");
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);

    let first = fixture.commit().await;
    let second = fixture.commit().await;
    assert_eq!(committed_sequence(next(&mut feed).await), first.sequence);
    assert_eq!(committed_sequence(next(&mut feed).await), second.sequence);
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

    let ProcessObservationStreamItem::Gap { observation, gap } = next(&mut feed).await else {
        panic!("released facts are a gap");
    };
    assert!(matches!(
        gap.cause,
        ProcessObservationGapCause::CommitUnbridged
    ));
    assert_eq!(gap.latest_sequence.as_u64(), last.sequence);
    assert_eq!(observation.read_view.sequence().as_u64(), last.sequence);
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
    assert_eq!(replacement.read_view.sequence().as_u64(), last.sequence);
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
/// of its own, whose feeds reconcile without a tick only every hour.
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
            .observer_pacing(crate::ObserverPacing {
                process_reconcile: PollPacing::new(NO_CADENCE, NO_CADENCE).expect("pacing"),
                ..crate::ObserverPacing::standard()
            })
            .build(lash_core::LeaseOwnerIdentity::opaque(
                node,
                format!("{node}-boot"),
            ))
            .expect("standard core");
    (stores, core)
}

/// Two cores serve one SQLite file as two nodes, each with a replay store of
/// its own. Core A follows a process it executes nothing of; core B commits
/// the process's terminal. A's feed delivers the terminal fact, with no host
/// timer and no local trace: its own cadence is an hour, so it came by B's
/// node wake.
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
    let mut sequence = snapshot.read_view.sequence().as_u64();
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
