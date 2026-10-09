//! The process feed's recovery laws, over a real SQLite process registry and
//! the in-memory process replay store.
//!
//! Durable commits reach the replay the way an after-commit publisher
//! delivers them, as `Committed` drafts; a test that withholds that call is
//! a lost or late publication.

use super::*;
use lash_core::testing::{process_language_observation, process_observation_label};
use lash_core::{
    InMemoryProcessReplayStore, InMemoryProcessReplayStoreConfig, ProcessEvent,
    ProcessReplayEventDraft, ProcessReplayGapReason,
};

/// One lifecycle append: the `n`th tick enters a wait on the `n`th call, so
/// every tick is a new event at the next sequence.
fn tick(process_id: &ProcessId, n: u64) -> lash_core::ProcessEventAppendRequest {
    let wait = lash_core::WaitState {
        since_ms: n,
        kind: lash_core::WaitKind::Call {
            call_id: lash_core::ToolCallId::fixture(&format!("feed-call-{n}")),
            tool_id: lash_core::ToolId::from("feed-fixture"),
        },
        site: None,
    };
    lash_core::ProcessEventAppendRequest::wait_entered(process_id, &wait)
}

/// A reconcile cadence no law waits out.
const NO_CADENCE: std::time::Duration = std::time::Duration::from_secs(3600);

struct Fixture {
    /// What wakes this fixture's feeds to look at the durable process:
    /// nothing, unless a law says otherwise.
    reconcile: FeedReconcile,
    /// The hub the fixture's registry ticks at each commit.
    commits: ProcessChangeHub,
    registry: Arc<dyn ProcessRegistry>,
    replay: Arc<InMemoryProcessReplayStore>,
    limits: lash_trace::ObservationWorkLimits,
    process_id: ProcessId,
    authority: lash_core::ProcessExecutionWriteAuthority,
    ticks: std::sync::atomic::AtomicU64,
}

impl Fixture {
    /// A registered, started process: its durable sequence is 1, and
    /// nothing was published.
    async fn new() -> Self {
        Self::with_replay(InMemoryProcessReplayStoreConfig::standard()).await
    }

    /// The same process over a replay store that retains what `config` says.
    async fn with_replay(config: InMemoryProcessReplayStoreConfig) -> Self {
        // Watched, so a commit ticks `commits`, as a core's registry does.
        let watched = lash_core::runtime::watch_process_registry(
            crate::tests::sqlite_memory_store_set()
                .await
                .process_registry(),
        );
        let registry = Arc::clone(watched.registry());
        let commits = watched.hub().clone();
        let process_id = registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    lash_core::testing::held_engine_input(serde_json::Value::Null),
                    lash_core::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_execution_env_ref(Some(
                    lash_core::testing::process_execution_env_fixture_ref(),
                )),
            )
            .await
            .expect("register the feed process")
            .id;
        let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
            process_id.clone(),
            "feed-invocation",
        )
        .bind_attempt(1);
        registry
            .record_first_started_with_authority(
                &process_id,
                authority
                    .invocation_started()
                    .expect("the authority is bound to attempt one"),
                &authority,
            )
            .await
            .expect("record the execution start");
        let replay = Arc::new(InMemoryProcessReplayStore::new(config));
        let publisher = Arc::new(
            crate::language_observation::LanguageObservationPublisher::new(
                replay.clone(),
                Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                    lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                )),
            ),
        );
        Self {
            reconcile: FeedReconcile {
                publisher,
                changes: ProcessChangeHub::new(),
                pacing: PollPacing::new(NO_CADENCE, NO_CADENCE).expect("pacing"),
            },
            commits,
            registry,
            replay,
            limits: lash_trace::ObservationWorkLimits::standard(),
            process_id,
            authority,
            ticks: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn observe(&self) -> ObservableProcess {
        self.observe_process(&self.process_id)
    }

    fn observe_process(&self, process_id: &ProcessId) -> ObservableProcess {
        ObservableProcess {
            source: ProcessFeedSource::new(
                process_id.clone(),
                Arc::clone(&self.registry),
                lash_core::facade_support::ProcessWorkObserver::new(Arc::clone(&self.registry)),
                lash_core::ProcessEngineRegistry::default(),
                self.replay.clone(),
                self.limits,
                self.reconcile.clone(),
            ),
        }
    }

    /// Commit one durable event, unpublished.
    async fn commit(&self) -> ProcessEvent {
        self.registry
            .append_event_with_authority(
                &self.process_id,
                tick(
                    &self.process_id,
                    self.ticks
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ),
                &self.authority,
            )
            .await
            .expect("commit a tick")
            .event
    }

    /// The after-commit publisher delivering `event`.
    async fn publish(&self, event: &ProcessEvent) -> usize {
        self.replay
            .publish(
                &self.process_id,
                vec![ProcessReplayEventDraft::committed(event.clone().into())],
            )
            .await
            .expect("publish a committed fact")
            .len()
    }

    async fn commit_published(&self) -> ProcessEvent {
        let event = self.commit().await;
        assert_eq!(self.publish(&event).await, 1);
        event
    }

    /// A provisional node observation, stamped at `sequence`.
    async fn observe_node(&self, sequence: u64, label: &str) {
        self.replay
            .publish(
                &self.process_id,
                vec![ProcessReplayEventDraft::language_execution(
                    ProcessSequence::new(sequence),
                    process_language_observation(&self.process_id, label, label),
                )],
            )
            .await
            .expect("publish a language observation");
    }
}

