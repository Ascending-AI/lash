//! A session feed across an owner takeover and a live replay discontinuity
//! (FIG-5366).
//!
//! - **Takeover:** node A streams a turn's model call and dies before the
//!   call's answer commits with the turn; node B reaps it and re-sends the pinned call
//!   as its next attempt (ADR 0132 §4). A host's feed opened before the turn
//!   and one opened after A died both follow the turn to its commit without
//!   a replay gap: the live stream retracts A's partial prose with one
//!   `ModelAttemptReset` before B's attempt streams, so the text a follower
//!   holds is B's answer alone.
//! - **Discontinuity:** a feed whose live replay is invalidated reports one
//!   gap and follows on from a cursor the replay continues, even when the
//!   snapshot the gap carries trails the discontinuity.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::observe::{SessionObservationStreamItem, Stream as _};
use lash_core::llm::types::{LlmRequest, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{
    LiveReplayEventDraft, LiveReplayStore, SessionObservationEvent, SessionObservationEventPayload,
    SessionRevision, TurnActivity, TurnActivityId, TurnEvent,
};
use lash_durable::{ActorKey, ActorState, CommitLabel};
use lash_durable_test::{Fault, Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

use dialect::Dialect;

/// How long a feed may take to yield what is already on the live stream.
const PUBLISHED_WITHIN: Duration = Duration::from_secs(10);

const SESSION: &str = "live-stream-takeover";
/// What A streams before it dies.
const ABANDONED: &str = "the abandoned attempt's answer";
/// What B's re-sent attempt answers.
const RESENT: &str = "the re-sent attempt's answer";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

/// The model: each attempt streams its answer as one delta, A's the
/// abandoned answer, every later one the re-sent answer.
fn model(attempts: Arc<AtomicUsize>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("live-stream-takeover")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                let text = if attempt == 0 { ABANDONED } else { RESENT };
                Ok(served::text(&request, text))
            }
        })
        .build()
        .into_handle()
}

/// Everything a feed yields, kept as it arrives.
#[derive(Clone, Default)]
struct Feed(Arc<Mutex<Vec<SessionObservationStreamItem>>>);

impl Feed {
    fn follow(mut stream: lash::observe::SessionObservationStream) -> Self {
        let feed = Self::default();
        let kept = feed.clone();
        tokio::spawn(async move {
            while let Some(Ok(item)) =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream).poll_next(cx)).await
            {
                kept.0.lock_recover().push(item);
            }
        });
        feed
    }

    fn items(&self) -> Vec<SessionObservationStreamItem> {
        self.0.lock_recover().clone()
    }

    fn events(&self) -> Vec<Arc<SessionObservationEvent>> {
        self.items()
            .into_iter()
            .filter_map(|item| match item {
                SessionObservationStreamItem::Event(event) => Some(event),
                SessionObservationStreamItem::Gap { .. } => None,
            })
            .collect()
    }

    fn gaps(&self) -> usize {
        self.items()
            .iter()
            .filter(|item| matches!(item, SessionObservationStreamItem::Gap { .. }))
            .count()
    }

    /// Wait until `done` holds of what the feed yielded.
    async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        tokio::time::timeout(PUBLISHED_WITHIN, async {
            while !done(self) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}: the feed yielded {:#?}", self.items()));
    }
}

fn committed_past(feed: &Feed, revision: SessionRevision) -> bool {
    feed.events().iter().any(|event| {
        matches!(
            event.payload,
            SessionObservationEventPayload::Committed { .. }
        ) && event.revision() > revision
    })
}

/// The prose a follower holds once it applies every `ModelAttemptReset`:
/// each delta's text, less those a later reset retracts.
fn followed_prose(events: &[Arc<SessionObservationEvent>]) -> String {
    let mut deltas: Vec<(TurnActivityId, String)> = Vec::new();
    for event in events {
        let SessionObservationEventPayload::TurnActivity(activity) = &event.payload else {
            continue;
        };
        match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. } => {
                deltas.push((activity.correlation_id.clone(), text.to_string()));
            }
            TurnEvent::ModelAttemptReset {
                assistant_prose_correlation_ids,
                ..
            } => deltas.retain(|(id, _)| !assistant_prose_correlation_ids.contains(id)),
            _ => {}
        }
    }
    deltas.into_iter().map(|(_, text)| text).collect()
}

fn streamed(events: &[Arc<SessionObservationEvent>], text: &str) -> bool {
    events.iter().any(|event| {
        matches!(
            &event.payload,
            SessionObservationEventPayload::TurnActivity(TurnActivity {
                event: TurnEvent::AssistantProseDelta { text: streamed, .. },
                ..
            }) if &**streamed == text
        )
    })
}

