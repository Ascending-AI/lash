//! One process cursor across durable history and the live hub (FIG-3571 §E).
//!
//! A [`ProcessCursor`] pages a process's durable event history through
//! [`crate::Processes::events`] and resumes its live observation through
//! [`crate::Processes::subscribe_observation`]. The hub carries two kinds of
//! evidence per process, in one ordered ring:
//!
//! - live execution observations (trace records), best-effort, and
//! - `Committed { sequence }` items, published through the ADR 0017 sink after
//!   each durable commit. Publication never fails the commit.
//!
//! A subscription resumes silently only when the ring still holds every item
//! after the cursor's live position and its retained `Committed` evidence
//! bridges the cursor's durable sequence to the durable high-water mark. Any
//! other case — an epoch change, a trimmed ring, a cursor for another
//! process, an unknown or pruned process, or a sequence the ring cannot
//! bridge — answers
//! `Gap` with a snapshot and a new cursor, even when the process is idle.
//!
//! The snapshot names one process — a minted id is never reused, so the id is
//! the whole lifetime (ADR 0107) — and one durable high-water
//! sequence. Its durable half is the status plus the effect-summary fold
//! through that boundary, read with Full payloads under a bounded acquisition
//! budget; its live half is the publisher's graph at the cursor's live
//! position. The two report completeness separately.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::{
    PluginError, ProcessEffectReport, ProcessEvent, ProcessEventHistoryRetention,
    ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessRegistry, ProcessStatus,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{PROCESS_CURSOR_UNROUTED_EPOCH, ProcessId};
use lash_trace::{
    TraceEvent, TraceLanguageExecutionPayload, TraceLashlangGraph, TraceLashlangGraphAccumulator,
    TraceLashlangGraphCompleteness, TraceRecord, TraceRuntimeSubject, TraceSink, TraceSinkError,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use lash_sansio::{ProcessCursor, ProcessCursorError, ProcessCursorReference};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessObservationGapReason {
    Overflow,
    Expired,
    SubscriberLagged,
    PublisherReplaced,
    RoutingUnavailable,
    CrossProcess,
    InvalidCursor,
    /// Retained `Committed` evidence cannot bridge the cursor's durable
    /// sequence to the durable high-water mark.
    SequenceUnbridged,
    /// The requested process is unknown or no longer retained.
    HistoryUnavailable,
}

/// Why the live graph cannot describe the complete process execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLiveIncompleteness {
    RoutingUnavailable,
    PublisherReplaced,
    PublisherJoinedMidRun,
    IncompleteGraph,
    ProjectionTruncated,
}

#[derive(Clone, Copy, Debug)]
enum RingTrim {
    Overflow,
    Expired,
}

impl From<RingTrim> for ProcessObservationGapReason {
    fn from(value: RingTrim) -> Self {
        match value {
            RingTrim::Overflow => Self::Overflow,
            RingTrim::Expired => Self::Expired,
        }
    }
}

/// Live-graph completeness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessObservationCompleteness {
    Complete,
    Incomplete { reason: ProcessLiveIncompleteness },
}

/// The live half of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessObservationProjection {
    /// Missing when this core routes no live publisher for the process.
    pub graph: Option<TraceLashlangGraph>,
    pub completeness: ProcessObservationCompleteness,
}

/// Why a durable summary fold stops short of its high-water mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessDurableGapReason {
    AcquisitionBudgetExhausted,
    SummaryUndecodable,
    /// The host released a prefix of the history (FIG-3482): the summary
    /// folds only the events after it.
    HistoryReleased,
}

/// Durable-summary completeness, reported apart from the live graph's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessDurableCompleteness {
    Complete,
    Incomplete { reason: ProcessDurableGapReason },
}

/// The durable half of a snapshot of one process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessDurableSnapshot {
    /// Status and the effect-summary fold through `sequence`, the durable
    /// high-water mark read with the status.
    Retained {
        sequence: u64,
        status: ProcessStatus,
        summary: ProcessEffectReport,
        completeness: ProcessDurableCompleteness,
    },
    /// The process's history is typed-absent: it was pruned.
    NoLongerRetained {
        terminal_label: lash_core::RetiredProcessStatus,
        pruned_at_ms: u64,
    },
    /// No retained process or tombstone has this id.
    Unknown,
}

