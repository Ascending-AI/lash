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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::observe::{SessionObservationStreamItem, Stream as _};
use lash_core::facade_support::{InMemoryLiveReplayStore, InMemoryLiveReplayStoreConfig};
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmStreamEvent, StreamBlockIdentity};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{
    LiveReplayEventDraft, LiveReplayGapReason, LiveReplayOutcome, LiveReplayStore,
    LiveReplayStoreError, LiveReplaySubscribeOutcome, SessionCursor, SessionObservationEvent,
    SessionObservationEventPayload, SessionRevision, TurnActivity, TurnActivityId, TurnEvent,
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

/// One session served by sim nodes over `dialect`'s database, its turn's
/// model streaming [`ABANDONED`] on A's attempt and [`RESENT`] on every
/// later one.
struct Takeover {
    clock: Arc<SimClock>,
    nodes: SimNodes,
    durable: lash::DurableSession,
    observed: lash::LashSession,
    attempts: Arc<AtomicUsize>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Takeover {
    /// The core publishes to `live`, or to the live replay store it builds
    /// itself.
    async fn new(
        dialect: Dialect,
        postgres_url: Option<String>,
        live: Option<Arc<dyn LiveReplayStore>>,
    ) -> Self {
        let clock = SimClock::new();
        let keep = Mutex::new(Vec::new());
        let (stores, database) =
            dialect::open(dialect, postgres_url.as_deref(), Arc::clone(&clock), &keep).await;
        let backend = served::configured_backend(stores, sim::settings(), Vec::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut builder = lash::LashCore::standard_builder(backend.clone())
            .serve_sessions(false)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .serve_test_llm_profile(model(Arc::clone(&attempts)), served::metadata());
        if let Some(live) = live {
            builder = builder.live_replay_store(live);
        }
        let core = builder
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
        Self {
            clock,
            nodes,
            durable,
            observed,
            attempts,
            keep,
        }
    }

    /// A feed from the session's snapshot now, and the revision it opened at.
    async fn follow(&self) -> (Feed, SessionRevision) {
        let snapshot = self
            .observed
            .observe()
            .snapshot()
            .await
            .expect("the session's snapshot");
        let revision = snapshot
            .cursor
            .parse()
            .expect("a store cursor parses")
            .revision;
        (
            Feed::follow(
                self.observed
                    .observe()
                    .subscribe_and_recover(snapshot.cursor),
            ),
            revision,
        )
    }

    /// Send the turn's input and run A until it dies before the turn's
    /// commit, `follower` holding A's partial answer.
    async fn a_streams_and_dies(&self, follower: &Feed) {
        self.durable
            .send(lash::TurnInput::text("answer"))
            .await
            .expect("the turn's input is sent");
        self.nodes.start("a");
        self.nodes.quiesce().await;
        let horizon = self.clock.logical_ms() + 600_000;
        while self.nodes.script().cuts().is_empty() {
            let stepped = self.nodes.step().await;
            assert!(
                self.clock.logical_ms() < horizon
                    && (stepped.is_some() || !self.nodes.script().cuts().is_empty()),
                "A stalled before its turn's commit:\n{}",
                self.nodes.script().rendered_trace()
            );
        }
        self.nodes.kill("a");
        self.nodes.quiesce().await;
        follower
            .until("the follower carries A's partial answer", |feed| {
                streamed(&feed.events(), ABANDONED)
            })
            .await;
    }

    /// Run B until it has re-sent the call and committed the turn.
    async fn b_finishes(&self) {
        self.nodes.start("b");
        let actor = ActorKey::session(SESSION).unwrap();
        let horizon = self.clock.logical_ms() + 600_000;
        while !matches!(
            self.nodes.database().actor(&actor).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        ) {
            assert!(
                self.clock.logical_ms() < horizon && self.nodes.step().await.is_some(),
                "B did not finish the turn:\n{}",
                self.nodes.script().rendered_trace()
            );
        }
        self.nodes.quiesce().await;
        assert_eq!(
            self.attempts.load(Ordering::SeqCst),
            2,
            "A's attempt and B's re-sent one"
        );
    }

    fn end(self) {
        self.nodes.kill("b");
        drop(self.keep);
    }
}

/// FIG-5390: provider block ids are local to a response attempt. A reset
/// in the second call must leave the first call's text on the follower.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_model_call_reset_preserves_the_first_calls_blocks_on_sqlite_memory() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let model = lash_core::testing::TestProvider::builder()
        .kind("reused-provider-blocks")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let call = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                let stream = request.stream_events.as_ref().expect("the call streams");
                let block = StreamBlockIdentity::new("content_block:0", 0);
                let delta = |text: &str| LlmStreamEvent::Delta {
                    block: block.clone(),
                    text: text.to_owned(),
                };
                let text = if call == 0 { "Let me check." } else { "Done." };
                if call == 1 {
                    stream.send(delta("Partial"));
                    stream.send(LlmStreamEvent::AttemptReset);
                }
                stream.send(delta(text));
                let mut response = served::response(vec![LlmOutputPart::Text {
                    text: text.to_owned(),
                    response_meta: None,
                }]);
                if call == 0 {
                    response.parts.push(served::call(
                        "check",
                        "echo_tool",
                        serde_json::json!({ "value": "check" }),
                    ));
                }
                Ok(response)
            }
        })
        .build()
        .into_handle();
    let world =
        served::World::with_model(served::Tier::SqliteMemory, Vec::new(), model, |backend| {
            lash::LashCore::standard_builder(backend.clone())
                .tools(Arc::new(lash_core::testing::runtime_helpers::EchoTool))
        })
        .await
        .expect("the SQLite world opens");
    let name = "two-calls-reused-blocks";
    let durable = world.session(name, served::spec(4)).await;
    let observed = world
        .core
        .session(durable.session_id().clone())
        .open()
        .await
        .unwrap();
    let snapshot = observed.observe().snapshot().await.unwrap();
    let revision = snapshot.cursor.parse().unwrap().revision;
    let feed = Feed::follow(observed.observe().subscribe_and_recover(snapshot.cursor));
    let output = world.send(&durable, "answer after checking").await;
    served::assert_answered(name, &output);
    feed.until("the feed reaches the two-call turn's commit", |feed| {
        committed_past(feed, revision)
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let events = feed.events();
    assert!(streamed(&events, "Partial"));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                SessionObservationEventPayload::TurnActivity(TurnActivity {
                    event: TurnEvent::ModelAttemptReset { .. },
                    ..
                })
            ))
            .count(),
        1,
        "the second call retracts its failed attempt"
    );
    assert_eq!(followed_prose(&events), "Let me check.Done.");
    world.shutdown().await;
}

