use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio_util::sync::ReusableBoxFuture;

pub use super::process_lifecycle::SessionProcessEventKind;

use crate::runtime::LashRuntime;
use crate::runtime::RuntimeSessionState;

const SESSION_CURSOR_PREFIX: &str = "lashsc2:";
const STANDARD_LIVE_REPLAY_CAPACITY: usize = 2048;
const STANDARD_LIVE_REPLAY_TTL: Duration = Duration::from_secs(120);

#[path = "replay/activity_spans.rs"]
mod activity_spans;
#[path = "replay/bytes.rs"]
mod bytes;
#[path = "replay/retention.rs"]
mod retention;
use activity_spans::DeliveredActivities;
use retention::ReplayRetention;
#[path = "replay/publication.rs"]
mod publication;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

impl SessionRevision {
    /// Constructs a `SessionRevision` for store and durable-substrate implementors while resuming
    /// live observation from a durable cursor.
    pub fn new(revision: u64) -> Self {
        Self(revision)
    }

    /// Exposes the monotonic session revision to live-replay store implementors for cursor
    /// comparison without changing its ordering semantics.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub(in crate::runtime) fn from_runtime(runtime: &LashRuntime) -> Self {
        observation_revision(&runtime.state)
    }

    /// The observation revision of the durable head `head` names: what a
    /// runtime that adopts that head projects (see [`observation_revision`]).
    /// A head without a checkpoint has committed no turn, so its revision
    /// is zero.
    pub fn of_durable_head(head: &crate::store::SessionHeadMeta) -> Self {
        Self(if head.checkpoint_ref.is_some() {
            head.head_revision
        } else {
            0
        })
    }
}

/// The observation revision a session state projects: the committed head
/// revision once a checkpoint exists, the turn index before the first one.
pub(in crate::runtime) fn observation_revision(state: &RuntimeSessionState) -> SessionRevision {
    SessionRevision(if state.checkpoint_ref.is_some() {
        state.head_revision
    } else {
        state.turn_index as u64
    })
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionCursor(String);

impl SessionCursor {
    pub fn new(
        replay_incarnation_id: impl AsRef<str>,
        session_id: impl AsRef<str>,
        revision: SessionRevision,
        live_position: u64,
    ) -> Self {
        Self(format!(
            "{SESSION_CURSOR_PREFIX}{}:{}:{live_position}:{}",
            replay_incarnation_id.as_ref(),
            revision.0,
            session_id.as_ref()
        ))
    }

    /// Validate and adopt a cursor token produced by a custom live-replay store.
    ///
    /// Lash keeps the token opaque to ordinary consumers, while custom store
    /// implementations use persisted cursor values to construct
    /// [`SessionObservationEvent`] values and implement
    /// [`LiveReplayStore::current_cursor`].
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn from_store_token(token: impl Into<String>) -> Result<Self, SessionCursorError> {
        let cursor = Self(token.into());
        cursor.parse()?;
        Ok(cursor)
    }

    #[cfg(test)]
    pub(super) fn from_raw_for_testing(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Exposes the opaque durable cursor to live-replay store implementors for persistence and
    /// round-tripping, without promising lexical ordering.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse_for_session(
        &self,
        expected_session_id: &SessionId,
    ) -> Result<ParsedSessionCursor<'_>, SessionCursorError> {
        let parsed = self.parse()?;
        if parsed.session_id != *expected_session_id {
            return Err(SessionCursorError::WrongSession {
                expected_session_id: expected_session_id.clone(),
                actual_session_id: parsed.session_id,
            });
        }
        Ok(parsed)
    }

    /// Read the incarnation, session, revision and live position this
    /// cursor names. A store learns the session a cursor addresses here.
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn parse(&self) -> Result<ParsedSessionCursor<'_>, SessionCursorError> {
        let payload = self.0.strip_prefix(SESSION_CURSOR_PREFIX).ok_or_else(|| {
            SessionCursorError::Malformed {
                message: "missing cursor prefix".to_string(),
            }
        })?;
        let mut parts = payload.splitn(4, ':');
        let replay_incarnation_id =
            parts
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| SessionCursorError::Malformed {
                    message: "missing replay incarnation id".to_string(),
                })?;
        let revision = parts
            .next()
            .ok_or_else(|| SessionCursorError::Malformed {
                message: "missing session revision".to_string(),
            })?
            .parse::<u64>()
            .map_err(|err| SessionCursorError::Malformed {
                message: format!("invalid session revision: {err}"),
            })?;
        let live_position = parts
            .next()
            .ok_or_else(|| SessionCursorError::Malformed {
                message: "missing live replay position".to_string(),
            })?
            .parse::<u64>()
            .map_err(|err| SessionCursorError::Malformed {
                message: format!("invalid live replay position: {err}"),
            })?;
        let session_id = parts
            .next()
            .and_then(|value| SessionId::parse(value).ok())
            .ok_or_else(|| SessionCursorError::Malformed {
                message: "missing session id".to_string(),
            })?;
        Ok(ParsedSessionCursor {
            replay_incarnation_id,
            session_id,
            revision: SessionRevision(revision),
            live_position,
        })
    }
}

impl fmt::Debug for SessionCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCursor(<opaque>)")
    }
}