impl ProcessDurableSnapshot {
    fn high_water(&self) -> u64 {
        match self {
            Self::Retained { sequence, .. } => *sequence,
            Self::NoLongerRetained { .. } | Self::Unknown => 0,
        }
    }

    fn gap_reason(&self) -> Option<ProcessObservationGapReason> {
        match self {
            Self::Retained { .. } => None,
            Self::NoLongerRetained { .. } | Self::Unknown => {
                Some(ProcessObservationGapReason::HistoryUnavailable)
            }
        }
    }
}

/// A snapshot of one process at one durable high-water sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessObservationSnapshot {
    pub durable: ProcessDurableSnapshot,
    pub live: ProcessObservationProjection,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcessObservationItem {
    Snapshot {
        cursor: ProcessCursor,
        snapshot: ProcessObservationSnapshot,
    },
    Event {
        cursor: ProcessCursor,
        record: Box<TraceRecord>,
    },
    /// A durable event committed; read it through [`crate::Processes::events`].
    Committed {
        cursor: ProcessCursor,
        sequence: u64,
        kind: lash_core::ProcessEventKind,
    },
    Gap {
        requested_cursor: Option<ProcessCursor>,
        cursor: ProcessCursor,
        reason: ProcessObservationGapReason,
        snapshot: ProcessObservationSnapshot,
    },
}