async fn next(feed: &mut ProcessObservationStream) -> ProcessObservationStreamItem {
    tokio::time::timeout(std::time::Duration::from_secs(5), feed.next())
        .await
        .expect("an item arrives")
        .expect("the feed continues")
        .expect("the feed reads")
}

async fn quiet(feed: &mut ProcessObservationStream) {
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), feed.next())
            .await
            .is_err(),
        "nothing is pending"
    );
}

#[track_caller]
fn expect_event(item: ProcessObservationStreamItem, expected: &str) {
    match item {
        ProcessObservationStreamItem::Event(event) => {
            assert_eq!(process_observation_label(&event), expected);
        }
        other => panic!("expected event `{expected}`, got {other:?}"),
    }
}

/// The gap's cause, replacement sequence and cursor; the replacement and the
/// gap agree on where the consumer now stands.
#[track_caller]
fn expect_gap(
    item: ProcessObservationStreamItem,
    cause: ProcessObservationGapCause,
) -> (ProcessObservation, ProcessReplayGap) {
    let ProcessObservationStreamItem::Gap { observation, gap } = item else {
        panic!("expected a {cause:?} gap, got {item:?}");
    };
    assert_eq!(gap.cause, cause);
    assert_eq!(gap.latest_sequence, observation.read_view.sequence());
    assert_eq!(gap.latest_cursor, observation.cursor);
    (observation, gap)
}

fn held(feed: &ProcessObservationStream) -> u64 {
    feed.cursor()
        .parse()
        .expect("the feed's cursor parses")
        .sequence
        .as_u64()
}

/// FIG-5629: every process read carries the same durable actor park,
/// including the replacement view after replay continuity is lost.
#[tokio::test]
async fn an_unknown_engine_park_is_shared_by_get_list_snapshot_and_gap() {
    use crate::durable::{ActorKey, CommitLabel, NodeId, NodeSpec};

    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = crate::durable::DurableBackendBuilder::new(stores)
        .build()
        .expect("durable backend with no host engines");
    let process_id = backend
        .process_registry()
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::Engine {
                    kind: "absent-feed-engine".into(),
                    payload: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref())),
        )
        .await
        .expect("register the process")
        .id;
    // Seed the durable park through the actor's fenced transaction. This
    // law tests observation; the actor laws own why an absent engine parks.
    let actor = ActorKey::process(process_id.as_str()).expect("process actor key");
    let durable = backend.durable();
    let row = durable
        .actor(&actor)
        .await
        .expect("read actor")
        .expect("actor");
    let node = durable
        .register_node(&NodeSpec {
            node: NodeId::new("feed-park-fixture"),
            decodes: vec![row.formats],
            ttl_millis: 15_000,
        })
        .await
        .expect("register the actor owner");
    let claims = durable.claim(&node, 1).await.expect("claim process");
    assert_eq!(claims.len(), 1);
    let mut tx = durable
        .begin(&actor, claims[0].epoch)
        .await
        .expect("actor transaction");
    let reason = lash_core::ProcessParkReason::UnknownEngine {
        kind: "absent-feed-engine".into(),
    };
    tx.write(crate::durable::DomainWrite::ParkEvent(
        crate::durable::domain::ParkEventWrite::Park {
            reason_json: reason.encode(),
        },
    ));
    tx.give_up(crate::durable::Release::Parked);
    durable
        .commit(tx, CommitLabel::PROCESS_ADVANCE)
        .await
        .expect("commit the park");

    let core = crate::tests::explicit_ephemeral_facets(crate::LashCore::standard_builder(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core");
    let processes = core.processes();
    let get = processes
        .get(&process_id)
        .await
        .expect("get")
        .expect("retained process");
    assert_eq!(get.park, Some(reason));
    let list = processes
        .list(
            &lash_core::ProcessListFilter::default(),
            std::num::NonZeroUsize::MIN,
            None,
        )
        .await
        .expect("list");
    assert_eq!(list.processes.len(), 1);
    assert_eq!(list.processes[0].park, get.park);

    let observed = processes.observe(&process_id);
    let snapshot = observed.snapshot().await.expect("initial feed snapshot");
    let ProcessReadView::Retained(initial) = &snapshot.read_view else {
        panic!("the parked process is retained");
    };
    assert_eq!(
        initial.process.park, get.park,
        "initial feed snapshot retains the park"
    );
    core.process_replay_store
        .invalidate_all()
        .await
        .expect("lose replay continuity");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let (replacement, _) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::Replay {
            reason: ProcessReplayGapReason::Unavailable,
        },
    );
    let ProcessReadView::Retained(replacement) = replacement.read_view else {
        panic!("the gap replacement retains the process");
    };
    assert_eq!(
        replacement.process.park, get.park,
        "gap replacement retains the park"
    );
    core.shutdown().await.expect("shutdown");
}