impl fmt::Display for SessionCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct ParsedSessionCursor<'a> {
    pub replay_incarnation_id: &'a str,
    pub session_id: SessionId,
    pub revision: SessionRevision,
    pub live_position: u64,
}

#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SessionCursorError {
    #[error("malformed session cursor: {message}")]
    Malformed { message: String },
    #[error("session cursor belongs to `{actual_session_id}`, not `{expected_session_id}`")]
    WrongSession {
        expected_session_id: SessionId,
        actual_session_id: SessionId,
    },
}

#[derive(Clone, Debug)]
pub struct SessionObservation {
    pub read_view: crate::SessionReadView,
    pub cursor: SessionCursor,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionObservationEvent {
    pub turn_id: Option<TurnId>,
    pub cursor: SessionCursor,
    pub payload: SessionObservationEventPayload,
}

impl SessionObservationEvent {
    /// Construct an event from a live-replay store's durable cursor.
    ///
    /// The cursor is validated before the event is constructed, preserving the
    /// invariant that the event's identity accessors can parse it infallibly.
    /// Construction outside `lash-core` goes through [`Self::new`].
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn new(
        turn_id: Option<TurnId>,
        cursor: SessionCursor,
        payload: SessionObservationEventPayload,
    ) -> Result<Self, SessionCursorError> {
        cursor.parse()?;
        Ok(Self {
            turn_id,
            cursor,
            payload,
        })
    }

    /// Returns the session named by this event's durable cursor.
    ///
    /// Owned rather than borrowed: the cursor stores the identity as a slice of
    /// a larger string, and a `&SessionId` cannot be reborrowed out of a `&str`.
    #[expect(
        clippy::expect_used,
        reason = "the store writes cursors in the parsable form"
    )]
    pub fn session_id(&self) -> SessionId {
        self.cursor
            .parse()
            .expect("store-created observation event cursor must parse")
            .session_id
    }

    /// Returns the replay-store incarnation named by this event's durable cursor.
    #[expect(
        clippy::expect_used,
        reason = "the store writes cursors in the parsable form"
    )]
    pub fn replay_incarnation_id(&self) -> &str {
        self.cursor
            .parse()
            .expect("store-created observation event cursor must parse")
            .replay_incarnation_id
    }

    /// Returns the session revision named by this event's durable cursor.
    #[expect(
        clippy::expect_used,
        reason = "the store writes cursors in the parsable form"
    )]
    pub fn revision(&self) -> SessionRevision {
        self.cursor
            .parse()
            .expect("store-created observation event cursor must parse")
            .revision
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionQueueEventKind {
    Enqueued,
    Cancelled,
}

#[derive(Clone, Debug)]
// justification: the enclosing replay event is already Arc-owned, so another allocation would not bound retained event storage.
#[allow(clippy::large_enum_variant)]
pub enum SessionObservationEventPayload {
    /// Provisional language evidence with its stable producer identity.
    LanguageExecution(lash_trace::LanguageExecutionObservation),
    TurnActivity(crate::TurnActivity),
    /// A durable commit, by reference: the event's cursor names the
    /// committed revision, and `entries` are the transcript entries the
    /// commit added to the session at `base_revision`. A consumer holding
    /// `base_revision` advances by applying `entries`; any other consumer
    /// loads the durable head. The full read view never rides the feed.
    Committed {
        base_revision: SessionRevision,
        entries: Vec<crate::transcript::TranscriptEntry>,
    },
    /// A revision-stable change to resident authority, by reference: a
    /// consumer that needs the resident view reads it again.
    ResidentChanged,
    /// The session's current frame changed to `frame_id`. `commit` names
    /// the durable commit that made the switch, published ahead of that
    /// commit's `Committed`: a consumer holding that revision or a later
    /// one already holds the frame. A switch no commit made (a resident
    /// change at a stable revision) names none.
    AgentFrameSwitched {
        frame_id: String,
        commit: Option<SessionRevision>,
    },
    QueueChanged {
        kind: SessionQueueEventKind,
        batch_ids: Vec<String>,
    },
    ProcessChanged {
        kind: SessionProcessEventKind,
        process_ids: Vec<ProcessId>,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LiveReplayGap {
    pub session_id: SessionId,
    pub requested_cursor: SessionCursor,
    pub latest_cursor: SessionCursor,
    pub latest_revision: SessionRevision,
    pub reason: LiveReplayGapReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveReplayGapReason {
    Trimmed,
    Unavailable,
}

#[derive(Clone, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LiveReplayStoreError {
    #[error(
        "session `{session_id}` republished language event `{event_key}` with a different fact"
    )]
    ConflictingLanguageRedelivery {
        session_id: SessionId,
        event_key: String,
    },
    #[error("{0}")]
    Cursor(#[from] SessionCursorError),
    #[error("live replay store error: {0}")]
    Store(String),
    #[error("live replay subscriber lagged by {0} events")]
    SubscriberLagged(u64),
    #[error("live replay channel closed")]
    Closed,
}

#[derive(Clone, Debug)]
pub enum LiveReplayOutcome {
    Replayed(Vec<Arc<SessionObservationEvent>>),
    Gap(LiveReplayGapReason),
}

pub enum LiveReplaySubscribeOutcome {
    Subscribed(LiveReplaySubscription),
    Gap(LiveReplayGapReason),
}

/// One event of a batch handed to [`LiveReplayStore::publish`], before the
/// store assigns its position.
#[derive(Clone, Debug)]
pub struct LiveReplayEventDraft {
    pub turn_id: Option<TurnId>,
    pub payload: SessionObservationEventPayload,
}

impl LiveReplayEventDraft {
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn new(
        turn_id: Option<impl Into<TurnId>>,
        payload: SessionObservationEventPayload,
    ) -> Self {
        Self {
            turn_id: turn_id.map(Into::into),
            payload,
        }
    }
}

#[derive(Clone, Debug)]
struct ReplayNotification {
    position: u64,
    event: Weak<SessionObservationEvent>,
}

type LiveReplayRecvResult = (
    Result<ReplayNotification, broadcast::error::RecvError>,
    broadcast::Receiver<ReplayNotification>,
);

#[inline]
fn clone_event(event: &Arc<SessionObservationEvent>) -> Arc<SessionObservationEvent> {
    Arc::clone(event)
}

/// A live replay subscription: the retained events after the subscribed
/// cursor, then the store's live tail.
///
/// Any [`LiveReplayStore`] builds one with [`Self::new`] from the events it
/// replays and a stream of the events it publishes afterwards. Lash reads the
/// replayed prefix to judge whether it bridges a stale cursor to the
/// authoritative revision, so a store hands every retained event it replays
/// in `replay`, not in `live`. The live tail ends a lagging subscriber with
/// [`LiveReplayStoreError::SubscriberLagged`] and a closed one with
/// [`LiveReplayStoreError::Closed`]; observers then resubscribe from their
/// cursor.
///
/// Integrator class (ADR 0051): **custom live-replay store implementors**.
pub struct LiveReplaySubscription {
    replay: VecDeque<Arc<SessionObservationEvent>>,
    live: Pin<Box<dyn Stream<Item = LiveReplayItem> + Send>>,
}

/// One item of a live replay subscription's tail.
type LiveReplayItem = Result<Arc<SessionObservationEvent>, LiveReplayStoreError>;

impl LiveReplaySubscription {
    /// A subscription that yields `replay` in order, then `live`.
    pub fn new(
        replay: Vec<Arc<SessionObservationEvent>>,
        live: impl Stream<Item = LiveReplayItem> + Send + 'static,
    ) -> Self {
        Self {
            replay: replay.into(),
            live: Box::pin(live),
        }
    }

    /// Whether the replayed prefix holds a `Committed` event at or after
    /// `revision`: the evidence that a subscription from a cursor behind
    /// `revision` bridges to it.
    pub fn bridges_to(&self, revision: SessionRevision) -> bool {
        self.replay.iter().any(|event| {
            event.revision() >= revision
                && matches!(
                    &event.payload,
                    SessionObservationEventPayload::Committed { .. }
                )
        })
    }
}

impl fmt::Debug for LiveReplaySubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveReplaySubscription")
            .field("replayed", &self.replay.len())
            .finish_non_exhaustive()
    }
}