/// A streams the call and dies before its answer commits with the turn; B
/// re-sends it.
/// Both host feeds, one opened before the turn and one after A died, follow
/// the turn to its commit without a gap, and hold B's answer alone.
async fn a_takeover_resend_streams_on_without_a_gap(
    dialect: Dialect,
    postgres_url: Option<String>,
) {
    let takeover = Takeover::new(dialect, postgres_url, None).await;
    let (before, opened_at) = takeover.follow().await;
    takeover.a_streams_and_dies(&before).await;
    let (mid, _) = takeover.follow().await;
    takeover.b_finishes().await;

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
    takeover.end();
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

/// The prose a follower holds at the end of `items`: each delta's text
/// since its last gap, less those a later reset retracts. A gap restarts it
/// from the session's snapshot, which holds no uncommitted text.
fn held_prose(items: &[SessionObservationStreamItem]) -> String {
    let since_gap = items
        .iter()
        .rposition(|item| matches!(item, SessionObservationStreamItem::Gap { .. }))
        .map_or(items, |gap| &items[gap + 1..]);
    let events: Vec<_> = since_gap
        .iter()
        .filter_map(|item| match item {
            SessionObservationStreamItem::Event(event) => Some(Arc::clone(event)),
            SessionObservationStreamItem::Gap { .. } => None,
        })
        .collect();
    followed_prose(&events)
}

fn turn_started(events: &[Arc<SessionObservationEvent>]) -> bool {
    events.iter().any(|event| {
        matches!(
            &event.payload,
            SessionObservationEventPayload::TurnActivity(TurnActivity {
                event: TurnEvent::TurnStarted { .. },
                ..
            })
        )
    })
}

/// The session's live replay, its window held on its own clock. Once
/// [`arm`](Self::arm)ed, a replay waits for a `TurnStarted` published after
/// the arming: the resumed turn's preparation republishes its marker, and
/// its publisher has flushed it before the re-send reads the replay.
#[derive(Debug)]
struct Resumed {
    inner: InMemoryLiveReplayStore,
    armed: AtomicBool,
    republished: tokio::sync::Notify,
    started: AtomicBool,
}

impl Resumed {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl LiveReplayStore for Resumed {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        let published = self.inner.publish(session_id, revision, events).await?;
        if self.armed.load(Ordering::SeqCst) && turn_started(&published) {
            self.started.store(true, Ordering::SeqCst);
            self.republished.notify_waiters();
        }
        Ok(published)
    }

    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        if self.armed.load(Ordering::SeqCst) {
            let republished = self.republished.notified();
            if !self.started.load(Ordering::SeqCst) {
                let _ = tokio::time::timeout(PUBLISHED_WITHIN, republished).await;
            }
        }
        self.inner.replay_after_cursor(cursor).await
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        self.inner.subscribe_after_cursor(cursor).await
    }

    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.inner.current_cursor(session_id, revision)
    }

    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        self.inner.invalidate_session(session_id).await
    }

    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        self.inner.trim_session(session_id).await
    }
}