impl ProcessObservationItem {
    pub fn cursor(&self) -> &ProcessCursor {
        match self {
            Self::Snapshot { cursor, .. }
            | Self::Event { cursor, .. }
            | Self::Committed { cursor, .. }
            | Self::Gap { cursor, .. } => cursor,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessObservationConfig {
    pub capacity: usize,
    pub ttl: Duration,
    /// Durable pages one snapshot acquisition may read.
    pub snapshot_page_budget: usize,
    /// Events per durable page a snapshot acquisition reads.
    pub snapshot_page_size: std::num::NonZeroUsize,
}

impl Default for ProcessObservationConfig {
    fn default() -> Self {
        Self {
            capacity: 2048,
            ttl: Duration::from_secs(120),
            snapshot_page_budget: 64,
            snapshot_page_size: std::num::NonZeroUsize::new(256)
                .unwrap_or(std::num::NonZeroUsize::MIN),
        }
    }
}

#[derive(Clone)]
enum PublishedItem {
    Live(Box<TraceRecord>),
    Committed {
        sequence: u64,
        kind: lash_core::ProcessEventKind,
    },
}

#[derive(Clone)]
struct Published {
    position: u64,
    at: Instant,
    item: PublishedItem,
}

#[derive(Clone)]
enum PublishedNotification {
    Item(u64, PublishedItem),
    Replaced,
}

/// The live records one accumulator update takes at most. Snapshot projection
/// happens at capture; folding a batch updates only its occurrence indexes.
const LIVE_FOLD_BATCH: usize = 256;

struct ProcessState {
    epoch: String,
    position: u64,
    base_position: u64,
    /// The key of the graph this route publishes, set by its first record.
    graph_key: Option<String>,
    /// The live graph through every published record but `unfolded`.
    current_graph: TraceLashlangGraphAccumulator,
    /// Published records the graph has not folded yet, at most
    /// `LIVE_FOLD_BATCH`. Folding partitions equals folding their
    /// concatenation, so the graph a capture reads is the one a fold per
    /// record would have built.
    unfolded: Vec<TraceRecord>,
    #[cfg(test)]
    folds: usize,
    joined_at_start: bool,
    terminal: bool,
    last_published: Instant,
    ring: VecDeque<Published>,
    trim_reason: RingTrim,
    sender: broadcast::Sender<PublishedNotification>,
}

impl ProcessState {
    fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            epoch: uuid::Uuid::new_v4().simple().to_string(),
            position: 0,
            base_position: 0,
            graph_key: None,
            current_graph: TraceLashlangGraphAccumulator::default(),
            unfolded: Vec::new(),
            #[cfg(test)]
            folds: 0,
            joined_at_start: false,
            terminal: false,
            last_published: Instant::now(),
            ring: VecDeque::new(),
            trim_reason: RingTrim::Overflow,
            sender,
        }
    }

    fn trim(&mut self, now: Instant, config: ProcessObservationConfig) {
        while self.ring.len() > config.capacity.max(1)
            || self
                .ring
                .front()
                .is_some_and(|item| now.duration_since(item.at) > config.ttl)
        {
            let expired = self
                .ring
                .front()
                .is_some_and(|item| now.duration_since(item.at) > config.ttl);
            if let Some(old) = self.ring.pop_front() {
                self.base_position = old.position;
                self.trim_reason = if expired {
                    RingTrim::Expired
                } else {
                    RingTrim::Overflow
                };
            }
        }
    }

    /// Take one live record into the graph's pending batch.
    fn observe(&mut self, record: &TraceRecord) {
        self.unfolded.push(record.clone());
        if self.unfolded.len() >= LIVE_FOLD_BATCH {
            self.fold_unfolded();
        }
    }

    /// Fold the pending batch into the live graph.
    fn fold_unfolded(&mut self) {
        if self.unfolded.is_empty() {
            return;
        }
        let records = std::mem::take(&mut self.unfolded);
        #[cfg(test)]
        {
            self.folds += 1;
        }
        let _ = self.current_graph.fold(&records);
    }

    fn live_projection(&mut self) -> ProcessObservationProjection {
        self.fold_unfolded();
        let Some(graph) = self.current_graph.snapshot() else {
            return ProcessObservationProjection {
                graph: None,
                completeness: ProcessObservationCompleteness::Incomplete {
                    reason: ProcessLiveIncompleteness::RoutingUnavailable,
                },
            };
        };
        let reason = if !self.joined_at_start {
            Some(ProcessLiveIncompleteness::PublisherJoinedMidRun)
        } else if graph.completeness != TraceLashlangGraphCompleteness::Complete {
            Some(ProcessLiveIncompleteness::IncompleteGraph)
        } else if !graph.node_retention.is_empty() {
            Some(ProcessLiveIncompleteness::ProjectionTruncated)
        } else {
            None
        };
        ProcessObservationProjection {
            graph: Some(graph),
            completeness: match reason {
                Some(reason) => ProcessObservationCompleteness::Incomplete { reason },
                None => ProcessObservationCompleteness::Complete,
            },
        }
    }

    fn publish(&mut self, item: PublishedItem, config: ProcessObservationConfig) {
        let now = Instant::now();
        self.position += 1;
        self.last_published = now;
        let position = self.position;
        self.ring.push_back(Published {
            position,
            at: now,
            item: item.clone(),
        });
        self.trim(now, config);
        let _ = self
            .sender
            .send(PublishedNotification::Item(position, item));
    }

    fn replace(&mut self, capacity: usize) {
        let _ = self.sender.send(PublishedNotification::Replaced);
        *self = Self::new(capacity);
    }
}

/// What one lock hold captured of a process's live route.
struct Capture {
    state: Arc<Mutex<ProcessState>>,
    /// The hub had no route for this process before this capture.
    created: bool,
    epoch: String,
    position: u64,
    base_position: u64,
    trim_reason: RingTrim,
    live: ProcessObservationProjection,
    tail: VecDeque<(u64, PublishedItem)>,
    receiver: broadcast::Receiver<PublishedNotification>,
}

/// One core's live observation routes. A new core build creates new epochs,
/// and so does a publisher restart.
///
/// Lock order: the `states` map lock is taken before a per-process state lock
/// and only for lookup, insertion and release; graph folds run under the
/// per-process lock alone, one per `LIVE_FOLD_BATCH` records or per capture.
/// A process's state is released once no subscription holds it and it has
/// either published `ExecutionFinished` or published nothing for `ttl`.
pub struct ProcessObservationHub {
    config: ProcessObservationConfig,
    states: Mutex<HashMap<ProcessId, Arc<Mutex<ProcessState>>>>,
    last_sweep: Mutex<Instant>,
    #[cfg(test)]
    snapshot_capture_pause: Mutex<Option<Arc<tests::SnapshotCapturePause>>>,
}

impl Default for ProcessObservationHub {
    fn default() -> Self {
        Self::new(ProcessObservationConfig::default())
    }
}

impl ProcessObservationHub {
    pub fn new(config: ProcessObservationConfig) -> Self {
        Self {
            config,
            states: Mutex::new(HashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
            #[cfg(test)]
            snapshot_capture_pause: Mutex::new(None),
        }
    }

    /// The state for `process_id`, created when absent.
    fn state_for(&self, process_id: &ProcessId) -> (Arc<Mutex<ProcessState>>, bool) {
        let mut states = self.states.lock_recover();
        match states.get(process_id) {
            Some(state) => (Arc::clone(state), false),
            None => {
                let state = Arc::new(Mutex::new(ProcessState::new(self.config.capacity)));
                states.insert(process_id.clone(), Arc::clone(&state));
                (state, true)
            }
        }
    }

    /// Capture the route for one process: the live projection at the current
    /// position, the retained items after `after_position` (when given), and a
    /// receiver for everything published after this capture.
    fn capture(&self, process_id: &ProcessId, after_position: Option<u64>) -> Capture {
        let (state, created) = self.state_for(process_id);
        let mut publisher = state.lock_recover();
        publisher.trim(Instant::now(), self.config);
        let tail = match after_position {
            Some(after) => publisher
                .ring
                .iter()
                .filter(|item| item.position > after)
                .map(|item| (item.position, item.item.clone()))
                .collect(),
            None => VecDeque::new(),
        };
        let capture = Capture {
            state: Arc::clone(&state),
            created,
            epoch: publisher.epoch.clone(),
            position: publisher.position,
            base_position: publisher.base_position,
            trim_reason: publisher.trim_reason,
            live: publisher.live_projection(),
            tail,
            receiver: publisher.sender.subscribe(),
        };
        drop(publisher);
        capture
    }

    /// The live route a cursor minted now names: `(epoch, position)`, taken
    /// before the durable read it will be paired with so no commit can fall
    /// between the two unseen.
    pub(crate) fn route(&self, process_id: &ProcessId) -> (String, u64) {
        let capture = self.capture(process_id, None);
        (capture.epoch, capture.position)
    }

    /// Subscribe to one process.
    ///
    /// Without a cursor the first item is a `Snapshot`. With a cursor the
    /// subscription resumes after it when the ring bridges it; otherwise the
    /// first item is a `Gap` with a snapshot and a new cursor.
    pub async fn subscribe(
        self: &Arc<Self>,
        registry: Arc<dyn ProcessRegistry>,
        process_id: &ProcessId,
        from: Option<&ProcessCursor>,
    ) -> Result<ProcessObservationSubscription, PluginError> {
        let reference = ProcessCursorReference::for_process(process_id);
        let mut subscription = ProcessObservationSubscription {
            hub: Arc::clone(self),
            registry,
            process_id: process_id.clone(),
            reference,
            state: None,
            epoch: PROCESS_CURSOR_UNROUTED_EPOCH.to_string(),
            position: 0,
            sequence: 0,
            pending: VecDeque::new(),
            replay: VecDeque::new(),
            receiver: None,
        };
        let Some(from) = from else {
            subscription.resync(None, None).await?;
            return Ok(subscription);
        };
        if !from.reference().names(process_id) {
            subscription
                .resync(
                    Some(ProcessObservationGapReason::CrossProcess),
                    Some(from.clone()),
                )
                .await?;
            return Ok(subscription);
        }
        // The high-water mark is read before the capture, so every commit it
        // names that was published is already in the captured tail.
        let durable = subscription
            .registry
            .get_process(process_id)
            .await
            .map(|record| record.map(|record| record.last_event_sequence));
        let high_water = match durable {
            Ok(Some(high_water)) => high_water,
            Ok(None) | Err(PluginError::ProcessNoLongerRetained { .. }) => {
                subscription.resync(None, Some(from.clone())).await?;
                return Ok(subscription);
            }
            Err(error) => return Err(error),
        };
        let capture = self.capture(process_id, Some(from.position()));
        let reason = if from.epoch() != capture.epoch {
            Some(if capture.created {
                ProcessObservationGapReason::RoutingUnavailable
            } else {
                ProcessObservationGapReason::PublisherReplaced
            })
        } else if from.position() > capture.position || from.sequence() > high_water {
            Some(ProcessObservationGapReason::InvalidCursor)
        } else if from.position() < capture.base_position {
            Some(capture.trim_reason.into())
        } else if !bridges(&capture.tail, from.sequence(), high_water) {
            Some(ProcessObservationGapReason::SequenceUnbridged)
        } else {
            None
        };
        if let Some(reason) = reason {
            drop(capture);
            subscription
                .resync(Some(reason), Some(from.clone()))
                .await?;
            return Ok(subscription);
        }
        subscription.epoch = capture.epoch;
        subscription.position = from.position();
        subscription.sequence = from.sequence();
        subscription.state = Some(capture.state);
        subscription.replay = capture.tail;
        subscription.receiver = Some(capture.receiver);
        Ok(subscription)
    }

    /// Release every state no subscription holds whose publisher finished or
    /// went idle for `ttl`. Runs at most once per `ttl` from a publish.
    fn sweep_idle(&self, now: Instant) {
        {
            let mut last_sweep = self.last_sweep.lock_recover();
            if now.duration_since(*last_sweep) < self.config.ttl {
                return;
            }
            *last_sweep = now;
        }
        // Clones of a state are only taken under the map lock, so a count of
        // one here means no subscription or publisher holds this state.
        self.states.lock_recover().retain(|_, state| {
            Arc::strong_count(state) > 1 || {
                let state = state.lock_recover();
                !state.terminal && now.duration_since(state.last_published) <= self.config.ttl
            }
        });
    }

    /// Lock order: map before a state only for this short terminal check.
    fn evict_if_terminal(&self, process_id: &ProcessId, state: &Arc<Mutex<ProcessState>>) {
        let mut states = self.states.lock_recover();
        if states
            .get(process_id)
            .is_some_and(|current| Arc::ptr_eq(current, state))
            && Arc::strong_count(state) == 2
            && state.lock_recover().terminal
        {
            states.remove(process_id);
        }
    }

    /// Publish one durable commit. Called after the commit, from the ADR 0017
    /// sink; it cannot fail the write.
    pub(crate) fn publish_committed(&self, event: &ProcessEvent) {
        let (state, _) = self.state_for(&event.process_id);
        let mut publisher = state.lock_recover();
        publisher.publish(
            PublishedItem::Committed {
                sequence: event.sequence,
                kind: event.fact.kind(),
            },
            self.config,
        );
        drop(publisher);
        drop(state);
        self.sweep_idle(Instant::now());
    }
}

/// Whether `tail` holds contiguous `Committed` evidence from just after
/// `from_sequence` through at least `high_water` (the session rule of
/// `observation.rs`, per durable event).
fn bridges(tail: &VecDeque<(u64, PublishedItem)>, from_sequence: u64, high_water: u64) -> bool {
    if from_sequence == high_water {
        return true;
    }
    let mut next = from_sequence.saturating_add(1);
    for (_, item) in tail {
        let PublishedItem::Committed { sequence, .. } = item else {
            continue;
        };
        if *sequence < next {
            continue;
        }
        if *sequence != next {
            return false;
        }
        if next >= high_water {
            return true;
        }
        next += 1;
    }
    false
}

/// Read the durable half of a snapshot of one process: the record's
/// status and high-water sequence, then the effect-summary fold through that
/// sequence with Full payloads, within the acquisition budget.
///
/// Events are append-only, so a concurrent append past the high-water mark
/// never changes the fold; a concurrent prune surfaces as the
/// typed retention outcome of the page read, never as an empty history.
async fn acquire_durable(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
    config: ProcessObservationConfig,
) -> Result<ProcessDurableSnapshot, PluginError> {
    let record = match registry.get_process(process_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(ProcessDurableSnapshot::Unknown),
        Err(PluginError::ProcessNoLongerRetained {
            terminal_label,
            pruned_at_ms,
        }) => {
            return Ok(ProcessDurableSnapshot::NoLongerRetained {
                terminal_label,
                pruned_at_ms,
            });
        }
        Err(PluginError::ProcessUnknown { .. }) => return Ok(ProcessDurableSnapshot::Unknown),
        Err(error) => return Err(error),
    };
    let high_water = record.last_event_sequence;
    let fleet_format = registry.fleet_format();
    let mut summary = ProcessEffectReport::default();
    let mut completeness = ProcessDurableCompleteness::Complete;
    let mut after = 0;
    let mut pages = 0;
    'pages: while after < high_water {
        if pages >= config.snapshot_page_budget {
            completeness = ProcessDurableCompleteness::Incomplete {
                reason: ProcessDurableGapReason::AcquisitionBudgetExhausted,
            };
            break;
        }
        pages += 1;
        let page = match registry
            .event_page_after(
                process_id,
                after,
                config.snapshot_page_size,
                ProcessEventQueryMode::Full,
            )
            .await?
        {
            ProcessEventReadOutcome::Retained(page) => page,
            ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Pruned {
                terminal_label,
                pruned_at_ms,
            }) => {
                return Ok(ProcessDurableSnapshot::NoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                });
            }
            // A released prefix is a gap the summary reports, not a reason
            // to drop the events after it: fold on from the horizon.
            ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
                released_through,
            }) => {
                completeness = ProcessDurableCompleteness::Incomplete {
                    reason: ProcessDurableGapReason::HistoryReleased,
                };
                after = released_through;
                continue;
            }
        };
        let ProcessEventPageEvents::Full(events) = page.events else {
            return Err(PluginError::Session(
                "a Full process event page returned Lite events".to_string(),
            ));
        };
        for event in events {
            if event.sequence > high_water {
                break 'pages;
            }
            if summary
                .fold_event(event.fact.event_type(), &event.fact.payload(), fleet_format)
                .is_err()
            {
                completeness = ProcessDurableCompleteness::Incomplete {
                    reason: ProcessDurableGapReason::SummaryUndecodable,
                };
            }
            after = event.sequence;
        }
        if matches!(page.more, ProcessEventPageMore::Complete) {
            break;
        }
    }
    Ok(ProcessDurableSnapshot::Retained {
        sequence: high_water,
        status: record.status(),
        summary,
        completeness,
    })
}

