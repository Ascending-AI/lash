//! The in-memory process replay store: one bounded window per process.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures_util::Stream;
use lash_sansio::sync::MutexExt;
use tokio::sync::broadcast;
use tokio_util::sync::ReusableBoxFuture;

use super::{
    ParsedProcessObservationCursor, ProcessObservationCursor, ProcessObservationEvent,
    ProcessObservationIdentity, ProcessReplayEventDraft, ProcessReplayGapReason, ProcessReplayItem,
    ProcessReplayOutcome, ProcessReplayStore, ProcessReplayStoreError,
    ProcessReplaySubscribeOutcome, ProcessReplaySubscription, ProcessSequence,
};
use crate::ProcessId;

/// What an [`InMemoryProcessReplayStore`] retains for process observers. The
/// host states it; there is no default (D-DEFAULTS2).
///
/// A process's window is cut by whichever bound it reaches first, so the
/// replay it can offer lasts about
/// `min(max_age, max_events / events per second, max_bytes / bytes per second)`:
/// at 1,000 events a second, 2,048 events are two seconds, not 120.
#[derive(Clone, Debug)]
pub struct InMemoryProcessReplayStoreConfig {
    /// The most recent events one process's window keeps.
    pub max_events_per_process: usize,
    /// How long an event stays replayable, whether or not the process has
    /// ended and whether or not anyone is subscribed.
    pub max_age: Duration,
    /// Maximum charged bytes of one process's retained events.
    pub max_bytes_per_process: usize,
    /// Maximum resident process windows across this store.
    pub max_processes: usize,
    /// Maximum charged bytes across window metadata and retained events.
    pub max_retained_bytes: usize,
}

impl InMemoryProcessReplayStoreConfig {
    /// The standard retention, a provisional preset: 2,048 events per
    /// process, replayable for 120 seconds, 8 MiB per process, across at
    /// most 4,096 processes and 64 MiB. No measurement backs these values.
    pub const fn standard() -> Self {
        Self {
            max_events_per_process: 2048,
            max_age: Duration::from_secs(120),
            max_bytes_per_process: 8 * 1024 * 1024,
            max_processes: 4096,
            max_retained_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct InMemoryProcessReplayStore {
    work_limits: lash_trace::ObservationWorkLimits,
    replay_incarnation_id: String,
    config: InMemoryProcessReplayStoreConfig,
    clock: Arc<dyn crate::Clock>,
    windows: Arc<StdMutex<Windows>>,
}

impl InMemoryProcessReplayStore {
    pub fn new(config: InMemoryProcessReplayStoreConfig) -> Self {
        Self::with_clock(config, Arc::new(crate::SystemClock))
    }

    pub fn with_clock(
        config: InMemoryProcessReplayStoreConfig,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            work_limits: lash_trace::ObservationWorkLimits::standard(),
            replay_incarnation_id: uuid::Uuid::new_v4().to_string(),
            config,
            clock,
            windows: Arc::new(StdMutex::new(Windows::default())),
        }
    }

    /// Configure bounded expiry work per store operation, separately from
    /// retention.
    #[must_use]
    pub fn with_work_limits(mut self, limits: lash_trace::ObservationWorkLimits) -> Self {
        self.work_limits = limits;
        self
    }

    /// Release every window idle beyond `max_age`. Hosts call this in
    /// traffic-free periods; ordinary store calls also do a bounded amount
    /// of this work.
    pub fn expire_idle_processes(&self) -> usize {
        self.windows
            .lock_recover()
            .expire(&self.config, self.clock.now(), usize::MAX)
    }

    /// The event bytes charged to one process's window.
    #[cfg(test)]
    pub(super) fn event_bytes(&self, process_id: &ProcessId) -> usize {
        self.windows
            .lock_recover()
            .windows
            .get(process_id)
            .map_or(0, |window| window.event_bytes)
    }

    /// Another handle over this store's incarnation and windows.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn clone_preserving_history(&self) -> Self {
        Self {
            work_limits: self.work_limits,
            replay_incarnation_id: self.replay_incarnation_id.clone(),
            config: self.config.clone(),
            clock: Arc::clone(&self.clock),
            windows: Arc::clone(&self.windows),
        }
    }

    fn cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
        live_position: u64,
    ) -> ProcessObservationCursor {
        ProcessObservationCursor::new(
            &self.replay_incarnation_id,
            process_id,
            sequence,
            live_position,
        )
    }