/// A late attach replays what the window retains: the provisional node
/// evidence published before the snapshot is delivered, the commits the
/// snapshot already reflects are not, and every commit after it arrives
/// once, in sequence, however often it is republished.
#[tokio::test]
async fn an_attach_replays_the_retained_window_and_delivers_each_later_commit_once() {
    let fixture = Fixture::new().await;
    fixture.observe_node(1, "node before").await;
    let reflected = fixture.commit_published().await;

    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let ProcessReadView::Retained(view) = &snapshot.read_view else {
        panic!("the process is retained");
    };
    assert_eq!(view.process.last_event_sequence, reflected.sequence);
    assert_eq!(
        view.effects.observed_through,
        ProcessSequence::new(reflected.sequence)
    );
    assert_eq!(view.effects.coverage, ProcessEffectCoverage::Complete);

    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    expect_event(next(&mut feed).await, "node before");
    quiet(&mut feed).await;
    assert_eq!(held(&feed), reflected.sequence);

    let later = fixture.commit_published().await;
    fixture.observe_node(0, "node stamped behind").await;
    assert_eq!(
        fixture.publish(&later).await,
        0,
        "a takeover's republication is the same fact"
    );
    expect_event(
        next(&mut feed).await,
        &format!("committed:{}", later.sequence),
    );
    expect_event(next(&mut feed).await, "node stamped behind");
    quiet(&mut feed).await;
    assert_eq!(
        held(&feed),
        later.sequence,
        "a provisional event stamped behind never moves the held sequence back"
    );

    let mut resumed = observed.subscribe_and_recover(feed.cursor().clone());
    quiet(&mut resumed).await;
}

/// A commit whose publication was lost is never skipped silently: the next
/// commit arrives without its bridge, and the feed answers one gap whose
/// replacement is the durable process, then continues from it.
#[tokio::test]
async fn a_commit_without_its_bridge_is_one_gap_with_the_durable_process() {
    let fixture = Fixture::new().await;
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    quiet(&mut feed).await;

    fixture.observe_node(1, "node kept").await;
    expect_event(next(&mut feed).await, "node kept");
    let _lost = fixture.commit().await;
    let unbridged = fixture.commit_published().await;

    let (replacement, gap) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::CommitUnbridged,
    );
    assert_eq!(
        replacement.read_view.sequence(),
        ProcessSequence::new(unbridged.sequence)
    );
    assert_eq!(gap.process_id, fixture.process_id);
    // After a gap the consumer refolds the retained provisional window; the
    // commits the replacement reflects are not delivered again.
    expect_event(next(&mut feed).await, "node kept");
    let after = fixture.commit_published().await;
    expect_event(
        next(&mut feed).await,
        &format!("committed:{}", after.sequence),
    );
    quiet(&mut feed).await;
}