impl TraceSink for ProcessObservationHub {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let TraceEvent::LanguageExecution { language, event } = &record.event else {
            return Ok(());
        };
        if language != "typescript" {
            return Ok(());
        }
        let TraceRuntimeSubject::Process { process_id } = &event.identity.subject else {
            return Ok(());
        };
        let graph_key = event.identity.graph_key();
        let (state, _) = self.state_for(process_id);
        let mut publisher = state.lock_recover();
        if publisher.graph_key.as_ref().is_some_and(|current| {
            *current != graph_key
                || matches!(
                    &event.payload,
                    TraceLanguageExecutionPayload::ExecutionStarted { .. }
                )
        }) {
            publisher.replace(self.config.capacity);
        }
        if publisher.graph_key.is_none() {
            publisher.joined_at_start = matches!(
                &event.payload,
                TraceLanguageExecutionPayload::ExecutionStarted { .. }
            );
            publisher.graph_key = Some(graph_key);
        }
        publisher.terminal = matches!(
            &event.payload,
            TraceLanguageExecutionPayload::ExecutionFinished { .. }
        );
        publisher.observe(record);
        publisher.publish(PublishedItem::Live(Box::new(record.clone())), self.config);
        let terminal = publisher.terminal;
        drop(publisher);
        if terminal {
            self.evict_if_terminal(process_id, &state);
        }
        drop(state);
        self.sweep_idle(Instant::now());
        Ok(())
    }
}