impl Stream for LiveReplaySubscription {
    type Item = LiveReplayItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.replay.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        self.live.as_mut().poll_next(cx)
    }
}

/// The in-memory store's live tail: its broadcast channel's notifications
/// past the subscribed position.
struct BroadcastTail {
    receiver: ReusableBoxFuture<'static, LiveReplayRecvResult>,
    after_position: u64,
    closed: bool,
}

impl BroadcastTail {
    fn new(receiver: broadcast::Receiver<ReplayNotification>, after_position: u64) -> Self {
        Self {
            receiver: ReusableBoxFuture::new(live_replay_recv(receiver)),
            after_position,
            closed: false,
        }
    }
}

async fn live_replay_recv(
    mut receiver: broadcast::Receiver<ReplayNotification>,
) -> LiveReplayRecvResult {
    let result = receiver.recv().await;
    (result, receiver)
}

impl Stream for BroadcastTail {
    type Item = LiveReplayItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.closed {
            return Poll::Ready(None);
        }
        let (result, receiver) = ready!(self.receiver.poll(cx));
        self.receiver.set(live_replay_recv(receiver));
        match result {
            Ok(notification) => {
                if notification.position <= self.after_position {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    self.after_position = notification.position;
                    Poll::Ready(Some(
                        notification
                            .event
                            .upgrade()
                            .ok_or(LiveReplayStoreError::SubscriberLagged(1)),
                    ))
                }
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                Poll::Ready(Some(Err(LiveReplayStoreError::SubscriberLagged(count))))
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.closed = true;
                Poll::Ready(Some(Err(LiveReplayStoreError::Closed)))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum SessionResume {
    Replayed {
        events: Vec<Arc<SessionObservationEvent>>,
    },
    Gap {
        observation: SessionObservation,
        gap: LiveReplayGap,
    },
}

pub enum SessionObservationSubscription {
    Subscribed(LiveReplaySubscription),
    Gap {
        observation: SessionObservation,
        gap: LiveReplayGap,
    },
}

/// Bounded, best-effort live replay for host reconnects: the tail of every
/// session feed.
///
/// [`InMemoryLiveReplayStore`] is the default and holds one process's
/// publications. A host whose sessions run on several processes plugs in one
/// shared implementation (`LashCoreBuilder::live_replay_store`), and every
/// process's feed then carries every process's events. A feed's snapshot is
/// the session's durable head either way, so the store decides freshness,
/// never the snapshot's consistency.
///
/// # Publication
///
/// The store is each session's sequencer. [`publish`](Self::publish) hands it
/// a batch; the store assigns the batch its positions, makes it visible, and
/// answers the published events with their cursors. Lash installs the
/// authoritative observation with the returned cursor only after that
/// answer, so no position exists before it is visible, and a publication
/// that fails leaves no hole: lash logs it and the observation takes the
/// store's [`current_cursor`](Self::current_cursor).
///
/// Lash publishes off the turn: turn activity reaches the store from the
/// turn's observation publisher, which drains outside the shift, so a slow
/// store slows a feed, never a turn. A subscriber that falls behind ends with
/// [`LiveReplayStoreError::SubscriberLagged`] and resubscribes from its cursor.
///
/// # Obligations
///
/// A [`SessionCursor`] names an incarnation, a session, a revision and a
/// live position. An implementation keeps every rule below; the conformance
/// laws in `lash-conformance` (`live_replay_tests!`) certify them.
///
/// - **One position universe per session.** Every event published for a
///   session, by any writer on any process, takes its position from one
///   sequence, so an event a subscriber missed is always a detectable gap,
///   never a silent loss.
/// - **One total order across writers.** Concurrent publications to one
///   session (the run's own runtime, queue changes on the process that
///   accepted a send, process transitions, host handles) receive
///   contiguous positions, and every reader sees them in position order.
///   A batch's events are contiguous and in batch order.
/// - **Incarnation is the publisher epoch, and lives as long as its
///   history.** Positions are ordered and comparable only within one
///   incarnation. A cursor naming another incarnation answers
///   [`LiveReplayGapReason::Unavailable`], never an empty replay. An
///   implementation keeps an incarnation across a restart only when it keeps
///   the history behind it, and rotates it whenever that history is lost.
/// - **Positions never repeat.** A session's positions stay monotone across
///   retention, eviction and recreation of its buffer, so an old cursor can
///   never name a new event.
/// - **Replay and subscription are exact and linearizable.** Replay and
///   subscription after a cursor yield exactly the session's retained events
///   past its position, in position order, each once, and never another
///   session's events. A subscription yields its replayed prefix before any
///   live event, with no event lost or repeated across that boundary, even
///   while publications race the subscribe.
/// - **The window is bounded, and its gaps are typed.** A position that
///   retention dropped answers [`LiveReplayGapReason::Trimmed`]; one past
///   the tail, or before an [`invalidate_session`](Self::invalidate_session),
///   answers [`LiveReplayGapReason::Unavailable`].
/// - **A redelivered activity is published once.** A batch's
///   `TurnActivity` whose id the session's window already holds is dropped,
///   whichever process published the first copy: a redriven shift
///   republishes what its first attempt delivered (FIG-3753).
/// - **Lag means resubscribe.** A lagging or closed subscription ends with
///   [`LiveReplayStoreError::SubscriberLagged`] or
///   [`LiveReplayStoreError::Closed`], after which the observer resubscribes
///   from its cursor.
/// - **Invalidation reaches every subscriber.** After
///   [`invalidate_session`](Self::invalidate_session), every existing cursor
///   answers `Unavailable` and every live subscription to the session, on
///   any process, closes.
/// - **Current cursors stay behind newer revisions.**
///   [`current_cursor`](Self::current_cursor) at revision `N` sits before
///   every event at a revision past `N` that the store holds or will hold,
///   so a feed whose snapshot raced a newer commit replays that commit.
///   Revisions are stamped by publishers and need not grow with position.
///
/// A subscription's tail is any stream ([`LiveReplaySubscription::new`]), and
/// [`subscribe_after_cursor`](Self::subscribe_after_cursor) may take its time
/// to decide between a subscription and a gap: the feed awaits both.
#[async_trait::async_trait]
pub trait LiveReplayStore: Send + Sync {
    /// Assign `events` the session's next positions, in order, make them
    /// replay-visible, notify subscribers in position order, and answer the
    /// published events. A `TurnActivity` the session's window already
    /// holds is dropped from the batch, so the answer may be shorter than
    /// `events`, or empty.
    ///
    /// Ids that name observations
    /// ([`TurnActivityId::observed_span`](crate::TurnActivityId::observed_span))
    /// compare by the ordinals they cover, not literally: a draft inside the
    /// span of its replay key the session holds is dropped, one wholly
    /// outside it is published, and one that straddles its edge is not
    /// published and invalidates the session's continuity, as
    /// [`invalidate_session`](Self::invalidate_session) does, because its
    /// undelivered text cannot be cut from its delivered text (FIG-5098).
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError>;

    /// The session's retained events after `cursor`, or the gap that
    /// prevents continuing from it.
    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError>;

    /// Subscribe after `cursor`, replaying retained events before live
    /// events, or answer the gap that prevents continuing from it.
    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>;

    /// A runtime snapshot at revision N can race with a separate worker
    /// publishing revision N+1. The returned cursor must remain before that
    /// newer event so replay reconciles the stale snapshot.
    ///
    /// Lash calls this from synchronous code, so it answers from what the
    /// store holds locally: a cursor earlier than the true tail only
    /// replays more.
    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor;

    /// The cursor every event the store still retains for the session comes
    /// after. An observer whose cursor gapped resumes from it, so its gap
    /// is only what the store no longer holds: what was published between
    /// the loss and its resubscription is replayed, not skipped.
    ///
    /// Synchronous like [`current_cursor`](Self::current_cursor). A store
    /// that cannot name the start of its window answers its head, and its
    /// observers lose that interval too.
    fn earliest_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        self.current_cursor(session_id, revision)
    }

    /// Mark this session's replay continuity unavailable without inventing a
    /// revision. Existing cursors must return `Gap(Unavailable)` and active
    /// subscriptions must close so observers reload their authoritative snapshot.
    /// A cursor acquired after that snapshot establishes fresh continuity.
    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError>;

    /// Retire every session's continuity after a publisher loses an unbounded
    /// set of subjects. Every existing cursor returns `Gap(Unavailable)` and
    /// every live subscription closes. New windows cannot reuse old positions.
    async fn invalidate_all(&self) -> Result<(), LiveReplayStoreError>;

    /// Apply retention to the session's window.
    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError>;
}

/// What an [`InMemoryLiveReplayStore`] retains for reconnecting observers.
/// The host states it; there is no default (D-DEFAULTS2).
#[derive(Clone, Debug)]
pub struct InMemoryLiveReplayStoreConfig {
    /// The most recent events one session's window keeps.
    pub max_events_per_session: usize,
    /// How long an event stays replayable; also how long an entry nobody
    /// follows may sit idle before it is released.
    pub max_age: Duration,
    /// Maximum resident session entries across this store.
    pub max_sessions: usize,
    /// Maximum charged bytes across session metadata and retained events.
    pub max_retained_bytes: usize,
}

impl InMemoryLiveReplayStoreConfig {
    /// The standard retention: 2,048 events per session, replayable for 120
    /// seconds, across at most 4,096 sessions and 64 MiB. No measurement
    /// backs these values.
    pub const fn standard() -> Self {
        Self {
            max_events_per_session: STANDARD_LIVE_REPLAY_CAPACITY,
            max_age: STANDARD_LIVE_REPLAY_TTL,
            max_sessions: 4096,
            max_retained_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct InMemoryLiveReplayStore {
    work_limits: lash_trace::ObservationWorkLimits,
    replay_incarnation_id: String,
    config: InMemoryLiveReplayStoreConfig,
    clock: Arc<dyn crate::Clock>,
    sessions: Arc<StdMutex<ReplayRetention>>,
    #[cfg(any(test, feature = "testing"))]
    before_notification_gate: Option<BeforeNotificationGate>,
}

#[cfg(any(test, feature = "testing"))]
type BeforeNotificationCallback = dyn Fn(&[Arc<SessionObservationEvent>]) + Send + Sync + 'static;

#[cfg(any(test, feature = "testing"))]
#[derive(Clone)]
struct BeforeNotificationGate(Arc<BeforeNotificationCallback>);

#[cfg(any(test, feature = "testing"))]
impl fmt::Debug for BeforeNotificationGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BeforeNotificationGate(..)")
    }
}

impl InMemoryLiveReplayStore {
    pub fn new(config: InMemoryLiveReplayStoreConfig) -> Self {
        Self::with_clock(config, Arc::new(crate::SystemClock))
    }

    pub fn with_clock(config: InMemoryLiveReplayStoreConfig, clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            work_limits: lash_trace::ObservationWorkLimits::standard(),
            replay_incarnation_id: uuid::Uuid::new_v4().to_string(),
            config,
            clock,
            sessions: Arc::new(StdMutex::new(ReplayRetention::default())),
            #[cfg(any(test, feature = "testing"))]
            before_notification_gate: None,
        }
    }

    /// Configure bounded expiry work per store operation, separately from retention.
    #[must_use]
    pub fn with_work_limits(mut self, limits: lash_trace::ObservationWorkLimits) -> Self {
        self.work_limits = limits;
        self
    }

    /// Release all entries idle beyond `max_age` that have no live
    /// subscriber, and answer how many. Hosts call this tick during
    /// traffic-free periods; normal store calls also perform a bounded
    /// amount of global expiry work.
    pub fn expire_idle_sessions(&self) -> usize {
        self.sessions
            .lock_recover()
            .expire(&self.config, self.clock.now(), usize::MAX)
    }

    /// A store keeping `max_events_per_session` events for `max_age`, with
    /// [`InMemoryLiveReplayStoreConfig::standard`]'s session and byte bounds.
    pub fn with_bounds(max_events_per_session: usize, max_age: Duration) -> Self {
        Self::new(InMemoryLiveReplayStoreConfig {
            max_events_per_session,
            max_age,
            ..InMemoryLiveReplayStoreConfig::standard()
        })
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn with_before_notification_gate(
        mut self,
        gate: impl Fn(&[Arc<SessionObservationEvent>]) + Send + Sync + 'static,
    ) -> Self {
        self.before_notification_gate = Some(BeforeNotificationGate(Arc::new(gate)));
        self
    }

    /// Clone this store while preserving its replay incarnation and history.
    ///
    /// This is deliberately restricted to test and conformance support; hosts
    /// must construct a fresh store when retained history cannot be proven.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn clone_preserving_history(&self) -> Self {
        Self {
            replay_incarnation_id: self.replay_incarnation_id.clone(),
            config: self.config.clone(),
            work_limits: self.work_limits,
            clock: Arc::clone(&self.clock),
            sessions: Arc::clone(&self.sessions),
            #[cfg(any(test, feature = "testing"))]
            before_notification_gate: self.before_notification_gate.clone(),
        }
    }
}

#[derive(Debug)]
struct LiveReplaySessionBuffer {
    replay_incarnation_id: String,
    first_position: u64,
    last_access: Instant,
    retained_bytes: usize,
    channel_bytes: usize,
    events: VecDeque<StoredObservationEvent>,
    /// The last position assigned: every position up to it is published.
    tail_position: u64,
    /// Delivered `TurnActivity` identities with the live position each holds
    /// in `events`. A replayed shift region or a journaled step re-executed
    /// after a mid-run suspension re-publishes the observations its first
    /// attempt already delivered, framed alike or not; `publish` collapses
    /// those redeliveries into the stored copies (FIG-3753), by the ordinal
    /// ranges they cover (FIG-5098, `activity_spans`).
    delivered_activities: DeliveredActivities,
    sender: Option<broadcast::Sender<ReplayNotification>>,
}

impl LiveReplaySessionBuffer {
    fn new(now: Instant, first_position: u64, replay_incarnation_id: &str) -> Self {
        Self {
            replay_incarnation_id: replay_incarnation_id.to_string(),
            first_position,
            last_access: now,
            retained_bytes: 0,
            channel_bytes: 0,
            events: VecDeque::new(),
            tail_position: first_position,
            delivered_activities: DeliveredActivities::default(),
            sender: None,
        }
    }

    /// Whether a live subscription holds this entry's channel.
    fn is_followed(&self) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.receiver_count() > 0)
    }