    /// The store's state with its bounded upkeep done: `process_id`'s
    /// window trimmed and touched, and a batch of idle windows expired.
    fn upkeep(&self, process_id: &ProcessId) -> (std::sync::MutexGuard<'_, Windows>, Instant) {
        let now = self.clock.now();
        let mut windows = self.windows.lock_recover();
        windows.update(process_id, |window| window.trim(&self.config, now));
        windows.touch(process_id, now);
        windows.expire(
            &self.config,
            now,
            self.work_limits.replay_expiry_batch.get(),
        );
        (windows, now)
    }

    /// The gap that prevents continuing from `cursor`, if any.
    fn gap(
        &self,
        windows: &Windows,
        cursor: &ParsedProcessObservationCursor<'_>,
    ) -> Option<ProcessReplayGapReason> {
        if cursor.replay_incarnation_id != self.replay_incarnation_id {
            return Some(ProcessReplayGapReason::Unavailable);
        }
        match windows.windows.get(&cursor.process_id) {
            Some(window) => window.gap(cursor.live_position),
            None => Some(ProcessReplayGapReason::Unavailable),
        }
    }

    /// A cursor in the process's window, which is created when absent so
    /// the cursor names continuity a subscription can hold.
    fn cursor_at(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
        position: impl FnOnce(&Window) -> u64,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
        let (mut windows, now) = self.upkeep(process_id);
        windows.ensure(&self.config, process_id, now)?;
        let live_position = windows
            .windows
            .get(process_id)
            .map(position)
            .ok_or_else(missing_window)?;
        Ok(self.cursor(process_id, sequence, live_position))
    }
}