/// A reconnect is judged against the durable process: a cursor continues
/// when the replay holds every commit between it and the process, gaps when
/// one is missing however many later ones are retained, and gaps when it
/// names a sequence the process never reached.
#[tokio::test]
async fn a_stale_cursor_continues_only_across_every_commit_it_missed() {
    let fixture = Fixture::new().await;
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let stale = snapshot.cursor.clone();

    let first = fixture.commit_published().await;
    let second = fixture.commit_published().await;
    let mut bridged = observed.subscribe_and_recover(stale.clone());
    expect_event(
        next(&mut bridged).await,
        &format!("committed:{}", first.sequence),
    );
    expect_event(
        next(&mut bridged).await,
        &format!("committed:{}", second.sequence),
    );

    let _lost = fixture.commit().await;
    let last = fixture.commit_published().await;
    let mut unbridged = observed.subscribe_and_recover(bridged.cursor().clone());
    let (replacement, _) = expect_gap(
        next(&mut unbridged).await,
        ProcessObservationGapCause::CommitUnbridged,
    );
    assert_eq!(
        replacement.read_view.sequence(),
        ProcessSequence::new(last.sequence)
    );
    quiet(&mut unbridged).await;

    let parsed = stale.parse().expect("the snapshot cursor parses");
    let ahead = ProcessObservationCursor::new(
        parsed.replay_incarnation_id,
        &fixture.process_id,
        ProcessSequence::new(last.sequence + 10),
        parsed.live_position,
    );
    let mut ahead = observed.subscribe_and_recover(ahead);
    expect_gap(
        next(&mut ahead).await,
        ProcessObservationGapCause::AheadOfDurableProcess,
    );
}

/// When the replay loses continuity under an open feed (a store-wide
/// invalidation, as a publisher whose ingress overflowed issues), the feed
/// answers exactly one gap with the durable process and then delivers what
/// is published afterwards.
#[tokio::test]
async fn a_replay_that_loses_continuity_is_one_gap_and_the_feed_resumes() {
    let fixture = Fixture::new().await;
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    fixture.observe_node(1, "node lost").await;
    expect_event(next(&mut feed).await, "node lost");

    fixture
        .replay
        .invalidate_all()
        .await
        .expect("invalidate every process");
    let committed = fixture.commit().await;

    let (replacement, _) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::Replay {
            reason: ProcessReplayGapReason::Unavailable,
        },
    );
    assert_eq!(
        replacement.read_view.sequence(),
        ProcessSequence::new(committed.sequence),
        "the replacement holds the commit whose publication the replay lost"
    );
    quiet(&mut feed).await;
    fixture.observe_node(committed.sequence, "node after").await;
    expect_event(next(&mut feed).await, "node after");
    quiet(&mut feed).await;
}

/// A cursor names one process. Presenting it to another is a request error,
/// never a feed of the wrong process or a retargeted snapshot.
#[tokio::test]
async fn a_cursor_for_another_process_is_refused() {
    let fixture = Fixture::new().await;
    let snapshot = fixture.observe().snapshot().await.expect("snapshot");
    let other = fixture.observe_process(&ProcessId::fixture("another-process"));
    let mut feed = other.subscribe_and_recover(snapshot.cursor);
    let error = feed
        .next()
        .await
        .expect("the feed answers")
        .expect_err("a cursor for another process is refused");
    assert!(
        error.to_string().contains("belongs to"),
        "expected the wrong-process refusal, got {error}"
    );
    assert!(feed.next().await.is_none(), "a refused feed ends");
}

/// An id no process is retained under is typed absence, not an empty
/// process at sequence zero: the snapshot says so, and a feed answers one
/// gap that says so and ends.
#[tokio::test]
async fn a_process_that_is_not_retained_is_typed_absence_and_ends_the_feed() {
    let fixture = Fixture::new().await;
    let unknown = fixture.observe_process(&ProcessId::fixture("never-registered"));
    let snapshot = unknown.snapshot().await.expect("snapshot");
    assert!(matches!(snapshot.read_view, ProcessReadView::Unknown));

    let mut feed = unknown.subscribe_and_recover(snapshot.cursor);
    let (replacement, _) = expect_gap(
        next(&mut feed).await,
        ProcessObservationGapCause::NotRetained,
    );
    assert!(matches!(replacement.read_view, ProcessReadView::Unknown));
    assert!(feed.next().await.is_none(), "the feed ends");
}

/// FIG-5624: a snapshot of an id no process is retained under takes no
/// replay window, so it cannot evict the window of a followed process.
#[tokio::test]
async fn a_snapshot_of_an_unretained_id_takes_no_replay_window() {
    let fixture = Fixture::with_replay(InMemoryProcessReplayStoreConfig {
        max_processes: 1,
        ..InMemoryProcessReplayStoreConfig::standard()
    })
    .await;
    let observed = fixture.observe();
    let snapshot = observed.snapshot().await.expect("snapshot");
    fixture.observe_node(1, "node kept").await;

    let unknown = fixture
        .observe_process(&ProcessId::fixture("never-registered"))
        .snapshot()
        .await
        .expect("snapshot of an unknown id");
    assert!(matches!(unknown.read_view, ProcessReadView::Unknown));

    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    expect_event(next(&mut feed).await, "node kept");
    quiet(&mut feed).await;
}

#[path = "reconcile_tests.rs"]
mod reconcile;