/// The live replay's retention: far shorter than the model call's deadline,
/// so it drops A's activity while the call stays re-sendable (FIG-5399).
const RETENTION: Duration = Duration::from_secs(1);

/// A follower connected before the turn holds A's partial answer; retention
/// then drops A's activity and the turn's original marker, while the session
/// stays live. B's resume republishes the turn's `TurnStarted`, which lands
/// fresh in the trimmed window. That marker was made after A streamed, so
/// it cannot prove the replay still holds what A streamed: the re-send
/// restarts the stream with one gap, and the follower never holds A's text
/// joined to B's (FIG-5399).
async fn a_resend_after_retention_trimmed_the_abandoned_attempt_gaps(dialect: Dialect) {
    let window = SimClock::new();
    let live = Arc::new(Resumed {
        inner: InMemoryLiveReplayStore::with_clock(
            InMemoryLiveReplayStoreConfig {
                max_age: RETENTION,
                ..InMemoryLiveReplayStoreConfig::default()
            },
            Arc::clone(&window) as _,
        ),
        armed: AtomicBool::new(false),
        republished: tokio::sync::Notify::new(),
        started: AtomicBool::new(false),
    });
    let takeover = Takeover::new(dialect, None, Some(Arc::clone(&live) as _)).await;
    let (follower, opened_at) = takeover.follow().await;
    let opened = takeover
        .observed
        .observe()
        .snapshot()
        .await
        .expect("the session's snapshot")
        .cursor;
    takeover.a_streams_and_dies(&follower).await;

    window.advance_by(2 * RETENTION.as_millis() as u64).await;
    live.trim_session(&session())
        .await
        .expect("retention applies");
    assert!(
        matches!(
            live.replay_after_cursor(&opened).await,
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Trimmed))
        ),
        "retention dropped A's activity, the session still live"
    );
    live.arm();
    takeover.b_finishes().await;

    follower
        .until("the follower reaches the turn's commit", |feed| {
            committed_past(feed, opened_at)
        })
        .await;
    let items = follower.items();
    assert_eq!(
        follower.gaps(),
        1,
        "a replay that cannot prove it holds A's activity restarts the stream: {items:#?}"
    );
    let held = held_prose(&items);
    assert!(
        !held.contains(ABANDONED),
        "the follower holds A's abandoned text joined to B's answer: {held:?}"
    );
    takeover.end();
}

#[tokio::test]
async fn a_resend_after_retention_trimmed_the_abandoned_attempt_gaps_on_sqlite_memory() {
    a_resend_after_retention_trimmed_the_abandoned_attempt_gaps(Dialect::SqliteMemory).await;
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
    let store: Arc<dyn LiveReplayStore> = Arc::new(InMemoryLiveReplayStore::default());
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