#[async_trait::async_trait]
impl ProcessReplayStore for InMemoryProcessReplayStore {
    async fn publish(
        &self,
        process_id: &ProcessId,
        drafts: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<Arc<ProcessObservationEvent>>, ProcessReplayStoreError> {
        if drafts.is_empty() {
            return Err(ProcessReplayStoreError::Store(
                "cannot publish an empty process replay batch".to_string(),
            ));
        }
        let (mut windows, now) = self.upkeep(process_id);
        windows.ensure(&self.config, process_id, now)?;
        let fresh = match windows
            .windows
            .get(process_id)
            .ok_or_else(missing_window)?
            .fresh(drafts)
        {
            Ok(fresh) => fresh,
            Err(identity) => {
                windows.remove(process_id);
                return Err(ProcessReplayStoreError::ConflictingRedelivery {
                    process_id: process_id.clone(),
                    identity,
                });
            }
        };
        if fresh.is_empty() {
            return Ok(Vec::new());
        }
        let start = windows
            .windows
            .get(process_id)
            .and_then(|window| window.tail_position.checked_add(1))
            .filter(|start| start.checked_add(fresh.len() as u64).is_some())
            .ok_or_else(|| {
                ProcessReplayStoreError::Store("process replay position overflow".into())
            })?;
        // A batch the store cannot hold takes no position and retires the
        // process's continuity: no cursor may replay cleanly across it.
        let staged = fresh
            .into_iter()
            .enumerate()
            .map(|(offset, draft)| {
                let position = start + offset as u64;
                let sequence = draft.sequence();
                let identity = draft.payload().identity();
                let event = Arc::new(ProcessObservationEvent::new(
                    self.cursor(process_id, sequence, position),
                    draft.into_payload(),
                )?);
                let bytes = event_bytes(&event, self.config.max_bytes_per_process)?;
                Ok(Stored {
                    position,
                    sequence,
                    identity,
                    bytes,
                    appended_at: now,
                    event,
                })
            })
            .collect::<Result<Vec<_>, ProcessReplayStoreError>>()
            .and_then(|staged| {
                let total = staged
                    .iter()
                    .try_fold(0_usize, |total, stored| total.checked_add(stored.bytes))
                    .ok_or_else(capacity_error)?;
                windows.make_room(&self.config, process_id, total)?;
                Ok(staged)
            });
        let staged = match staged {
            Ok(staged) => staged,
            Err(error) => {
                windows.remove(process_id);
                return Err(error);
            }
        };
        let events = staged
            .iter()
            .map(|stored| Arc::clone(&stored.event))
            .collect::<Vec<_>>();
        windows.update(process_id, |window| {
            for stored in staged {
                window.append(stored);
            }
            window.trim(&self.config, now);
            for event in &events {
                window.notify(event);
            }
        });
        Ok(events)
    }

    async fn replay_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplayOutcome, ProcessReplayStoreError> {
        let parsed = cursor.parse()?;
        let (windows, _) = self.upkeep(&parsed.process_id);
        if let Some(reason) = self.gap(&windows, &parsed) {
            return Ok(ProcessReplayOutcome::Gap(reason));
        }
        Ok(ProcessReplayOutcome::Replayed(
            windows
                .windows
                .get(&parsed.process_id)
                .map(|window| window.after(parsed.live_position))
                .unwrap_or_default(),
        ))
    }

    async fn subscribe_after_cursor(
        &self,
        cursor: &ProcessObservationCursor,
    ) -> Result<ProcessReplaySubscribeOutcome, ProcessReplayStoreError> {
        let parsed = cursor.parse()?;
        let (mut windows, _) = self.upkeep(&parsed.process_id);
        if let Some(reason) = self.gap(&windows, &parsed) {
            return Ok(ProcessReplaySubscribeOutcome::Gap(reason));
        }
        let capacity = self.config.max_events_per_process.max(1);
        Ok(windows
            .update(&parsed.process_id, |window| {
                let receiver = match window.sender.as_ref() {
                    Some(sender) => sender.subscribe(),
                    None => {
                        let (sender, receiver) = broadcast::channel(capacity);
                        window.sender = Some(sender);
                        receiver
                    }
                };
                ProcessReplaySubscribeOutcome::Subscribed(ProcessReplaySubscription::new(
                    window.after(parsed.live_position),
                    BroadcastTail::new(receiver, parsed.live_position),
                ))
            })
            .unwrap_or(ProcessReplaySubscribeOutcome::Gap(
                ProcessReplayGapReason::Unavailable,
            )))
    }

    async fn current_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
        self.cursor_at(process_id, sequence, |window| {
            window
                .events
                .iter()
                .find(|stored| stored.sequence > sequence)
                .map_or(window.tail_position, |stored| stored.position - 1)
        })
    }

    async fn earliest_cursor(
        &self,
        process_id: &ProcessId,
        sequence: ProcessSequence,
    ) -> Result<ProcessObservationCursor, ProcessReplayStoreError> {
        self.cursor_at(process_id, sequence, |window| {
            window
                .events
                .front()
                .map_or(window.tail_position, |stored| stored.position - 1)
        })
    }

    async fn invalidate_process(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), ProcessReplayStoreError> {
        let (mut windows, _) = self.upkeep(process_id);
        windows.remove(process_id);
        Ok(())
    }

    async fn invalidate_all(&self) -> Result<(), ProcessReplayStoreError> {
        let mut windows = self.windows.lock_recover();
        windows.windows.clear();
        windows.idle.clear();
        windows.retained_bytes = 0;
        Ok(())
    }

    async fn trim_process(&self, process_id: &ProcessId) -> Result<(), ProcessReplayStoreError> {
        drop(self.upkeep(process_id));
        Ok(())
    }
}

/// Every process's window, with the idle order and the aggregate charge.
///
/// A removed window takes its positions with it; `high_watermark` fences
/// them, so a recreated window never reuses a position without the store
/// keeping a tombstone per evicted process.
#[derive(Debug, Default)]
struct Windows {
    windows: HashMap<ProcessId, Window>,
    idle: BTreeSet<(Instant, ProcessId)>,
    retained_bytes: usize,
    high_watermark: u64,
}

