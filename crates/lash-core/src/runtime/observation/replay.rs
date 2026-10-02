use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
const DEFAULT_LIVE_REPLAY_CAPACITY: usize = 2048;
const DEFAULT_LIVE_REPLAY_TTL: Duration = Duration::from_secs(120);
const EXPIRY_WORK_PER_CALL: usize = 64;

#[path = "replay/bytes.rs"]
mod bytes;
#[path = "replay/retention.rs"]
mod retention;
use retention::ReplayRetention;

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

    pub(super) fn from_runtime(runtime: &LashRuntime) -> Self {
        observation_revision(&runtime.state)
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

    fn parse(&self) -> Result<ParsedSessionCursor<'_>, SessionCursorError> {
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
    TurnActivity(crate::TurnActivity),
    Committed {
        read_view: crate::SessionReadView,
    },
    ResidentChanged {
        read_view: crate::SessionReadView,
    },
    AgentFrameSwitched {
        frame_id: String,
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

/// One event in a cursor batch reserved by [`LiveReplayStore::prepare_publication`].
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

type AbandonReservation = Arc<dyn Fn(&str) + Send + Sync>;

/// Opaque cursor reservation returned by [`LiveReplayStore::prepare_publication`].
///
/// Dropping an unpublished value invokes the store-provided retirement hook, so
/// reconnects crossing an abandoned batch can return `Gap(Unavailable)` rather
/// than mistaking missing history for a clean empty replay.
pub struct PreparedLiveReplayPublication {
    reservation_id: String,
    events: Vec<Arc<SessionObservationEvent>>,
    abandon: Option<AbandonReservation>,
}

impl PreparedLiveReplayPublication {
    pub fn new(
        reservation_id: impl Into<String>,
        events: Vec<Arc<SessionObservationEvent>>,
        abandon: impl Fn(&str) + Send + Sync + 'static,
    ) -> Result<Self, LiveReplayStoreError> {
        if events.is_empty() {
            return Err(LiveReplayStoreError::Store(
                "a prepared live replay publication must contain at least one event".to_string(),
            ));
        }
        Ok(Self {
            reservation_id: reservation_id.into(),
            events,
            abandon: Some(Arc::new(abandon)),
        })
    }

    /// A publication that holds no events: `prepare_publication` returns
    /// one when every draft redelivers an activity this buffer already
    /// holds, so nothing is reserved, settled, or announced.
    fn noop() -> Self {
        Self {
            reservation_id: String::new(),
            events: Vec::new(),
            abandon: None,
        }
    }

    /// Inspect the events whose cursors are reserved by this publication.
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn events(&self) -> &[Arc<SessionObservationEvent>] {
        &self.events
    }

    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    #[expect(
        clippy::expect_used,
        reason = "a prepared publication holds at least one event"
    )]
    pub fn latest_cursor(&self) -> &SessionCursor {
        &self
            .events
            .last()
            .expect("prepared publications are non-empty")
            .cursor
    }

    /// Consume the reservation for publication and disarm abandonment.
    pub fn into_parts(mut self) -> (String, Vec<Arc<SessionObservationEvent>>) {
        self.abandon = None;
        (
            std::mem::take(&mut self.reservation_id),
            std::mem::take(&mut self.events),
        )
    }
}