    /// Drop the oldest stored event and release the activity identity it
    /// held, so a delivery after the replay window can land fresh.
    fn drop_front(&mut self) {
        if let Some(stored) = self.events.pop_front() {
            self.retained_bytes -= stored.retained_bytes;
            if let SessionObservationEventPayload::TurnActivity(activity) = &stored.event.payload {
                self.delivered_activities
                    .remove(&activity.id, stored.position);
            }
        }
    }

    /// Append one published event at `position`, claiming its activity
    /// identity.
    fn append(
        &mut self,
        event: &Arc<SessionObservationEvent>,
        position: u64,
        retained_bytes: usize,
        now: Instant,
    ) {
        if let SessionObservationEventPayload::TurnActivity(activity) = &event.payload {
            self.delivered_activities.insert(&activity.id, position);
        }
        self.retained_bytes += retained_bytes;
        self.events.push_back(StoredObservationEvent {
            retained_bytes,
            position,
            appended_at: now,
            event: clone_event(event),
        });
        self.tail_position = position;
    }

    fn subscribe(
        &mut self,
        channel_capacity: usize,
        channel_bytes: usize,
    ) -> broadcast::Receiver<ReplayNotification> {
        match self.sender.as_ref() {
            Some(sender) => sender.subscribe(),
            None => {
                let (sender, receiver) = broadcast::channel(channel_capacity.max(1));
                self.sender = Some(sender);
                self.channel_bytes = channel_bytes;
                self.retained_bytes += channel_bytes;
                receiver
            }
        }
    }

