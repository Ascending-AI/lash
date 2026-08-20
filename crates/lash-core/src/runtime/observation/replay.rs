use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio_util::sync::ReusableBoxFuture;

use crate::runtime::LashRuntime;
#[cfg(test)]
use crate::runtime::RuntimeSessionState;

const SESSION_CURSOR_PREFIX: &str = "lashsc1:";
const DEFAULT_LIVE_REPLAY_CAPACITY: usize = 2048;
const DEFAULT_LIVE_REPLAY_TTL: Duration = Duration::from_secs(120);

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
        Self(if runtime.state.checkpoint_ref.is_some() {
            runtime.state.head_revision
        } else {
            runtime.state.turn_index as u64
        })
    }

    #[cfg(test)]
    pub(super) fn from_state(state: &RuntimeSessionState) -> Self {
        Self(if state.checkpoint_ref.is_some() {
            state.head_revision
        } else {
            state.turn_index as u64
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SessionCursor(String);

impl SessionCursor {
    pub(crate) fn new(
        session_id: impl AsRef<str>,
        revision: SessionRevision,
        live_position: u64,
    ) -> Self {
        Self(format!(
            "{SESSION_CURSOR_PREFIX}{}:{live_position}:{}",
            revision.0,
            session_id.as_ref()
        ))
    }

    /// Validate and adopt a cursor token produced by a custom live-replay store.
    ///
    /// Lash keeps the token opaque to ordinary consumers, while custom store
    /// implementations need to return their persisted cursor values through
    /// [`SessionObservationEvent`] and [`LiveReplayStore::current_cursor`].
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

    pub(crate) fn parse_for_session(
        &self,
        expected_session_id: &str,
    ) -> Result<ParsedSessionCursor, SessionCursorError> {
        let parsed = self.parse()?;
        if parsed.session_id != expected_session_id {
            return Err(SessionCursorError::WrongSession {
                expected_session_id: expected_session_id.to_string(),
                actual_session_id: parsed.session_id,
            });
        }
        Ok(parsed)
    }

    fn parse(&self) -> Result<ParsedSessionCursor, SessionCursorError> {
        let payload = self.0.strip_prefix(SESSION_CURSOR_PREFIX).ok_or_else(|| {
            SessionCursorError::Malformed {
                message: "missing cursor prefix".to_string(),
            }
        })?;
        let mut parts = payload.splitn(3, ':');
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
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SessionCursorError::Malformed {
                message: "missing session id".to_string(),
            })?
            .to_string();
        Ok(ParsedSessionCursor {
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
pub(crate) struct ParsedSessionCursor {
    pub session_id: String,
    pub revision: SessionRevision,
    pub live_position: u64,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionCursorError {
    #[error("malformed session cursor: {message}")]
    Malformed { message: String },
    #[error("session cursor belongs to `{actual_session_id}`, not `{expected_session_id}`")]
    WrongSession {
        expected_session_id: String,
        actual_session_id: String,
    },
}

#[derive(Clone, Debug)]
pub struct SessionObservation {
    pub read_view: crate::SessionReadView,
    pub cursor: SessionCursor,
}

#[derive(Clone, Debug)]
pub struct SessionObservationEvent {
    pub session_id: String,
    pub replay_incarnation_id: String,
    pub turn_id: Option<String>,
    pub revision: SessionRevision,
    pub cursor: SessionCursor,
    pub payload: SessionObservationEventPayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionQueueEventKind {
    Enqueued,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProcessEventKind {
    Started,
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
        process_ids: Vec<String>,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LiveReplayGap {
    pub session_id: String,
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
    pub turn_id: Option<String>,
    pub payload: SessionObservationEventPayload,
}

impl LiveReplayEventDraft {
    /// Construct one event in a publication reservation.
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn new(
        turn_id: Option<impl Into<String>>,
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
    /// Construct a prepared publication for a custom store implementation.
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

    /// Inspect the events whose cursors are reserved by this publication.
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
    pub fn events(&self) -> &[Arc<SessionObservationEvent>] {
        &self.events
    }

    /// Return the cursor at the end of this reserved publication.
    ///
    /// Integrator class (ADR 0051): **custom live-replay store implementors**.
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

type LiveReplayRecvResult = (
    Result<Arc<SessionObservationEvent>, broadcast::error::RecvError>,
    broadcast::Receiver<Arc<SessionObservationEvent>>,
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
        receiver: broadcast::Receiver<Arc<SessionObservationEvent>>,
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
            event.revision >= revision
                && matches!(
                    &event.payload,
                    SessionObservationEventPayload::Committed { .. }
                )
        })
    }
}

async fn live_replay_recv(
    mut receiver: broadcast::Receiver<Arc<SessionObservationEvent>>,
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
            Ok(event) => {
                let position = event
                    .cursor
                    .parse()
                    .expect("store-created live event cursor must parse")
                    .live_position;
                if position <= self.after_position {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    self.after_position = position;
                    Poll::Ready(Some(Ok(event)))
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
        session_id: &str,
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

    /// Return buffered events after `cursor`, or report a recoverable gap.
    ///
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

    /// Return the latest cursor known locally for a session without skipping
    /// buffered events newer than `revision`.
    ///
    /// A runtime snapshot at revision N can race with a separate worker
    /// publishing revision N+1. The returned cursor must remain before that
    /// newer event so replay reconciles the stale snapshot.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn current_cursor(&self, session_id: &str, revision: SessionRevision) -> SessionCursor;

    /// Apply best-effort retention trimming for a session.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn trim_session(&self, session_id: &str) -> Result<(), LiveReplayStoreError>;
}

#[derive(Clone, Debug)]
pub struct InMemoryLiveReplayStoreConfig {
    pub max_events_per_session: usize,
    pub max_age: Duration,
}

impl Default for InMemoryLiveReplayStoreConfig {
    fn default() -> Self {
        Self {
            max_events_per_session: DEFAULT_LIVE_REPLAY_CAPACITY,
            max_age: DEFAULT_LIVE_REPLAY_TTL,
        }
    }
}

#[derive(Debug)]
pub struct InMemoryLiveReplayStore {
    replay_incarnation_id: String,
    config: InMemoryLiveReplayStoreConfig,
    clock: Arc<dyn crate::Clock>,
    sessions: Arc<StdMutex<HashMap<String, LiveReplaySessionBuffer>>>,
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
            sessions: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub fn with_bounds(max_events_per_session: usize, max_age: Duration) -> Self {
        Self::new(InMemoryLiveReplayStoreConfig {
            max_events_per_session,
            max_age,
        })
    }
}

impl Default for InMemoryLiveReplayStore {
    fn default() -> Self {
        Self::new(InMemoryLiveReplayStoreConfig::default())
    }
}

#[derive(Debug)]
struct LiveReplaySessionBuffer {
    events: VecDeque<StoredObservationEvent>,
    tail_position: u64,
    settled_position: u64,
    unavailable_through: u64,
    reservations: BTreeMap<u64, ReservedPublication>,
    sender: Option<broadcast::Sender<Arc<SessionObservationEvent>>>,
}

impl LiveReplaySessionBuffer {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            tail_position: 0,
            settled_position: 0,
            unavailable_through: 0,
            reservations: BTreeMap::new(),
            sender: None,
        }
    }

    fn subscribe(
        &mut self,
        channel_capacity: usize,
    ) -> broadcast::Receiver<Arc<SessionObservationEvent>> {
        match self.sender.as_ref() {
            Some(sender) => sender.subscribe(),
            None => {
                let (sender, receiver) = broadcast::channel(channel_capacity.max(1));
                self.sender = Some(sender);
                receiver
            }
        }
    }

    fn publish(&mut self, event: Arc<SessionObservationEvent>) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        if sender.send(event).is_err() {
            self.sender = None;
        }
    }
}

#[derive(Debug)]
struct ReservedPublication {
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
    position: u64,
    appended_at: Instant,
    event: Arc<SessionObservationEvent>,
}

impl InMemoryLiveReplayStore {
    fn settle_ready(
        config: &InMemoryLiveReplayStoreConfig,
        buffer: &mut LiveReplaySessionBuffer,
        now: Instant,
    ) {
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
                    for event in events {
                        let position = event
                            .cursor
                            .parse()
                            .expect("store-created cursor must parse")
                            .live_position;
                        buffer.events.push_back(StoredObservationEvent {
                            position,
                            appended_at: now,
                            event: clone_event(&event),
                        });
                        buffer.publish(event);
                    }
                }
                ReservedPublicationState::Abandoned => {
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
    }

    fn trim_locked(
        config: &InMemoryLiveReplayStoreConfig,
        buffer: &mut LiveReplaySessionBuffer,
        now: Instant,
    ) {
        while buffer.events.len() > config.max_events_per_session {
            buffer.events.pop_front();
        }
        while buffer
            .events
            .front()
            .is_some_and(|event| now.duration_since(event.appended_at) > config.max_age)
        {
            buffer.events.pop_front();
        }
    }

    fn gap_reason_for_cursor(
        buffer: Option<&LiveReplaySessionBuffer>,
        cursor_position: u64,
    ) -> Option<LiveReplayGapReason> {
        let Some(buffer) = buffer else {
            return (cursor_position > 0).then_some(LiveReplayGapReason::Unavailable);
        };
        if cursor_position > buffer.tail_position {
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
}

impl LiveReplayStore for InMemoryLiveReplayStore {
    fn prepare_publication(
        &self,
        session_id: &str,
        revision: SessionRevision,
        drafts: Vec<LiveReplayEventDraft>,
    ) -> Result<PreparedLiveReplayPublication, LiveReplayStoreError> {
        if drafts.is_empty() {
            return Err(LiveReplayStoreError::Store(
                "cannot reserve an empty live replay publication".to_string(),
            ));
        }
        let mut sessions = self.sessions.lock_recover();
        let buffer = sessions
            .entry(session_id.to_string())
            .or_insert_with(LiveReplaySessionBuffer::new);
        let start_position = buffer.tail_position.checked_add(1).ok_or_else(|| {
            LiveReplayStoreError::Store("live replay position overflow".to_string())
        })?;
        let event_count = u64::try_from(drafts.len()).map_err(|_| {
            LiveReplayStoreError::Store("live replay batch length overflow".to_string())
        })?;
        let end_position = buffer
            .tail_position
            .checked_add(event_count)
            .ok_or_else(|| {
                LiveReplayStoreError::Store("live replay position overflow".to_string())
            })?;
        let events = drafts
            .into_iter()
            .enumerate()
            .map(|(offset, draft)| {
                let position = start_position + offset as u64;
                Arc::new(SessionObservationEvent {
                    session_id: session_id.to_string(),
                    replay_incarnation_id: self.replay_incarnation_id.clone(),
                    turn_id: draft.turn_id,
                    revision,
                    cursor: SessionCursor::new(session_id, revision, position),
                    payload: draft.payload,
                })
            })
            .collect::<Vec<_>>();
        let reservation_id = uuid::Uuid::new_v4().to_string();
        buffer.tail_position = end_position;
        buffer.reservations.insert(
            start_position,
            ReservedPublication {
                reservation_id: reservation_id.clone(),
                end_position,
                state: ReservedPublicationState::Pending(events.clone()),
            },
        );
        drop(sessions);

        let sessions = Arc::clone(&self.sessions);
        let config = self.config.clone();
        let clock = Arc::clone(&self.clock);
        let abandoned_session_id = session_id.to_string();
        PreparedLiveReplayPublication::new(reservation_id, events, move |reservation_id| {
            let now = clock.now();
            let mut sessions = sessions.lock_recover();
            let Some(buffer) = sessions.get_mut(&abandoned_session_id) else {
                return;
            };
            let Some(reservation) = buffer
                .reservations
                .values_mut()
                .find(|reservation| reservation.reservation_id == reservation_id)
            else {
                return;
            };
            reservation.state = ReservedPublicationState::Abandoned;
            InMemoryLiveReplayStore::settle_ready(&config, buffer, now);
        })
    }

    fn publish_prepared(
        &self,
        prepared: PreparedLiveReplayPublication,
    ) -> Result<Vec<Arc<SessionObservationEvent>>, LiveReplayStoreError> {
        let now = self.clock.now();
        let reservation_id = prepared.reservation_id.clone();
        let events = prepared.events.clone();
        let session_id = events
            .first()
            .expect("prepared publications are non-empty")
            .session_id
            .clone();
        let mut sessions = self.sessions.lock_recover();
        let buffer = sessions.get_mut(&session_id).ok_or_else(|| {
            LiveReplayStoreError::Store("prepared live replay session is missing".to_string())
        })?;
        let reservation = buffer
            .reservations
            .values_mut()
            .find(|reservation| reservation.reservation_id == reservation_id)
            .ok_or_else(|| {
                LiveReplayStoreError::Store(
                    "prepared live replay reservation is missing or retired".to_string(),
                )
            })?;
        if !matches!(reservation.state, ReservedPublicationState::Pending(_)) {
            return Err(LiveReplayStoreError::Store(
                "prepared live replay reservation was already settled".to_string(),
            ));
        }
        reservation.state = ReservedPublicationState::Ready(events.clone());
        Self::settle_ready(&self.config, buffer, now);
        let _ = prepared.into_parts();
        Ok(events)
    }

    fn replay_after_cursor(
        &self,
        cursor: &SessionCursor,
    ) -> Result<LiveReplayOutcome, LiveReplayStoreError> {
        let parsed = cursor.parse()?;
        let _cursor_revision = parsed.revision;
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        if let Some(buffer) = sessions.get_mut(&parsed.session_id) {
            Self::trim_locked(&self.config, buffer, now);
        }
        let buffer = sessions.get(&parsed.session_id);
        if let Some(reason) = Self::gap_reason_for_cursor(buffer, parsed.live_position) {
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
        let _cursor_revision = parsed.revision;
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        let buffer = sessions
            .entry(parsed.session_id.clone())
            .or_insert_with(LiveReplaySessionBuffer::new);
        Self::trim_locked(&self.config, buffer, now);
        if let Some(reason) = Self::gap_reason_for_cursor(Some(buffer), parsed.live_position) {
            return Ok(LiveReplaySubscribeOutcome::Gap(reason));
        }
        let replay = buffer
            .events
            .iter()
            .filter(|event| event.position > parsed.live_position)
            .map(|event| clone_event(&event.event))
            .collect();
        let receiver = buffer.subscribe(self.config.max_events_per_session);
        Ok(LiveReplaySubscribeOutcome::Subscribed(
            LiveReplaySubscription::new(replay, receiver, parsed.live_position),
        ))
    }

    fn current_cursor(&self, session_id: &str, revision: SessionRevision) -> SessionCursor {
        let live_position = self
            .sessions
            .lock_recover()
            .get(session_id)
            .map(|buffer| {
                buffer
                    .events
                    .iter()
                    .find(|stored| stored.event.revision > revision)
                    .map_or(buffer.tail_position, |stored| {
                        stored.position.saturating_sub(1)
                    })
            })
            .unwrap_or(0);
        SessionCursor::new(session_id, revision, live_position)
    }

    fn trim_session(&self, session_id: &str) -> Result<(), LiveReplayStoreError> {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        if let Some(buffer) = sessions.get_mut(session_id) {
            Self::trim_locked(&self.config, buffer, now);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    impl InMemoryLiveReplayStore {
        fn publish_test_event(
            &self,
            session_id: &str,
            revision: SessionRevision,
            turn_id: Option<&str>,
            payload: SessionObservationEventPayload,
        ) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
            let prepared = self.prepare_publication(
                session_id,
                revision,
                vec![LiveReplayEventDraft::new(turn_id, payload)],
            )?;
            self.publish_prepared(prepared)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    LiveReplayStoreError::Store("published test batch was empty".to_string())
                })
        }
    }

    struct CountingAllocator;

    static ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
    static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            // SAFETY: forwarding the allocator contract unchanged to System.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` and `layout` came from the forwarded System allocation.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

    fn activity(text: &str) -> SessionObservationEventPayload {
        SessionObservationEventPayload::TurnActivity(crate::TurnActivity::independent(
            crate::TurnEvent::AssistantProseDelta { text: text.into() },
        ))
    }

    #[test]
    fn session_cursor_round_trips_and_debug_is_opaque() {
        let cursor = SessionCursor::new("session:with:colon", SessionRevision(3), 9);
        let encoded = serde_json::to_string(&cursor).expect("serialize");
        let decoded: SessionCursor = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, cursor);
        assert_eq!(format!("{cursor:?}"), "SessionCursor(<opaque>)");
        let parsed = cursor
            .parse_for_session("session:with:colon")
            .expect("parse");
        assert_eq!(parsed.revision, SessionRevision(3));
        assert_eq!(parsed.live_position, 9);
        assert_eq!(
            SessionCursor::from_store_token(cursor.as_str()).expect("adopt store token"),
            cursor
        );
        assert!(SessionCursor::from_store_token("not-a-cursor").is_err());
    }

    #[test]
    fn reserved_cursors_are_valid_until_publication_and_abandonment_forces_gap() {
        let store = InMemoryLiveReplayStore::default();
        let revision = SessionRevision::new(1);
        let start = store.current_cursor("reserved", revision);
        let prepared = store
            .prepare_publication(
                "reserved",
                revision,
                vec![LiveReplayEventDraft::new(
                    None::<String>,
                    activity("reserved"),
                )],
            )
            .expect("reserve publication");
        let reserved = prepared.latest_cursor().clone();

        assert!(matches!(
            store.replay_after_cursor(&reserved),
            Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
        ));
        assert!(matches!(
            store.subscribe_after_cursor(&reserved),
            Ok(LiveReplaySubscribeOutcome::Subscribed(_))
        ));

        drop(prepared);
        assert!(matches!(
            store.replay_after_cursor(&reserved),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ));
        assert!(matches!(
            store.replay_after_cursor(&start),
            Ok(LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable))
        ));
        assert!(matches!(
            store.subscribe_after_cursor(&start),
            Ok(LiveReplaySubscribeOutcome::Gap(
                LiveReplayGapReason::Unavailable
            ))
        ));
        let retired = store.current_cursor("reserved", revision);
        assert!(matches!(
            store.replay_after_cursor(&retired),
            Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
        ));
    }

    #[test]
    fn prepared_batches_become_visible_in_reserved_cursor_order() {
        let store = InMemoryLiveReplayStore::default();
        let revision = SessionRevision::new(1);
        let start = store.current_cursor("ordered", revision);
        let first = store
            .prepare_publication(
                "ordered",
                revision,
                vec![LiveReplayEventDraft::new(None::<String>, activity("first"))],
            )
            .expect("reserve first publication");
        let second = store
            .prepare_publication(
                "ordered",
                revision,
                vec![LiveReplayEventDraft::new(
                    None::<String>,
                    activity("second"),
                )],
            )
            .expect("reserve second publication");

        store
            .publish_prepared(second)
            .expect("mark second publication ready");
        assert!(matches!(
            store.replay_after_cursor(&start),
            Ok(LiveReplayOutcome::Replayed(events)) if events.is_empty()
        ));
        store
            .publish_prepared(first)
            .expect("publish first and flush ready suffix");
        let LiveReplayOutcome::Replayed(events) = store
            .replay_after_cursor(&start)
            .expect("replay ordered publications")
        else {
            panic!("ordered publications must remain replayable");
        };
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].payload,
            SessionObservationEventPayload::TurnActivity(activity)
                if matches!(&activity.event, crate::TurnEvent::AssistantProseDelta { text } if text.as_ref() == "first")
        ));
        assert!(matches!(
            &events[1].payload,
            SessionObservationEventPayload::TurnActivity(activity)
                if matches!(&activity.event, crate::TurnEvent::AssistantProseDelta { text } if text.as_ref() == "second")
        ));
    }

    #[test]
    fn session_cursor_rejects_malformed_and_wrong_session() {
        let malformed = SessionCursor::from_raw_for_testing("bad");
        assert!(matches!(
            malformed.parse_for_session("s"),
            Err(SessionCursorError::Malformed { .. })
        ));
        let cursor = SessionCursor::new("actual", SessionRevision(0), 0);
        assert!(matches!(
            cursor.parse_for_session("expected"),
            Err(SessionCursorError::WrongSession { .. })
        ));
    }

    #[test]
    fn in_memory_replay_store_replays_after_cursor_in_order() {
        let store = InMemoryLiveReplayStore::default();
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .publish_test_event("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        let LiveReplayOutcome::Replayed(events) =
            store.replay_after_cursor(&start).expect("replay")
        else {
            panic!("expected replay");
        };
        assert_eq!(events.len(), 2);
        match &events[0].payload {
            SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
                crate::TurnEvent::AssistantProseDelta { text } => assert_eq!(text.as_ref(), "a"),
                _ => panic!("wrong event"),
            },
            _ => panic!("wrong payload"),
        }
    }

    #[test]
    fn current_cursor_for_stale_snapshot_replays_newer_revision_events() {
        let store = InMemoryLiveReplayStore::default();
        store
            .publish_test_event("s", SessionRevision(2), None, activity("worker commit"))
            .expect("append newer worker commit");

        // A runtime can finish loading durable revision 1 just before a separate
        // worker publishes revision 2. Its initial cursor must not skip that
        // newer event merely because the live-replay tail already advanced.
        let stale_snapshot_cursor = store.current_cursor("s", SessionRevision(1));
        let LiveReplayOutcome::Replayed(events) = store
            .replay_after_cursor(&stale_snapshot_cursor)
            .expect("replay from stale snapshot")
        else {
            panic!("expected replay");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].revision, SessionRevision(2));
    }

    #[test]
    fn in_memory_replay_store_reports_gap_after_capacity_trim() {
        let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .publish_test_event("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        assert!(matches!(
            store.replay_after_cursor(&start).expect("gap"),
            LiveReplayOutcome::Gap(LiveReplayGapReason::Trimmed)
        ));
    }

    #[test]
    fn in_memory_replay_store_reports_gap_after_ttl_trim() {
        let store = InMemoryLiveReplayStore::with_bounds(16, Duration::from_millis(1));
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            store.replay_after_cursor(&start).expect("gap"),
            LiveReplayOutcome::Gap(LiveReplayGapReason::Trimmed)
        ));
    }

    #[test]
    fn in_memory_replay_store_reports_unavailable_for_cursor_ahead_of_tail() {
        let store = InMemoryLiveReplayStore::default();
        let ahead = SessionCursor::new("s", SessionRevision(0), 99);
        assert!(matches!(
            store.replay_after_cursor(&ahead).expect("gap"),
            LiveReplayOutcome::Gap(LiveReplayGapReason::Unavailable)
        ));
    }

    #[tokio::test]
    async fn in_memory_replay_subscription_yields_replay_then_live() {
        let store = InMemoryLiveReplayStore::default();
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        let LiveReplaySubscribeOutcome::Subscribed(mut subscription) =
            store.subscribe_after_cursor(&start).expect("subscribe")
        else {
            panic!("expected subscription");
        };
        let first = futures_util::StreamExt::next(&mut subscription)
            .await
            .expect("subscription open")
            .expect("replay");
        assert_eq!(first.session_id, "s");
        store
            .publish_test_event("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        let second = futures_util::StreamExt::next(&mut subscription)
            .await
            .expect("subscription open")
            .expect("live");
        match &second.payload {
            SessionObservationEventPayload::TurnActivity(activity) => match &activity.event {
                crate::TurnEvent::AssistantProseDelta { text } => assert_eq!(text.as_ref(), "b"),
                _ => panic!("wrong event"),
            },
            _ => panic!("wrong payload"),
        }
    }

    #[tokio::test]
    #[ignore = "manual lane-O allocation measurement"]
    async fn measure_streamed_token_allocations() {
        const TOKENS: usize = 1_000;
        let store = InMemoryLiveReplayStore::with_bounds(TOKENS + 1, Duration::from_secs(120));
        let mut cursor = store.current_cursor("perf-session", SessionRevision(7));
        let LiveReplaySubscribeOutcome::Subscribed(mut subscription) = store
            .subscribe_after_cursor(&cursor)
            .expect("subscribe for allocation measurement")
        else {
            panic!("expected subscription");
        };

        ALLOCATION_COUNT.store(0, Ordering::SeqCst);
        ALLOCATED_BYTES.store(0, Ordering::SeqCst);
        LIVE_REPLAY_EVENT_CLONES.store(0, Ordering::SeqCst);
        for ordinal in 0..TOKENS {
            let event = store
                .publish_test_event(
                    "perf-session",
                    SessionRevision(7),
                    None,
                    activity(&format!("token-{ordinal}")),
                )
                .expect("append token event");
            let live = futures_util::StreamExt::next(&mut subscription)
                .await
                .expect("subscription open")
                .expect("receive live event");
            assert_eq!(live.cursor, event.cursor);
            let LiveReplayOutcome::Replayed(replayed) = store
                .replay_after_cursor(&cursor)
                .expect("replay token event")
            else {
                panic!("expected replay");
            };
            assert_eq!(replayed.len(), 1);
            cursor = event.cursor.clone();
        }
        let allocations = ALLOCATION_COUNT.load(Ordering::SeqCst);
        let bytes = ALLOCATED_BYTES.load(Ordering::SeqCst);
        let event_clones = LIVE_REPLAY_EVENT_CLONES.load(Ordering::SeqCst);
        eprintln!(
            "streamed-token allocations: total={allocations} per_token={:.3} bytes_total={bytes} bytes_per_token={:.3} deep_event_clones_per_token=0 arc_handle_clones_per_token={:.3}",
            allocations as f64 / TOKENS as f64,
            bytes as f64 / TOKENS as f64,
            event_clones as f64 / TOKENS as f64,
        );
    }

    #[test]
    fn in_memory_replay_store_allocates_live_channel_lazily() {
        let store = InMemoryLiveReplayStore::default();
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        {
            let sessions = store.sessions.lock_recover();
            assert!(sessions.get("s").expect("buffer").sender.is_none());
        }
        let LiveReplaySubscribeOutcome::Subscribed(subscription) =
            store.subscribe_after_cursor(&start).expect("subscribe")
        else {
            panic!("expected subscription");
        };
        {
            let sessions = store.sessions.lock_recover();
            assert!(sessions.get("s").expect("buffer").sender.is_some());
        }
        drop(subscription);
        store
            .publish_test_event("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        let sessions = store.sessions.lock_recover();
        assert!(sessions.get("s").expect("buffer").sender.is_none());
    }

    #[test]
    fn in_memory_replay_subscription_reports_gap_after_capacity_trim() {
        let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .publish_test_event("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        assert!(matches!(
            store.subscribe_after_cursor(&start).expect("subscribe"),
            LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
        ));
    }

    #[test]
    fn in_memory_replay_subscription_reports_gap_after_ttl_trim() {
        let store = InMemoryLiveReplayStore::with_bounds(16, Duration::from_millis(1));
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .publish_test_event("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            store.subscribe_after_cursor(&start).expect("subscribe"),
            LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
        ));
    }
}
