use lash_sansio::sync::MutexExt;
use std::collections::{HashMap, VecDeque};
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
    closed: bool,
}

impl LiveReplaySubscription {
    fn new(
        replay: Vec<Arc<SessionObservationEvent>>,
        receiver: broadcast::Receiver<Arc<SessionObservationEvent>>,
    ) -> Self {
        Self {
            replay: replay.into(),
            receiver: ReusableBoxFuture::new(live_replay_recv(receiver)),
            closed: false,
        }
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
            Ok(event) => Poll::Ready(Some(Ok(event))),
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
    /// Append one observation event and return its assigned cursor.
    ///
    /// This must be fast and nonblocking from the runtime's point of view.
    fn append(
        &self,
        session_id: &str,
        revision: SessionRevision,
        turn_id: Option<&str>,
        payload: SessionObservationEventPayload,
    ) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError>;

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
    sessions: StdMutex<HashMap<String, LiveReplaySessionBuffer>>,
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
            sessions: StdMutex::new(HashMap::new()),
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
    sender: Option<broadcast::Sender<Arc<SessionObservationEvent>>>,
}

impl LiveReplaySessionBuffer {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            tail_position: 0,
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

#[derive(Clone, Debug)]
struct StoredObservationEvent {
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
}

impl LiveReplayStore for InMemoryLiveReplayStore {
    fn append(
        &self,
        session_id: &str,
        revision: SessionRevision,
        turn_id: Option<&str>,
        payload: SessionObservationEventPayload,
    ) -> Result<Arc<SessionObservationEvent>, LiveReplayStoreError> {
        let now = self.clock.now();
        let mut sessions = self.sessions.lock_recover();
        let buffer = sessions
            .entry(session_id.to_string())
            .or_insert_with(LiveReplaySessionBuffer::new);
        buffer.tail_position = buffer.tail_position.checked_add(1).ok_or_else(|| {
            LiveReplayStoreError::Store("live replay position overflow".to_string())
        })?;
        let cursor = SessionCursor::new(session_id, revision, buffer.tail_position);
        let event = Arc::new(SessionObservationEvent {
            session_id: session_id.to_string(),
            replay_incarnation_id: self.replay_incarnation_id.clone(),
            turn_id: turn_id.map(str::to_string),
            revision,
            cursor,
            payload,
        });
        buffer.events.push_back(StoredObservationEvent {
            position: buffer.tail_position,
            appended_at: now,
            event: clone_event(&event),
        });
        Self::trim_locked(&self.config, buffer, now);
        buffer.publish(clone_event(&event));
        Ok(event)
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
            LiveReplaySubscription::new(replay, receiver),
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
            .append("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .append("s", SessionRevision(0), None, activity("b"))
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
            .append("s", SessionRevision(2), None, activity("worker commit"))
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
            .append("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .append("s", SessionRevision(0), None, activity("b"))
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
            .append("s", SessionRevision(0), None, activity("a"))
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
            .append("s", SessionRevision(0), None, activity("a"))
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
            .append("s", SessionRevision(0), None, activity("b"))
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
                .append(
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
            .append("s", SessionRevision(0), None, activity("a"))
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
            .append("s", SessionRevision(0), None, activity("b"))
            .expect("append b");
        let sessions = store.sessions.lock_recover();
        assert!(sessions.get("s").expect("buffer").sender.is_none());
    }

    #[test]
    fn in_memory_replay_subscription_reports_gap_after_capacity_trim() {
        let store = InMemoryLiveReplayStore::with_bounds(1, Duration::from_secs(120));
        let start = store.current_cursor("s", SessionRevision(0));
        store
            .append("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        store
            .append("s", SessionRevision(0), None, activity("b"))
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
            .append("s", SessionRevision(0), None, activity("a"))
            .expect("append a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            store.subscribe_after_cursor(&start).expect("subscribe"),
            LiveReplaySubscribeOutcome::Gap(LiveReplayGapReason::Trimmed)
        ));
    }
}