    #[expect(clippy::expect_used, reason = "store-created event cursors must parse")]
    fn notify(&mut self, event: &Arc<SessionObservationEvent>) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let position = event
            .cursor
            .parse()
            .expect("store-created cursor must parse")
            .live_position;
        if sender
            .send(ReplayNotification {
                position,
                event: Arc::downgrade(event),
            })
            .is_err()
        {
            self.sender = None;
            self.retained_bytes -= self.channel_bytes;
            self.channel_bytes = 0;
        }
    }
}

#[derive(Clone, Debug)]
struct StoredObservationEvent {
    retained_bytes: usize,
    position: u64,
    appended_at: Instant,
    event: Arc<SessionObservationEvent>,
}

impl InMemoryLiveReplayStore {
    fn trim_locked(
        config: &InMemoryLiveReplayStoreConfig,
        buffer: &mut LiveReplaySessionBuffer,
        now: Instant,
    ) {
        while buffer.events.len() > config.max_events_per_session {
            buffer.drop_front();
        }
        while buffer
            .events
            .front()
            .is_some_and(|event| now.duration_since(event.appended_at) > config.max_age)
        {
            buffer.drop_front();
        }
    }

    fn gap_reason_for_cursor(
        buffer: &LiveReplaySessionBuffer,
        cursor_position: u64,
    ) -> Option<LiveReplayGapReason> {
        if cursor_position < buffer.first_position || cursor_position > buffer.tail_position {
            return Some(LiveReplayGapReason::Unavailable);
        }
        let Some(first) = buffer.events.front() else {
            return (cursor_position < buffer.tail_position)
                .then_some(LiveReplayGapReason::Trimmed);
        };
        if cursor_position + 1 < first.position {
            Some(LiveReplayGapReason::Trimmed)
        } else {
            None
        }
    }