impl Windows {
    fn touch(&mut self, process_id: &ProcessId, now: Instant) {
        if let Some(window) = self.windows.get_mut(process_id) {
            self.idle.remove(&(window.last_access, process_id.clone()));
            window.last_access = now;
            self.idle.insert((now, process_id.clone()));
        }
    }

    /// Remove a window: its subscribers' channel closes with its sender.
    fn remove(&mut self, process_id: &ProcessId) {
        if let Some(window) = self.windows.remove(process_id) {
            self.idle.remove(&(window.last_access, process_id.clone()));
            self.retained_bytes -= window.retained_bytes;
        }
    }

    fn expire(
        &mut self,
        config: &InMemoryProcessReplayStoreConfig,
        now: Instant,
        budget: usize,
    ) -> usize {
        let mut removed = 0;
        while removed < budget {
            let Some((last_access, process_id)) = self.idle.first() else {
                break;
            };
            if now.saturating_duration_since(*last_access) <= config.max_age {
                break;
            }
            let process_id = process_id.clone();
            self.remove(&process_id);
            removed += 1;
        }
        removed
    }

    fn ensure(
        &mut self,
        config: &InMemoryProcessReplayStoreConfig,
        process_id: &ProcessId,
        now: Instant,
    ) -> Result<(), ProcessReplayStoreError> {
        if self.windows.contains_key(process_id) {
            return Ok(());
        }
        let metadata_bytes = WINDOW_METADATA_BYTES;
        if config.max_processes == 0 || metadata_bytes > config.max_retained_bytes {
            return Err(capacity_error());
        }
        while self.windows.len() >= config.max_processes
            || self.retained_bytes > config.max_retained_bytes - metadata_bytes
        {
            self.evict_oldest(None)?;
        }
        let first_position = self.high_watermark.checked_add(1).ok_or_else(|| {
            ProcessReplayStoreError::Store("process replay position overflow".into())
        })?;
        self.high_watermark = first_position;
        self.retained_bytes += metadata_bytes;
        self.windows.insert(
            process_id.clone(),
            Window {
                first_position,
                tail_position: first_position,
                last_access: now,
                retained_bytes: metadata_bytes,
                event_bytes: 0,
                events: VecDeque::new(),
                identities: HashMap::new(),
                sender: None,
            },
        );
        self.idle.insert((now, process_id.clone()));
        Ok(())
    }

    /// Make room for `additional` event bytes in `process_id`'s window:
    /// within its own byte bound by dropping its oldest events, and within
    /// the aggregate bound by evicting the idlest other windows first.
    fn make_room(
        &mut self,
        config: &InMemoryProcessReplayStoreConfig,
        process_id: &ProcessId,
        additional: usize,
    ) -> Result<(), ProcessReplayStoreError> {
        if additional > config.max_bytes_per_process || additional > config.max_retained_bytes {
            return Err(capacity_error());
        }
        self.update(process_id, |window| {
            while window.event_bytes > config.max_bytes_per_process - additional {
                window.drop_front();
            }
        });
        while self.retained_bytes > config.max_retained_bytes - additional {
            if self.evict_oldest(Some(process_id)).is_ok() {
                continue;
            }
            let dropped = self
                .update(process_id, |window| {
                    let had = !window.events.is_empty();
                    window.drop_front();
                    had
                })
                .unwrap_or(false);
            if !dropped {
                return Err(capacity_error());
            }
        }
        Ok(())
    }

    fn evict_oldest(
        &mut self,
        protected: Option<&ProcessId>,
    ) -> Result<(), ProcessReplayStoreError> {
        let victim = self
            .idle
            .iter()
            .find(|(_, id)| Some(id) != protected)
            .map(|(_, id)| id.clone())
            .ok_or_else(capacity_error)?;
        self.remove(&victim);
        Ok(())
    }