/// A streams the call and dies before its answer commits with the turn; B
/// re-sends it.
/// Both host feeds, one opened before the turn and one after A died, follow
/// the turn to its commit without a gap, and hold B's answer alone.
async fn a_takeover_resend_streams_on_without_a_gap(
    dialect: Dialect,
    postgres_url: Option<String>,
) {
    let clock = SimClock::new();
    let keep = Mutex::new(Vec::new());
    let (stores, database) =
        dialect::open(dialect, postgres_url.as_deref(), Arc::clone(&clock), &keep).await;
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(model(Arc::clone(&attempts)), served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "live-stream-takeover",
            "live-stream-takeover-boot",
        ))
        .expect("the core builds");
    let script = Script::new();
    script.cut_on("a", CommitLabel::TURN_COMMIT, 1, Fault::Abort);
    let nodes = SimNodes::new(
        database,
        Arc::clone(&clock),
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            max_active: 4,
        },
        Arc::new(SessionActivation::new(
            backend.clone(),
            lash::testing::session_turn_services(&core),
            Arc::new(Tripwire::new()) as _,
        )),
    );

    let durable = core
        .session(session())
        .create(lash::SessionCreation::root(served::spec(4)))
        .await
        .expect("the session is created");
    let observed = core
        .session(session())
        .open()
        .await
        .expect("the session opens for observation");
    let snapshot = observed
        .observe()
        .snapshot()
        .await
        .expect("the session's snapshot");
    let opened_at = snapshot
        .cursor
        .parse()
        .expect("a store cursor parses")
        .revision;
    let before = Feed::follow(observed.observe().subscribe_and_recover(snapshot.cursor));
    durable
        .send(lash::TurnInput::text("answer"))
        .await
        .expect("the turn's input is sent");

    nodes.start("a");
    nodes.quiesce().await;
    let horizon = clock.logical_ms() + 600_000;
    while nodes.script().cuts().is_empty() {
        let stepped = nodes.step().await;
        assert!(
            clock.logical_ms() < horizon
                && (stepped.is_some() || !nodes.script().cuts().is_empty()),
            "A stalled before its turn's commit:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.kill("a");
    nodes.quiesce().await;
    before
        .until("the before feed carries A's partial answer", |feed| {
            streamed(&feed.events(), ABANDONED)
        })
        .await;
    let snapshot = observed
        .observe()
        .snapshot()
        .await
        .expect("the session's snapshot after A died");
    let mid = Feed::follow(observed.observe().subscribe_and_recover(snapshot.cursor));

    nodes.start("b");
    let actor = ActorKey::session(SESSION).unwrap();
    let horizon = clock.logical_ms() + 600_000;
    while !matches!(
        nodes.database().actor(&actor).await,
        Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
    ) {
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "B did not finish the turn:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "A's attempt and B's re-sent one"
    );

    for (name, feed) in [("before", &before), ("mid", &mid)] {
        feed.until(
            &format!("the {name} feed reaches the turn's commit"),
            |feed| committed_past(feed, opened_at),
        )
        .await;
        assert_eq!(
            feed.gaps(),
            0,
            "the {name} feed gapped across the takeover: {:#?}",
            feed.items()
        );
        let events = feed.events();
        assert!(
            streamed(&events, RESENT),
            "the {name} feed carries B's re-sent attempt: {events:#?}"
        );
        assert_eq!(
            followed_prose(&events),
            RESENT,
            "the {name} feed's follower holds B's answer alone"
        );
    }
    nodes.kill("b");
    drop(keep);
}

#[tokio::test]
async fn a_takeover_resend_streams_on_without_a_gap_on_sqlite_memory() {
    a_takeover_resend_streams_on_without_a_gap(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn a_takeover_resend_streams_on_without_a_gap_on_sqlite_file() {
    a_takeover_resend_streams_on_without_a_gap(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn a_takeover_resend_streams_on_without_a_gap_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    a_takeover_resend_streams_on_without_a_gap(Dialect::Postgres, Some(url)).await;
}

fn activity(label: &str) -> LiveReplayEventDraft {
    LiveReplayEventDraft::new(
        None::<lash_core::TurnId>,
        SessionObservationEventPayload::TurnActivity(TurnActivity::independent(
            TurnEvent::AssistantProseDelta {
                text: label.into(),
                block: StreamBlockIdentity::new("text:0", 0),
            },
        )),
    )
}

/// Another process's activity reaches the feed past the snapshot the
/// session's resident runtime holds, and the live replay is invalidated
/// after it: the feed reports one gap and follows on to the next event,
/// rather than gapping again from the resident's stale cursor.
async fn an_invalidated_feed_gaps_once_and_follows_on(tier: served::Tier) {
    let store: Arc<dyn LiveReplayStore> =
        Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::default());
    let shared = Arc::clone(&store);
    let Some(world) = served::World::new(tier, move |backend| {
        lash::LashCore::standard_builder(backend.clone()).live_replay_store(shared)
    })
    .await
    else {
        return;
    };
    let name = "invalidated-feed";
    let _durable = world.session(name, served::spec(4)).await;
    let session_id = SessionId::try_from(name.to_owned()).unwrap();
    let observed = world
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("the session opens for observation");
    let snapshot = observed
        .observe()
        .snapshot()
        .await
        .expect("the session's snapshot");
    let revision = snapshot.cursor.parse().expect("a cursor parses").revision;
    let feed = Feed::follow(observed.observe().subscribe_and_recover(snapshot.cursor));
    for label in ["elsewhere one", "elsewhere two"] {
        store
            .publish(&session_id, revision, vec![activity(label)])
            .await
            .expect("another process publishes");
    }
    feed.until("the feed follows the other process", |feed| {
        feed.events().len() == 2
    })
    .await;
    store
        .invalidate_session(&session_id)
        .await
        .expect("the live replay is invalidated");
    feed.until("the feed reports the discontinuity", |feed| feed.gaps() > 0)
        .await;
    store
        .publish(&session_id, revision, vec![activity("after")])
        .await
        .expect("publish after the discontinuity");
    feed.until("the feed follows on past the gap", |feed| {
        streamed(&feed.events(), "after")
    })
    .await;
    assert_eq!(
        feed.gaps(),
        1,
        "one discontinuity is one gap: {:#?}",
        feed.items()
    );
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalidated_feed_gaps_once_and_follows_on_on_sqlite_memory() {
    an_invalidated_feed_gaps_once_and_follows_on(served::Tier::SqliteMemory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalidated_feed_gaps_once_and_follows_on_on_postgres() {
    an_invalidated_feed_gaps_once_and_follows_on(served::Tier::Postgres).await;
}