impl fmt::Debug for PreparedLiveReplayPublication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedLiveReplayPublication")
            .field("reservation_id", &self.reservation_id)
            .field("event_count", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl Drop for PreparedLiveReplayPublication {
    fn drop(&mut self) {
        if let Some(abandon) = self.abandon.take() {
            abandon(&self.reservation_id);
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

#[cfg(test)]
static LIVE_REPLAY_EVENT_CLONES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[inline]
fn clone_event(event: &Arc<SessionObservationEvent>) -> Arc<SessionObservationEvent> {
    #[cfg(test)]
    LIVE_REPLAY_EVENT_CLONES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Arc::clone(event)
}

pub struct LiveReplaySubscription {
    replay: VecDeque<Arc<SessionObservationEvent>>,
    receiver: ReusableBoxFuture<'static, LiveReplayRecvResult>,
    after_position: u64,
    closed: bool,
}

impl LiveReplaySubscription {
    fn new(
        replay: Vec<Arc<SessionObservationEvent>>,
        receiver: broadcast::Receiver<ReplayNotification>,
        after_position: u64,
    ) -> Self {
        Self {
            replay: replay.into(),
            receiver: ReusableBoxFuture::new(live_replay_recv(receiver)),
            after_position,
            closed: false,
        }
    }

    pub(super) fn contains_committed_at_or_after(&self, revision: SessionRevision) -> bool {
        self.replay.iter().any(|event| {
            event.revision() >= revision
                && matches!(
                    &event.payload,
                    SessionObservationEventPayload::Committed { .. }
                )
        })
    }
}

async fn live_replay_recv(
    mut receiver: broadcast::Receiver<ReplayNotification>,
) -> LiveReplayRecvResult {
    let result = receiver.recv().await;
    #[cfg(test)]
    if result.is_ok() {
        LIVE_REPLAY_EVENT_CLONES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    (result, receiver)
}

impl Stream for LiveReplaySubscription {
    type Item = Result<Arc<SessionObservationEvent>, LiveReplayStoreError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.replay.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
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

/// Bounded, best-effort live replay for host reconnects.
///
/// Runtime turn execution calls this trait from synchronous boundary code. All
/// methods must therefore be fast and nonblocking from the runtime's point of
/// view. A custom external store should expose local or buffered behavior here,
/// or offload blocking transport and durability work internally. Runtime turn
/// execution must not wait for slow network or storage durability in this path.
pub trait LiveReplayStore: Send + Sync {
    /// Reserve an ordered cursor batch without making it replay-visible.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn prepare_publication(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        events: Vec<LiveReplayEventDraft>,
    ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError>;

    /// Make a prepared batch replay-visible and notify subscribers in cursor order.
    ///
    /// This must be called only after the authoritative projection carrying
    /// `prepared.latest_cursor()` has been installed.
    fn publish_prepared(
        &self,
        prepared: PreparedLiveReplayPublication,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError>;

    /// This must be fast and nonblocking from the runtime's point of view.
    fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError>;

    /// Subscribe after `cursor`, replaying buffered events before live events.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn subscribe_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplaySubscribeOutcome, LiveReplayStoreError>;

    /// A runtime snapshot at revision N can race with a separate worker
    /// publishing revision N+1. The returned cursor must remain before that
    /// newer event so replay reconciles the stale snapshot.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn current_cursor(&self, session_id: &SessionId, revision: SessionRevision) -> SessionCursor;

    /// Mark this session's replay continuity unavailable without inventing a
    /// revision. Existing cursors must return `Gap(Unavailable)` and active
    /// subscriptions must close so observers reload their authoritative snapshot.
    /// A cursor acquired after that snapshot establishes fresh continuity.
    /// Pending publications must not restore continuity across this gap.
    /// This must be fast and nonblocking from the runtime's point of view.
    fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError>;

    /// This must be fast and nonblocking from the runtime's point of view.
    fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError>;
}

#[derive(Clone, Debug)]
pub struct InMemoryLiveReplayStoreConfig {
    pub max_events_per_session: usize,
    pub max_age: Duration,
    /// Maximum resident session entries across this store.
    pub max_sessions: usize,
    /// Maximum charged bytes across session metadata and reserved/retained events.
    pub max_retained_bytes: usize,
}

impl Default for InMemoryLiveReplayStoreConfig {
    fn default() -> Self {
        Self {
            max_events_per_session: DEFAULT_LIVE_REPLAY_CAPACITY,
            max_age: DEFAULT_LIVE_REPLAY_TTL,
            max_sessions: 4096,
            max_retained_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct InMemoryLiveReplayStore {
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
            replay_incarnation_id: uuid::Uuid::new_v4().to_string(),
            config,
            clock,
            sessions: Arc::new(StdMutex::new(ReplayRetention::default())),
            #[cfg(any(test, feature = "testing"))]
            before_notification_gate: None,
        }
    }

    /// Release all entries idle beyond `max_age`, including reservations and
    /// live channels. Hosts call this tick during traffic-free periods; normal
    /// store calls also perform a bounded amount of global expiry work.
    pub fn expire_idle_sessions(&self) -> usize {
        self.sessions
            .lock_recover()
            .expire(&self.config, self.clock.now(), usize::MAX)
    }

    pub fn with_bounds(max_events_per_session: usize, max_age: Duration) -> Self {
        Self::new(InMemoryLiveReplayStoreConfig {
            max_events_per_session,
            max_age,
            ..InMemoryLiveReplayStoreConfig::default()
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
            clock: Arc::clone(&self.clock),
            sessions: Arc::clone(&self.sessions),
            #[cfg(any(test, feature = "testing"))]
            before_notification_gate: self.before_notification_gate.clone(),
        }
    }
}

impl Default for InMemoryLiveReplayStore {
    fn default() -> Self {
        Self::new(InMemoryLiveReplayStoreConfig::default())
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
    tail_position: u64,
    settled_position: u64,
    unavailable_through: u64,
    reservations: BTreeMap<u64, ReservedPublication>,
    /// Delivered `TurnActivity` identities (`{replay key}#{ordinal}`) with
    /// the live position each holds in `events`. A replayed shift region or
    /// a journaled step re-executed after a mid-run suspension re-publishes
    /// the observations its first attempt already delivered; `prepare_publication`
    /// collapses those redeliveries into the stored copy (FIG-3753).
    delivered_activity_positions: HashMap<crate::TurnActivityId, u64>,
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
            settled_position: first_position,
            unavailable_through: 0,
            reservations: BTreeMap::new(),
            delivered_activity_positions: HashMap::new(),
            sender: None,
        }
    }

    /// Whether this activity identity is already appended to `events` or
    /// carried by an in-flight reservation. An `Abandoned` reservation never
    /// reached an observer, so it does not claim the identity.
    fn turn_activity_delivered(&self, id: &crate::TurnActivityId) -> bool {
        self.delivered_activity_positions.contains_key(id)
            || self.reservations.values().any(|reservation| {
                let events = match &reservation.state {
                    ReservedPublicationState::Pending(events)
                    | ReservedPublicationState::Ready(events) => events,
                    ReservedPublicationState::Abandoned => return false,
                };
                events.iter().any(|event| {
                    matches!(
                        &event.payload,
                        SessionObservationEventPayload::TurnActivity(activity)
                            if activity.id == *id
                    )
                })
            })
    }

    /// Drop the oldest stored event and release the activity identity it
    /// held, so a delivery after the replay window can land fresh.
    fn drop_front(&mut self) {
        if let Some(stored) = self.events.pop_front() {
            self.retained_bytes -= stored.retained_bytes;
            if let SessionObservationEventPayload::TurnActivity(activity) = &stored.event.payload
                && self.delivered_activity_positions.get(&activity.id) == Some(&stored.position)
            {
                self.delivered_activity_positions.remove(&activity.id);
            }
        }
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
    fn publish(&mut self, event: Arc<SessionObservationEvent>) {
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
                event: Arc::downgrade(&event),
            })
            .is_err()
        {
            self.sender = None;
            self.retained_bytes -= self.channel_bytes;
            self.channel_bytes = 0;
        }
    }

    fn reservation_mut(&mut self, reservation_id: &str) -> Option<&mut ReservedPublication> {
        self.reservations
            .values_mut()
            .find(|reservation| reservation.reservation_id == reservation_id)
    }
}

#[derive(Debug)]
struct ReservedPublication {
    retained_bytes: usize,
    event_bytes: Vec<usize>,
    reservation_id: String,
    end_position: u64,
    state: ReservedPublicationState,
}

#[derive(Debug)]
enum ReservedPublicationState {
    Pending(Vec<Arc<SessionObservationEvent>>),
    Ready(Vec<Arc<SessionObservationEvent>>),
    Abandoned,
}

#[derive(Clone, Debug)]
struct StoredObservationEvent {
    retained_bytes: usize,
    position: u64,
    appended_at: Instant,
    event: Arc<SessionObservationEvent>,
}

impl InMemoryLiveReplayStore {
    #[expect(
        clippy::expect_used,
        reason = "the store writes cursors in the parsable form"
    )]
    fn settle_ready(
        config: &InMemoryLiveReplayStoreConfig,
        buffer: &mut LiveReplaySessionBuffer,
        now: Instant,
    ) -> Vec<Arc<SessionObservationEvent>> {
        let mut notifications = Vec::new();
        loop {
            let next_position = buffer.settled_position.saturating_add(1);
            let Some(mut reservation) = buffer.reservations.remove(&next_position) else {
                break;
            };
            match reservation.state {
                ReservedPublicationState::Pending(events) => {
                    reservation.state = ReservedPublicationState::Pending(events);
                    buffer.reservations.insert(next_position, reservation);
                    break;
                }
                ReservedPublicationState::Ready(events) => {
                    buffer.retained_bytes -= reservation.retained_bytes;
                    for (event, retained_bytes) in events.into_iter().zip(reservation.event_bytes) {
                        buffer.retained_bytes += retained_bytes;
                        let position = event
                            .cursor
                            .parse()
                            .expect("store-created cursor must parse")
                            .live_position;
                        if let SessionObservationEventPayload::TurnActivity(activity) =
                            &event.payload
                        {
                            buffer
                                .delivered_activity_positions
                                .insert(activity.id.clone(), position);
                        }
                        buffer.events.push_back(StoredObservationEvent {
                            retained_bytes,
                            position,
                            appended_at: now,
                            event: clone_event(&event),
                        });
                        notifications.push(event);
                    }
                }
                ReservedPublicationState::Abandoned => {
                    buffer.retained_bytes -= reservation.retained_bytes;
                    buffer.unavailable_through =
                        buffer.unavailable_through.max(reservation.end_position);
                    if buffer.tail_position == reservation.end_position {
                        let retirement_position = reservation.end_position.saturating_add(1);
                        buffer.tail_position = retirement_position;
                        reservation.end_position = retirement_position;
                    }
                }
            }
            buffer.settled_position = reservation.end_position;
        }
        Self::trim_locked(config, buffer, now);
        notifications
    }

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
        if buffer.unavailable_through > 0 && cursor_position <= buffer.unavailable_through {
            return Some(LiveReplayGapReason::Unavailable);
        }
        let Some(first) = buffer.events.front() else {
            return (cursor_position < buffer.settled_position)
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
}

impl LiveReplayStore for InMemoryLiveReplayStore {
    fn prepare_publication(
        &self,
        session_id: &SessionId,
        revision: SessionRevision,
        drafts: Vec<LiveReplayEventDraft>,
    ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError> {
        if drafts.is_empty() {
            return Err(LiveReplayStoreError::Store(
                "cannot reserve an empty live replay publication".to_string(),
            ));
        }
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
        sessions.ensure_session(&self.config, session_id, now, &self.replay_incarnation_id)?;
        let (drafts, start_position, end_position, incarnation) = sessions
            .update(session_id, |buffer| {
                Self::trim_locked(&self.config, buffer, now);
                let mut claimed_activity_ids = HashSet::new();
                let drafts = drafts
                    .into_iter()
                    .filter(|draft| {
                        let SessionObservationEventPayload::TurnActivity(activity) = &draft.payload
                        else {
                            return true;
                        };
                        claimed_activity_ids.insert(activity.id.clone())
                            && !buffer.turn_activity_delivered(&activity.id)
                    })
                    .collect::<Vec<_>>();
                let start_position = buffer.tail_position.checked_add(1);
                let end_position = u64::try_from(drafts.len())
                    .ok()
                    .and_then(|count| buffer.tail_position.checked_add(count));
                (
                    drafts,
                    start_position,
                    end_position,
                    buffer.replay_incarnation_id.clone(),
                )
            })
            .ok_or_else(|| LiveReplayStoreError::Store("live replay session is missing".into()))?;
        if drafts.is_empty() {
            return Ok(PreparedLiveReplayPublication::noop());
        }
        let start_position = start_position
            .ok_or_else(|| LiveReplayStoreError::Store("live replay position overflow".into()))?;
        let end_position = end_position
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
        let event_bytes = match events
            .iter()
            .map(|event| bytes::event_bytes(event, self.config.max_retained_bytes))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(bytes) => bytes,
            Err(error) => {
                sessions.remove(session_id);
                return Err(error);
            }
        };
        let reservation_id = uuid::Uuid::new_v4().to_string();
        let retained_bytes = event_bytes
            .iter()
            .try_fold(
                std::mem::size_of::<ReservedPublication>() + reservation_id.len(),
                |bytes, event| {
                    bytes
                        .checked_add(*event)?
                        .checked_add(std::mem::size_of::<usize>())
                },
            )
            .ok_or_else(|| LiveReplayStoreError::Store("live replay byte count overflow".into()))?;
        if let Err(error) = sessions.reserve_bytes(&self.config, session_id, retained_bytes) {
            sessions.remove(session_id);
            return Err(error);
        }
        sessions.update(session_id, |buffer| {
            buffer.tail_position = end_position;
            buffer.retained_bytes += retained_bytes;
            buffer.reservations.insert(
                start_position,
                ReservedPublication {
                    retained_bytes,
                    event_bytes,
                    reservation_id: reservation_id.clone(),
                    end_position,
                    state: ReservedPublicationState::Pending(events.clone()),
                },
            );
        });
        drop(sessions);

        let sessions = Arc::downgrade(&self.sessions);
        let config = self.config.clone();
        let clock = Arc::clone(&self.clock);
        let abandoned_session_id = session_id.clone();
        PreparedLiveReplayPublication::new(reservation_id, events, move |reservation_id| {
            let Some(sessions) = sessions.upgrade() else {
                return;
            };
            let now = clock.now();
            let mut sessions = sessions.lock_recover();
            sessions.expire(&config, now, EXPIRY_WORK_PER_CALL);
            sessions.update(&abandoned_session_id, |buffer| {
                let Some(reservation) = buffer.reservation_mut(reservation_id) else {
                    return;
                };
                reservation.state = ReservedPublicationState::Abandoned;
                let notifications = InMemoryLiveReplayStore::settle_ready(&config, buffer, now);
                for event in notifications {
                    buffer.publish(event);
                }
            });
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "a prepared publication holds at least one event"
    )]
    fn publish_prepared(
        &self,
        prepared: PreparedLiveReplayPublication,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        let now = self.clock.now();
        let events = prepared.events.clone();
        if events.is_empty() {
            return Ok(events);
        }
        let session_id = events
            .first()
            .expect("prepared publications are non-empty")
            .session_id();
        let mut sessions = self.sessions.lock_recover();
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
        let notifications = sessions
            .update(&session_id, |buffer| {
                let reservation = buffer
                    .reservation_mut(&prepared.reservation_id)
                    .ok_or_else(|| {
                        LiveReplayStoreError::Store(
                            "prepared live replay reservation is missing or retired".into(),
                        )
                    })?;
                if !matches!(reservation.state, ReservedPublicationState::Pending(_)) {
                    return Err(LiveReplayStoreError::Store(
                        "prepared live replay reservation was already settled".into(),
                    ));
                }
                reservation.state = ReservedPublicationState::Ready(events.clone());
                Ok(Self::settle_ready(&self.config, buffer, now))
            })
            .ok_or_else(|| {
                LiveReplayStoreError::Store("prepared live replay session is missing".into())
            })??;
        sessions.touch(&session_id, now);
        #[cfg(any(test, feature = "testing"))]
        if let Some(gate) = self.before_notification_gate.as_ref()
            && !notifications.is_empty()
        {
            drop(sessions);
            (gate.0)(&notifications);
            sessions = self.sessions.lock_recover();
        }
        sessions
            .update(&session_id, |buffer| {
                let position = events[0].cursor.parse()?.live_position;
                if buffer.replay_incarnation_id != events[0].replay_incarnation_id()
                    || position < buffer.first_position
                {
                    return Err(LiveReplayStoreError::Store(
                        "published live replay session disappeared before notification".into(),
                    ));
                }
                for event in notifications {
                    buffer.publish(event);
                }
                Ok(())
            })
            .ok_or_else(|| {
                LiveReplayStoreError::Store(
                    "published live replay session disappeared before notification".into(),
                )
            })??;
        let _ = prepared.into_parts();
        Ok(events)
    }

    fn replay_after_cursor(
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
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
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

    fn subscribe_after_cursor(
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
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
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
                    receiver,
                    parsed.live_position,
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
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
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

    fn invalidate_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        let mut sessions = self.sessions.lock_recover();
        sessions.remove(session_id);
        sessions.expire(&self.config, self.clock.now(), EXPIRY_WORK_PER_CALL);
        Ok(())
    }

    fn trim_session(&self, session_id: &SessionId) -> Result<(), LiveReplayStoreError> {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        sessions.touch(session_id, now);
        sessions.update(session_id, |buffer| {
            Self::trim_locked(&self.config, buffer, now)
        });
        sessions.expire(&self.config, now, EXPIRY_WORK_PER_CALL);
        Ok(())
    }
}

#[cfg(test)]
#[path = "replay/tests.rs"]
mod tests;