    /// Mutate one window, keeping the aggregate charge and the position
    /// fence in step with it.
    fn update<R>(
        &mut self,
        process_id: &ProcessId,
        mutate: impl FnOnce(&mut Window) -> R,
    ) -> Option<R> {
        let window = self.windows.get_mut(process_id)?;
        let before = window.retained_bytes;
        let result = mutate(window);
        self.high_watermark = self.high_watermark.max(window.tail_position);
        self.retained_bytes = self.retained_bytes - before + window.retained_bytes;
        Some(result)
    }
}

/// Charged for a window before it holds any event.
const WINDOW_METADATA_BYTES: usize = std::mem::size_of::<Window>() + 256;

#[derive(Debug)]
struct Window {
    /// The position before the window's first: a cursor behind it belongs
    /// to a window that was removed.
    first_position: u64,
    /// The last position assigned.
    tail_position: u64,
    last_access: Instant,
    /// Metadata and retained events, as charged against the aggregate.
    retained_bytes: usize,
    /// Retained events alone, as charged against the per-process bound.
    event_bytes: usize,
    /// Contiguous in position.
    events: VecDeque<Stored>,
    /// The position each retained identity holds in `events`.
    identities: HashMap<ProcessObservationIdentity, u64>,
    sender: Option<broadcast::Sender<Notification>>,
}

#[derive(Debug)]
struct Stored {
    position: u64,
    sequence: ProcessSequence,
    identity: ProcessObservationIdentity,
    bytes: usize,
    appended_at: Instant,
    event: Arc<ProcessObservationEvent>,
}

impl Window {
    fn drop_front(&mut self) {
        if let Some(stored) = self.events.pop_front() {
            self.event_bytes -= stored.bytes;
            self.retained_bytes -= stored.bytes;
            if self.identities.get(&stored.identity) == Some(&stored.position) {
                self.identities.remove(&stored.identity);
            }
        }
    }

    fn append(&mut self, stored: Stored) {
        self.event_bytes += stored.bytes;
        self.retained_bytes += stored.bytes;
        self.tail_position = stored.position;
        self.identities
            .insert(stored.identity.clone(), stored.position);
        self.events.push_back(stored);
    }

    fn trim(&mut self, config: &InMemoryProcessReplayStoreConfig, now: Instant) {
        while self.events.len() > config.max_events_per_process
            || self.event_bytes > config.max_bytes_per_process
            || self
                .events
                .front()
                .is_some_and(|stored| now.duration_since(stored.appended_at) > config.max_age)
        {
            self.drop_front();
        }
    }

    fn gap(&self, cursor_position: u64) -> Option<ProcessReplayGapReason> {
        if cursor_position < self.first_position || cursor_position > self.tail_position {
            return Some(ProcessReplayGapReason::Unavailable);
        }
        let retained_from = self
            .events
            .front()
            .map_or(self.tail_position, |first| first.position - 1);
        (cursor_position < retained_from).then_some(ProcessReplayGapReason::Trimmed)
    }

    fn after(&self, position: u64) -> Vec<Arc<ProcessObservationEvent>> {
        self.events
            .iter()
            .filter(|stored| stored.position > position)
            .map(|stored| Arc::clone(&stored.event))
            .collect()
    }

    fn retained(&self, identity: &ProcessObservationIdentity) -> Option<&Stored> {
        let position = *self.identities.get(identity)?;
        let first = self.events.front()?.position;
        self.events.get(usize::try_from(position - first).ok()?)
    }