pub struct ProcessObservationSubscription {
    hub: Arc<ProcessObservationHub>,
    registry: Arc<dyn ProcessRegistry>,
    process_id: ProcessId,
    reference: ProcessCursorReference,
    state: Option<Arc<Mutex<ProcessState>>>,
    epoch: String,
    position: u64,
    sequence: u64,
    pending: VecDeque<ProcessObservationItem>,
    replay: VecDeque<(u64, PublishedItem)>,
    receiver: Option<broadcast::Receiver<PublishedNotification>>,
}

impl ProcessObservationSubscription {
    fn cursor(&self) -> ProcessCursor {
        let version = self
            .registry
            .fleet_format()
            .writer_version(lash_core::surface_format!(
                lash_sansio::PROCESS_CURSOR_VERSION
            ));
        ProcessCursor::at_version(
            version,
            self.epoch.clone(),
            self.reference.clone(),
            self.position,
            self.sequence,
        )
        .unwrap_or_else(|_| {
            // Hub epochs never contain `:`; a failure here is a hub bug.
            unreachable!("a hub epoch is always a valid cursor epoch")
        })
    }

    /// Take a snapshot at a new boundary and continue from it.
    ///
    /// The live route is captured first, so everything published after the
    /// captured position reaches this subscription; the durable high-water
    /// mark is read after it, and buffered `Committed` items at or below it
    /// are dropped as already folded. The stream ends when the process is no
    /// longer retained.
    async fn resync(
        &mut self,
        reason: Option<ProcessObservationGapReason>,
        requested: Option<ProcessCursor>,
    ) -> Result<(), PluginError> {
        self.release_state();
        let current = match self.registry.get_process(&self.process_id).await {
            Ok(record) => record.is_some(),
            Err(
                PluginError::ProcessNoLongerRetained { .. } | PluginError::ProcessUnknown { .. },
            ) => false,
            Err(error) => return Err(error),
        };
        let capture = current.then(|| self.hub.capture(&self.process_id, None));
        #[cfg(test)]
        {
            let pause = self.hub.snapshot_capture_pause.lock_recover().take();
            if let Some(pause) = pause {
                pause.captured.notify_one();
                pause.release.notified().await;
            }
        }
        let durable =
            acquire_durable(self.registry.as_ref(), &self.process_id, self.hub.config).await?;
        let lifetime_gap = durable.gap_reason();
        self.sequence = durable.high_water();
        let (live, continues) = match &capture {
            Some(capture) => {
                self.epoch = capture.epoch.clone();
                self.position = capture.position;
                (capture.live.clone(), lifetime_gap.is_none())
            }
            None => {
                self.epoch = PROCESS_CURSOR_UNROUTED_EPOCH.to_string();
                self.position = 0;
                (
                    ProcessObservationProjection {
                        graph: None,
                        completeness: ProcessObservationCompleteness::Incomplete {
                            reason: if reason
                                == Some(ProcessObservationGapReason::PublisherReplaced)
                            {
                                ProcessLiveIncompleteness::PublisherReplaced
                            } else {
                                ProcessLiveIncompleteness::RoutingUnavailable
                            },
                        },
                    },
                    false,
                )
            }
        };
        let snapshot = ProcessObservationSnapshot { durable, live };
        let cursor = self.cursor();
        let item = match reason.or(lifetime_gap) {
            None => ProcessObservationItem::Snapshot { cursor, snapshot },
            Some(reason) => ProcessObservationItem::Gap {
                requested_cursor: requested,
                cursor,
                reason,
                snapshot,
            },
        };
        self.pending.push_back(item);
        self.replay.clear();
        if let Some(capture) = capture
            && continues
        {
            self.state = Some(capture.state);
            self.receiver = Some(capture.receiver);
        }
        Ok(())
    }