    fn incarnation_gap_for_cursor(
        buffer: Option<&LiveReplaySessionBuffer>,
        cursor: &ParsedSessionCursor,
    ) -> Option<LiveReplayGapReason> {
        if buffer.is_some_and(|buffer| cursor.replay_incarnation_id == buffer.replay_incarnation_id)
        {
            None
        } else {
            Some(LiveReplayGapReason::Unavailable)
        }
    }

    /// Notify the session's subscribers of `events`, which the caller just
    /// appended: refused when the session was evicted or recreated since.
    fn notify_published(
        sessions: &mut ReplayRetention,
        session_id: &SessionId,
        events: &[Arc<SessionObservationEvent>],
        first_position: u64,
    ) -> Result<(), LiveReplayStoreError> {
        let disappeared = || {
            LiveReplayStoreError::Store(
                "published live replay session disappeared before notification".into(),
            )
        };
        sessions
            .update(session_id, |buffer| {
                if buffer.replay_incarnation_id != events[0].replay_incarnation_id()
                    || first_position < buffer.first_position
                {
                    return Err(disappeared());
                }
                for event in events {
                    buffer.notify(event);
                }
                Ok(())
            })
            .ok_or_else(disappeared)?
    }
}

#[async_trait::async_trait]
impl LiveReplayStore for InMemoryLiveReplayStore {
    async fn publish(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        drafts: Vec<LiveReplayEventDraft>,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        if drafts.is_empty() {
            return Err(LiveReplayStoreError::Store(
                "cannot publish an empty live replay batch".to_string(),
            ));
        }
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        sessions.ensure_session(&self.config, session_id, now, &self.replay_incarnation_id)?;
        let filtered = sessions
            .update(session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now);
                publication::filter(buffer, drafts, session_id)
            })
            .ok_or_else(|| LiveReplayStoreError::Store("live replay session is missing".into()))?;
        let (drafts, overlapping) = match filtered {
            Ok(filtered) => filtered,
            Err(error) => {
                sessions.remove(session_id);
                return Err(error);
            }
        };
        if overlapping {
            // A redelivery that is only partly delivered is a gap: its
            // undelivered text cannot be cut out of the delivered part, so
            // continuity is invalidated rather than text duplicated or lost
            // (`activity_spans`).
            sessions.remove(session_id);
            sessions.ensure_session(&self.config, session_id, now, &self.replay_incarnation_id)?;
        }
        let (start_position, incarnation) = sessions
            .update(session_id, |buffer| {
                (
                    buffer.tail_position.checked_add(1),
                    buffer.replay_incarnation_id.clone(),
                )
            })
            .ok_or_else(|| LiveReplayStoreError::Store("live replay session is missing".into()))?;
        if drafts.is_empty() {
            return Ok(Vec::new());
        }
        let start_position = start_position
            .filter(|start| {
                u64::try_from(drafts.len())
                    .ok()
                    .and_then(|count| start.checked_add(count))
                    .is_some()
            })
            .ok_or_else(|| LiveReplayStoreError::Store("live replay position overflow".into()))?;
        let events = drafts
            .into_iter()
            .enumerate()
            .map(|(offset, draft)| {
                SessionObservationEvent::new(
                    draft.turn_id,
                    SessionCursor::new(
                        &incarnation,
                        session_id,
                        revision,
                        start_position + offset as u64,
                    ),
                    draft.payload,
                )
                .map(Arc::new)
            })
            .collect::<Result<Vec<_>, SessionCursorError>>()?;
        // An oversized batch is refused before it takes a position, and it
        // retires the session's continuity: a cursor across the refused
        // batch must not replay as a clean empty suffix.
        let charged = events
            .iter()
            .map(|event| bytes::event_bytes(event, self.config.max_retained_bytes))
            .collect::<Result<Vec<_>, _>>()
            .and_then(|event_bytes| {
                let total = event_bytes
                    .iter()
                    .try_fold(0_usize, |total, bytes| total.checked_add(*bytes))
                    .ok_or_else(|| {
                        LiveReplayStoreError::Store("live replay byte count overflow".into())
                    })?;
                sessions.reserve_bytes(&self.config, session_id, total)?;
                Ok(event_bytes)
            });
        let event_bytes = match charged {
            Ok(event_bytes) => event_bytes,
            Err(error) => {
                sessions.remove(session_id);
                return Err(error);
            }
        };
        sessions.update(session_id, |buffer| {
            for (offset, (event, retained_bytes)) in events.iter().zip(event_bytes).enumerate() {
                buffer.append(event, start_position + offset as u64, retained_bytes, now);
            }
            Self::trim_locked(&self.config, buffer, now);
        });
        sessions.touch(session_id, now);
        #[cfg(any(test, feature = "testing"))]
        if let Some(gate) = self.before_notification_gate.as_ref() {
            drop(sessions);
            (gate.0)(&events);
            sessions = self.sessions.lock_recover();
        }
        Self::notify_published(&mut sessions, session_id, &events, start_position)?;
        Ok(events)
    }

    async fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        let parsed = cursor.parse()?;
        let session_id = parsed.session_id.clone();
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        if Self::incarnation_gap_for_cursor(sessions.buffers.get(&session_id), &parsed).is_none() {
            sessions.touch(&session_id, now);
            sessions.update(&session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now)
            });
        }
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        let buffer = sessions.buffers.get(&session_id);
        if let Some(reason) = Self::incarnation_gap_for_cursor(buffer, &parsed).or_else(|| {
            buffer.and_then(|buffer| Self::gap_reason_for_cursor(buffer, parsed.live_position))
        }) {
            return Ok(LiveReplayOutcome::Gap(reason));
        }
        let events = buffer
            .map(|buffer| {
                buffer
                    .events
                    .iter()
                    .filter(|event| event.position > parsed.live_position)
                    .map(|event| clone_event(&event.event))
                    .collect()
            })
            .unwrap_or_default();
        Ok(LiveReplayOutcome::Replayed(events))
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError> {
        let parsed = cursor.parse()?;
        let session_id = parsed.session_id.clone();
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        if Self::incarnation_gap_for_cursor(sessions.buffers.get(&session_id), &parsed).is_none() {
            sessions.touch(&session_id, now);
            sessions.update(&session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now)
            });
        }
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        let buffer = sessions.buffers.get(&session_id);
        if let Some(reason) = Self::incarnation_gap_for_cursor(buffer, &parsed).or_else(|| {
            buffer.and_then(|buffer| Self::gap_reason_for_cursor(buffer, parsed.live_position))
        }) {
            return Ok(LiveReplaySubscribeOutcome::Gap(reason));
        }
        let channel_bytes = if buffer.is_some_and(|buffer| buffer.sender.is_none()) {
            retention::channel_bytes(self.config.max_events_per_session)?
        } else {
            0
        };
        if sessions
            .reserve_bytes(&self.config, &session_id, channel_bytes)
            .is_err()
        {
            sessions.remove(&session_id);
            return Ok(LiveReplaySubscribeOutcome::Gap(
                LiveReplayGapReason::Unavailable,
            ));
        }
        Ok(sessions
            .update(&session_id, |buffer| {
                let replay = buffer
                    .events
                    .iter()
                    .filter(|event| event.position > parsed.live_position)
                    .map(|event| clone_event(&event.event))
                    .collect();
                let receiver = buffer.subscribe(self.config.max_events_per_session, channel_bytes);
                LiveReplaySubscribeOutcome::Subscribed(LiveReplaySubscription::new(
                    replay,
                    BroadcastTail::new(receiver, parsed.live_position),
                ))
            })
            .unwrap_or(LiveReplaySubscribeOutcome::Gap(
                LiveReplayGapReason::Unavailable,
            )))
    }

    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.touch(session_id, now);
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        if sessions
            .ensure_session(&self.config, session_id, now, &self.replay_incarnation_id)
            .is_err()
        {
            return SessionCursor::new(uuid::Uuid::new_v4().to_string(), session_id, revision, 0);
        }
        sessions
            .update(session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now);
                let live_position = buffer
                    .events
                    .iter()
                    .find(|stored| stored.event.revision() > revision)
                    .map_or(buffer.tail_position, |stored| {
                        stored.position.saturating_sub(1)
                    });
                SessionCursor::new(
                    &buffer.replay_incarnation_id,
                    session_id,
                    revision,
                    live_position,
                )
            })
            .unwrap_or_else(|| {
                SessionCursor::new(uuid::Uuid::new_v4().to_string(), session_id, revision, 0)
            })
    }

    fn earliest_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.touch(session_id, now);
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        if sessions
            .ensure_session(&self.config, session_id, now, &self.replay_incarnation_id)
            .is_err()
        {
            return SessionCursor::new(uuid::Uuid::new_v4().to_string(), session_id, revision, 0);
        }
        sessions
            .update(session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now);
                let live_position = buffer
                    .events
                    .front()
                    .map_or(buffer.tail_position, |stored| {
                        stored.position.saturating_sub(1)
                    });
                SessionCursor::new(
                    &buffer.replay_incarnation_id,
                    session_id,
                    revision,
                    live_position,
                )
            })
            .unwrap_or_else(|| {
                SessionCursor::new(uuid::Uuid::new_v4().to_string(), session_id, revision, 0)
            })
    }

    async fn invalidate_all(&self) -> Result<(), LiveReplayStoreError> {
        self.sessions.lock_recover().invalidate_all()
    }

    async fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        let mut sessions = self.sessions.lock_recover();
        sessions.remove(session_id);
        sessions.expire(
            &self.config,
            self.clock.now(),
            self.work_limits.replay_expiry_batch.get(),
        );
        Ok(())
    }

    async fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.touch(session_id, now);
        sessions.update(session_id, |buffer| {
            Self::trim_locked(&self.config, buffer, now)
        });
        sessions.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "replay/tests.rs"]
mod tests;