    /// The drafts that are not redeliveries of what the window, or an
    /// earlier draft of the batch, already holds; or the identity a draft
    /// repeats with a different fact.
    fn fresh(
        &self,
        drafts: Vec<ProcessReplayEventDraft>,
    ) -> Result<Vec<ProcessReplayEventDraft>, ProcessObservationIdentity> {
        let mut fresh: Vec<ProcessReplayEventDraft> = Vec::with_capacity(drafts.len());
        let mut batch: HashMap<ProcessObservationIdentity, usize> = HashMap::new();
        for draft in drafts {
            let identity = draft.payload().identity();
            let held = self
                .retained(&identity)
                .map(|stored| &stored.event.payload)
                .or_else(|| batch.get(&identity).map(|index| fresh[*index].payload()));
            match held {
                Some(held) if held.same_fact(draft.payload()) => {}
                Some(_) => return Err(identity),
                None => {
                    batch.insert(identity, fresh.len());
                    fresh.push(draft);
                }
            }
        }
        Ok(fresh)
    }

    fn notify(&mut self, event: &Arc<ProcessObservationEvent>) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        if sender
            .send(Notification {
                position: event.live_position(),
                event: Arc::downgrade(event),
            })
            .is_err()
        {
            self.sender = None;
        }
    }
}

/// A published event, by weak reference: a slow subscriber's channel never
/// holds payloads the window has already dropped.
#[derive(Clone, Debug)]
struct Notification {
    position: u64,
    event: Weak<ProcessObservationEvent>,
}

type RecvResult = (
    Result<Notification, broadcast::error::RecvError>,
    broadcast::Receiver<Notification>,
);

async fn recv(mut receiver: broadcast::Receiver<Notification>) -> RecvResult {
    let result = receiver.recv().await;
    (result, receiver)
}

/// The store's live tail: its broadcast channel's notifications past the
/// subscribed position.
struct BroadcastTail {
    receiver: ReusableBoxFuture<'static, RecvResult>,
    after_position: u64,
    closed: bool,
}

impl BroadcastTail {
    fn new(receiver: broadcast::Receiver<Notification>, after_position: u64) -> Self {
        Self {
            receiver: ReusableBoxFuture::new(recv(receiver)),
            after_position,
            closed: false,
        }
    }
}

impl Stream for BroadcastTail {
    type Item = ProcessReplayItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.closed {
                return Poll::Ready(None);
            }
            let (result, receiver) = ready!(self.receiver.poll(cx));
            self.receiver.set(recv(receiver));
            match result {
                Ok(notification) if notification.position <= self.after_position => {}
                Ok(notification) => {
                    self.after_position = notification.position;
                    return Poll::Ready(Some(
                        notification
                            .event
                            .upgrade()
                            .ok_or(ProcessReplayStoreError::SubscriberLagged(1)),
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return Poll::Ready(Some(Err(ProcessReplayStoreError::SubscriberLagged(
                        count,
                    ))));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    self.closed = true;
                    return Poll::Ready(Some(Err(ProcessReplayStoreError::Closed)));
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) fn memory_window_bytes() -> usize {
    WINDOW_METADATA_BYTES
}

fn capacity_error() -> ProcessReplayStoreError {
    ProcessReplayStoreError::Store(
        "process replay publication exceeds the store retention capacity".into(),
    )
}

fn missing_window() -> ProcessReplayStoreError {
    ProcessReplayStoreError::Store("process replay window is missing".into())
}

/// The charge for one retained event: its serialized payload, counted
/// without allocating an encoded copy, plus its inline descriptors.
fn event_bytes(
    event: &ProcessObservationEvent,
    limit: usize,
) -> Result<usize, ProcessReplayStoreError> {
    let mut counter = ByteCounter {
        bytes: std::mem::size_of::<Stored>()
            + std::mem::size_of::<ProcessObservationEvent>()
            + event.cursor.as_str().len()
            + 128,
        limit,
    };
    let counted = match &event.payload {
        super::ProcessObservationEventPayload::LanguageExecution(observation) => {
            serde_json::to_writer(&mut counter, observation)
        }
        super::ProcessObservationEventPayload::Committed { event } => {
            serde_json::to_writer(&mut counter, event)
        }
    };
    counted.map_err(|_| capacity_error())?;
    if counter.bytes > limit {
        return Err(capacity_error());
    }
    Ok(counter.bytes)
}

struct ByteCounter {
    bytes: usize,
    limit: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > self.limit {
            return Err(io::Error::other("store retention capacity exceeded"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