    fn release_state(&mut self) {
        self.receiver = None;
        if let Some(state) = self.state.take() {
            self.hub.evict_if_terminal(&self.process_id, &state);
        }
    }

    /// The next item, or `None` once the subscription ended after a gap that
    /// left nothing to follow.
    pub async fn recv(&mut self) -> Result<Option<ProcessObservationItem>, PluginError> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(item));
            }
            let (position, item) = if let Some(next) = self.replay.pop_front() {
                next
            } else {
                let Some(receiver) = self.receiver.as_mut() else {
                    return Ok(None);
                };
                match receiver.recv().await {
                    Ok(PublishedNotification::Item(position, item)) => (position, item),
                    Ok(PublishedNotification::Replaced) => {
                        let requested = self.cursor();
                        self.resync(
                            Some(ProcessObservationGapReason::PublisherReplaced),
                            Some(requested),
                        )
                        .await?;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let requested = self.cursor();
                        self.resync(
                            Some(ProcessObservationGapReason::SubscriberLagged),
                            Some(requested),
                        )
                        .await?;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let requested = self.cursor();
                        self.resync(
                            Some(ProcessObservationGapReason::PublisherReplaced),
                            Some(requested),
                        )
                        .await?;
                        continue;
                    }
                }
            };
            if position <= self.position {
                continue;
            }
            if position != self.position + 1 {
                let requested = self.cursor();
                self.resync(
                    Some(ProcessObservationGapReason::SubscriberLagged),
                    Some(requested),
                )
                .await?;
                continue;
            }
            match item {
                PublishedItem::Live(record) => {
                    self.position = position;
                    return Ok(Some(ProcessObservationItem::Event {
                        cursor: self.cursor(),
                        record,
                    }));
                }
                PublishedItem::Committed { sequence, kind } => {
                    if sequence <= self.sequence {
                        self.position = position;
                        continue;
                    }
                    if sequence != self.sequence + 1 {
                        // A publication was lost between the two: the durable
                        // log is the truth, so re-read it.
                        let requested = self.cursor();
                        self.resync(
                            Some(ProcessObservationGapReason::SequenceUnbridged),
                            Some(requested),
                        )
                        .await?;
                        continue;
                    }
                    self.position = position;
                    self.sequence = sequence;
                    return Ok(Some(ProcessObservationItem::Committed {
                        cursor: self.cursor(),
                        sequence,
                        kind,
                    }));
                }
            }
        }
    }
}

impl Drop for ProcessObservationSubscription {
    fn drop(&mut self) {
        self.release_state();
    }
}

/// Where a durable event read starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessEventsFrom {
    /// The first event of the process.
    Start(ProcessId),
    /// The events after a cursor's durable sequence, of the process it names.
    After(ProcessCursor),
}

/// One durable event page and the cursor after it.
#[derive(Clone, Debug)]
pub struct ProcessEventsRead {
    pub outcome: lash_core::facade_support::ObservedProcessEventReadOutcome,
    /// The cursor after this page: its sequence is the last event returned, or
    /// the starting one when the page returned none. `None` only when a
    /// `Start` read found no process to name.
    pub cursor: Option<ProcessCursor>,
}

/// Read one durable event page for a cursor, minting the cursor's live route
/// from `hub` before the durable read (or the unrouted epoch without one).
pub(crate) async fn read_events(
    registry: &Arc<dyn ProcessRegistry>,
    hub: Option<&ProcessObservationHub>,
    from: ProcessEventsFrom,
    limit: std::num::NonZeroUsize,
    mode: ProcessEventQueryMode,
) -> Result<ProcessEventsRead, PluginError> {
    let observer = lash_core::facade_support::ProcessWorkObserver::new(Arc::clone(registry));
    let (process_id, cursor) = match from {
        ProcessEventsFrom::Start(process_id) => {
            let process_id = match registry.require_process_id(&process_id).await {
                Ok(process_id) => process_id,
                Err(PluginError::ProcessNoLongerRetained {
                    terminal_label,
                    pruned_at_ms,
                }) => {
                    return Ok(ProcessEventsRead {
                        outcome: ProcessEventReadOutcome::NoLongerRetained(
                            ProcessEventHistoryRetention::Pruned {
                                terminal_label,
                                pruned_at_ms,
                            },
                        ),
                        cursor: None,
                    });
                }
                Err(error) => return Err(error),
            };
            let (epoch, position) = match hub {
                Some(hub) => hub.route(&process_id),
                None => (PROCESS_CURSOR_UNROUTED_EPOCH.to_string(), 0),
            };
            let reference = ProcessCursorReference::for_process(&process_id);
            let version = registry
                .fleet_format()
                .writer_version(lash_core::surface_format!(
                    lash_sansio::PROCESS_CURSOR_VERSION
                ));
            let cursor = ProcessCursor::at_version(version, epoch, reference, position, 0)
                .map_err(|error| PluginError::Session(error.to_string()))?;
            (process_id, cursor)
        }
        ProcessEventsFrom::After(cursor) => (cursor.reference().process_id().clone(), cursor),
    };
    let outcome = observer
        .event_page(&process_id, cursor.sequence(), limit, mode)
        .await?;
    let last = match &outcome {
        ProcessEventReadOutcome::Retained(page) => {
            page.last_sequence(|event| event.sequence, |event| event.sequence)
        }
        // The reader resumes after the released prefix it was told about.
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Released {
            released_through,
        }) => Some(*released_through),
        ProcessEventReadOutcome::NoLongerRetained(ProcessEventHistoryRetention::Pruned {
            ..
        }) => None,
    };
    let cursor = match last {
        Some(sequence) => cursor.with_sequence(sequence),
        None => cursor,
    };
    Ok(ProcessEventsRead {
        outcome,
        cursor: Some(cursor),
    })
}
