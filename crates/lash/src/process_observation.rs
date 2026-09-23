//! Best-effort, process-scoped replay of identified execution observations.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_sansio::ProcessId;
use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceEvent, TraceLanguageExecutionPayload, TraceLashlangGraph, TraceLashlangGraphCompleteness,
    TraceLashlangGraphStore, TraceRecord, TraceRuntimeSubject, TraceSink, TraceSinkError,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

const CURSOR_PREFIX: &str = "lashpc1:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessObservationCursor(String);

impl ProcessObservationCursor {
    /// Adopt a wire token; subscription validates its process, incarnation and epoch.
    pub fn from_token(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn new(process_id: &ProcessId, incarnation: u64, epoch: &str, position: u64) -> Self {
        Self(format!(
            "{CURSOR_PREFIX}{epoch}:{incarnation}:{position}:{process_id}"
        ))
    }

    fn parse(&self) -> Option<(&str, u64, u64, &str)> {
        let mut parts = self.0.strip_prefix(CURSOR_PREFIX)?.splitn(4, ':');
        let epoch = parts.next().filter(|part| !part.is_empty())?;
        let incarnation = parts.next()?.parse().ok()?;
        let position = parts.next()?.parse().ok()?;
        let process_id = parts.next().filter(|part| !part.is_empty())?;
        Some((epoch, incarnation, position, process_id))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessObservationGapReason {
    Overflow,
    Expired,
    SubscriberLagged,
    PublisherReplaced,
    RoutingUnavailable,
    ProcessIdReused,
    CrossProcess,
    InvalidCursor,
    PublisherJoinedMidRun,
    IncompleteGraph,
    ProjectionTruncated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessObservationCompleteness {
    Complete,
    Incomplete { reason: ProcessObservationGapReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessObservationProjection {
    /// Missing when the process has no local publisher.
    pub graph: Option<TraceLashlangGraph>,
    pub completeness: ProcessObservationCompleteness,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessObservationItem {
    Snapshot {
        cursor: ProcessObservationCursor,
        graph: TraceLashlangGraph,
        completeness: ProcessObservationCompleteness,
    },
    Event {
        cursor: ProcessObservationCursor,
        record: Box<TraceRecord>,
    },
    Gap {
        requested_cursor: Option<ProcessObservationCursor>,
        latest_cursor: Option<ProcessObservationCursor>,
        projection: ProcessObservationProjection,
        reason: ProcessObservationGapReason,
    },
}

impl ProcessObservationItem {
    /// Project a local item to the exact-version remote envelope body.
    pub fn into_remote(
        self,
        process_id: ProcessId,
        incarnation: u64,
    ) -> lash_remote_protocol::RemoteProcessObservationItem {
        use lash_remote_protocol::{
            RemoteProcessObservationCompleteness as RemoteCompleteness,
            RemoteProcessObservationGapReason as RemoteReason,
            RemoteProcessObservationItem as RemoteItem,
            RemoteProcessObservationProjection as RemoteProjection,
        };
        fn reason(value: ProcessObservationGapReason) -> RemoteReason {
            match value {
                ProcessObservationGapReason::Overflow => RemoteReason::Overflow,
                ProcessObservationGapReason::Expired => RemoteReason::Expired,
                ProcessObservationGapReason::SubscriberLagged => RemoteReason::SubscriberLagged,
                ProcessObservationGapReason::PublisherReplaced => RemoteReason::PublisherReplaced,
                ProcessObservationGapReason::RoutingUnavailable => RemoteReason::RoutingUnavailable,
                ProcessObservationGapReason::ProcessIdReused => RemoteReason::ProcessIdReused,
                ProcessObservationGapReason::CrossProcess => RemoteReason::CrossProcess,
                ProcessObservationGapReason::InvalidCursor => RemoteReason::InvalidCursor,
                ProcessObservationGapReason::PublisherJoinedMidRun => {
                    RemoteReason::PublisherJoinedMidRun
                }
                ProcessObservationGapReason::IncompleteGraph => RemoteReason::IncompleteGraph,
                ProcessObservationGapReason::ProjectionTruncated => {
                    RemoteReason::ProjectionTruncated
                }
            }
        }
        match self {
            Self::Snapshot {
                cursor,
                graph,
                completeness,
            } => {
                let completeness = match completeness {
                    ProcessObservationCompleteness::Complete => RemoteCompleteness::Complete,
                    ProcessObservationCompleteness::Incomplete { reason: gap_reason } => {
                        RemoteCompleteness::Incomplete {
                            reason: reason(gap_reason),
                        }
                    }
                };
                RemoteItem::Snapshot {
                    process_id,
                    incarnation,
                    cursor: cursor.0,
                    projection: RemoteProjection {
                        graph: Some(graph),
                        completeness,
                    },
                }
            }
            Self::Event { cursor, record } => RemoteItem::Event {
                process_id,
                incarnation,
                cursor: cursor.0,
                record,
            },
            Self::Gap {
                requested_cursor,
                latest_cursor,
                projection,
                reason: gap_reason,
            } => RemoteItem::Gap {
                process_id,
                incarnation,
                requested_cursor: requested_cursor.map(|cursor| cursor.0),
                latest_cursor: latest_cursor.map(|cursor| cursor.0),
                projection: RemoteProjection {
                    graph: projection.graph,
                    completeness: RemoteCompleteness::Incomplete {
                        reason: reason(gap_reason),
                    },
                },
                reason: reason(gap_reason),
            },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessObservationConfig {
    pub capacity: usize,
    pub ttl: Duration,
}

impl Default for ProcessObservationConfig {
    fn default() -> Self {
        Self {
            capacity: 2048,
            ttl: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
struct Published {
    position: u64,
    at: Instant,
    record: TraceRecord,
}

#[derive(Clone)]
enum PublishedNotification {
    Event(ProcessObservationCursor, Box<TraceRecord>),
    Replaced(ProcessObservationGapReason),
}

struct ProcessState {
    incarnation: u64,
    epoch: String,
    position: u64,
    base_position: u64,
    base_graph: Option<TraceLashlangGraph>,
    current_graph: Option<TraceLashlangGraph>,
    joined_at_start: bool,
    terminal: bool,
    last_published: Instant,
    ring: VecDeque<Published>,
    trim_reason: ProcessObservationGapReason,
    sender: broadcast::Sender<PublishedNotification>,
}

impl ProcessState {
    fn new(incarnation: u64, capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self {
            incarnation,
            epoch: uuid::Uuid::new_v4().to_string(),
            position: 0,
            base_position: 0,
            base_graph: None,
            current_graph: None,
            joined_at_start: false,
            terminal: false,
            last_published: Instant::now(),
            ring: VecDeque::new(),
            trim_reason: ProcessObservationGapReason::Overflow,
            sender,
        }
    }

    fn cursor(&self, process_id: &ProcessId, position: u64) -> ProcessObservationCursor {
        ProcessObservationCursor::new(process_id, self.incarnation, &self.epoch, position)
    }

    fn trim(&mut self, now: Instant, config: ProcessObservationConfig) {
        let mut evicted = Vec::new();
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
                evicted.push(old.record);
                self.trim_reason = if expired {
                    ProcessObservationGapReason::Expired
                } else {
                    ProcessObservationGapReason::Overflow
                };
            }
        }
        if !evicted.is_empty() {
            self.base_graph =
                TraceLashlangGraphStore::fold(self.base_graph.as_ref(), &evicted).ok();
        }
    }

    fn graph_at(&self, position: u64) -> Option<TraceLashlangGraph> {
        if position == self.position {
            return self.current_graph.clone();
        }
        let records = self
            .ring
            .iter()
            .filter(|item| item.position <= position)
            .map(|item| item.record.clone())
            .collect::<Vec<_>>();
        if records.is_empty() {
            self.base_graph.clone()
        } else {
            TraceLashlangGraphStore::fold(self.base_graph.as_ref(), &records).ok()
        }
    }

    fn snapshot_completeness(&self, graph: &TraceLashlangGraph) -> ProcessObservationCompleteness {
        let reason = if !self.joined_at_start {
            Some(ProcessObservationGapReason::PublisherJoinedMidRun)
        } else if graph.completeness != TraceLashlangGraphCompleteness::Complete {
            Some(ProcessObservationGapReason::IncompleteGraph)
        } else if !graph.node_retention.is_empty() {
            Some(ProcessObservationGapReason::ProjectionTruncated)
        } else {
            None
        };
        match reason {
            Some(reason) => ProcessObservationCompleteness::Incomplete { reason },
            None => ProcessObservationCompleteness::Complete,
        }
    }
}

/// One publisher-local observation route. A new core build creates a new epoch.
///
/// Lock order: the `states` map lock is taken before a per-process state lock
/// and only for lookup, insertion and release; graph folds run under the
/// per-process lock alone. A process's state is released once no subscription
/// holds it and it has either published `ExecutionFinished` or published
/// nothing for `ttl`, so retention follows the ring and TTL settings rather
/// than the life of the core.
pub struct ProcessObservationHub {
    config: ProcessObservationConfig,
    states: Mutex<HashMap<ProcessId, Arc<Mutex<ProcessState>>>>,
    last_sweep: Mutex<Instant>,
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
        }
    }

    pub fn subscribe(
        self: &std::sync::Arc<Self>,
        process_id: &ProcessId,
        incarnation: u64,
        from: Option<&ProcessObservationCursor>,
    ) -> ProcessObservationSubscription {
        let state = self.states.lock_recover().get(process_id).cloned();
        let Some(state) = state else {
            return ProcessObservationSubscription::gap(
                self.clone(),
                process_id.clone(),
                incarnation,
                from.cloned(),
                ProcessObservationGapReason::RoutingUnavailable,
                None,
                None,
            );
        };
        let mut publisher = state.lock_recover();
        publisher.trim(Instant::now(), self.config);
        let latest = publisher.cursor(process_id, publisher.position);
        let reason = match from.map(ProcessObservationCursor::parse) {
            Some(None) => Some(ProcessObservationGapReason::InvalidCursor),
            Some(Some((_, _, _, named_process))) if named_process != process_id.as_str() => {
                Some(ProcessObservationGapReason::CrossProcess)
            }
            Some(Some((_, named_incarnation, _, _))) if named_incarnation != incarnation => {
                Some(ProcessObservationGapReason::ProcessIdReused)
            }
            _ if publisher.incarnation != incarnation => {
                Some(ProcessObservationGapReason::ProcessIdReused)
            }
            Some(Some((epoch, _, _, _))) if epoch != publisher.epoch => {
                Some(ProcessObservationGapReason::PublisherReplaced)
            }
            Some(Some((_, _, position, _))) if position > publisher.position => {
                Some(ProcessObservationGapReason::RoutingUnavailable)
            }
            Some(Some((_, _, position, _))) if position < publisher.base_position => {
                Some(publisher.trim_reason)
            }
            _ => None,
        };
        if let Some(reason) = reason {
            return ProcessObservationSubscription::gap(
                self.clone(),
                process_id.clone(),
                incarnation,
                from.cloned(),
                reason,
                Some(latest),
                publisher.current_graph.clone(),
            );
        }
        let position = from
            .and_then(|cursor| cursor.parse().map(|(_, _, pos, _)| pos))
            .unwrap_or(publisher.position);
        let Some(graph) = publisher.graph_at(position) else {
            return ProcessObservationSubscription::gap(
                self.clone(),
                process_id.clone(),
                incarnation,
                from.cloned(),
                ProcessObservationGapReason::RoutingUnavailable,
                Some(latest),
                publisher.current_graph.clone(),
            );
        };
        let completeness = publisher.snapshot_completeness(&graph);
        let receiver = publisher.sender.subscribe();
        let replay = publisher
            .ring
            .iter()
            .filter(|item| item.position > position)
            .map(|item| ProcessObservationItem::Event {
                cursor: publisher.cursor(process_id, item.position),
                record: Box::new(item.record.clone()),
            })
            .collect();
        ProcessObservationSubscription {
            hub: self.clone(),
            state: Some(Arc::clone(&state)),
            process_id: process_id.clone(),
            incarnation,
            next: Some(ProcessObservationItem::Snapshot {
                cursor: publisher.cursor(process_id, position),
                graph,
                completeness,
            }),
            replay,
            receiver: Some(receiver),
            last_position: position,
        }
    }

    fn gap_at_latest(
        &self,
        process_id: &ProcessId,
        incarnation: u64,
        requested_cursor: Option<ProcessObservationCursor>,
        reason: ProcessObservationGapReason,
    ) -> ProcessObservationItem {
        let state = self.states.lock_recover().get(process_id).cloned();
        let state = state.as_ref().map(|state| state.lock_recover());
        let state = state
            .as_deref()
            .filter(|state| state.incarnation == incarnation);
        ProcessObservationItem::Gap {
            requested_cursor,
            latest_cursor: state.map(|state| state.cursor(process_id, state.position)),
            projection: ProcessObservationProjection {
                graph: state.and_then(|state| state.current_graph.clone()),
                completeness: ProcessObservationCompleteness::Incomplete { reason },
            },
            reason,
        }
    }

    /// Release every state no subscription holds whose publisher finished or
    /// went idle for `ttl`. Runs at most once per `ttl` from `append`.
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
    /// Append and subscribe release the map lock before folding a graph.
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
}

impl TraceSink for ProcessObservationHub {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let TraceEvent::LanguageExecution { language, event } = &record.event else {
            return Ok(());
        };
        if language != "lashlang" {
            return Ok(());
        }
        let TraceRuntimeSubject::Process { process_id } = &event.identity.subject else {
            return Ok(());
        };
        let Some(incarnation) = event.identity.incarnation() else {
            return Ok(());
        };
        let state = self
            .states
            .lock_recover()
            .entry(process_id.clone())
            .or_insert_with(|| {
                Arc::new(Mutex::new(ProcessState::new(
                    incarnation,
                    self.config.capacity,
                )))
            })
            .clone();
        let mut publisher = state.lock_recover();
        let replaced = if publisher.incarnation != incarnation {
            Some(ProcessObservationGapReason::ProcessIdReused)
        } else if publisher.current_graph.as_ref().is_some_and(|graph| {
            graph.graph_key != event.identity.graph_key()
                || matches!(
                    &event.payload,
                    TraceLanguageExecutionPayload::ExecutionStarted { .. }
                )
        }) {
            Some(ProcessObservationGapReason::PublisherReplaced)
        } else {
            None
        };
        if let Some(reason) = replaced {
            let _ = publisher
                .sender
                .send(PublishedNotification::Replaced(reason));
            *publisher = ProcessState::new(incarnation, self.config.capacity);
        }
        let Ok(graph) = TraceLashlangGraphStore::fold(
            publisher.current_graph.as_ref(),
            std::slice::from_ref(record),
        ) else {
            return Ok(());
        };
        if publisher.position == 0 {
            publisher.joined_at_start = matches!(
                &event.payload,
                TraceLanguageExecutionPayload::ExecutionStarted { .. }
            );
        }
        publisher.terminal = matches!(
            &event.payload,
            TraceLanguageExecutionPayload::ExecutionFinished { .. }
        );
        let now = Instant::now();
        publisher.current_graph = Some(graph);
        publisher.position += 1;
        publisher.last_published = now;
        let position = publisher.position;
        let cursor = publisher.cursor(process_id, position);
        publisher.ring.push_back(Published {
            position,
            at: now,
            record: record.clone(),
        });
        publisher.trim(now, self.config);
        let _ = publisher.sender.send(PublishedNotification::Event(
            cursor,
            Box::new(record.clone()),
        ));
        let terminal = publisher.terminal;
        drop(publisher);
        if terminal {
            self.evict_if_terminal(process_id, &state);
        }
        drop(state);
        self.sweep_idle(now);
        Ok(())
    }
}

pub struct ProcessObservationSubscription {
    hub: std::sync::Arc<ProcessObservationHub>,
    state: Option<Arc<Mutex<ProcessState>>>,
    process_id: ProcessId,
    incarnation: u64,
    next: Option<ProcessObservationItem>,
    replay: VecDeque<ProcessObservationItem>,
    receiver: Option<broadcast::Receiver<PublishedNotification>>,
    last_position: u64,
}

impl ProcessObservationSubscription {
    fn gap(
        hub: std::sync::Arc<ProcessObservationHub>,
        process_id: ProcessId,
        incarnation: u64,
        requested: Option<ProcessObservationCursor>,
        reason: ProcessObservationGapReason,
        latest: Option<ProcessObservationCursor>,
        graph: Option<TraceLashlangGraph>,
    ) -> Self {
        Self {
            hub,
            state: None,
            process_id,
            incarnation,
            next: Some(ProcessObservationItem::Gap {
                requested_cursor: requested,
                latest_cursor: latest,
                projection: ProcessObservationProjection {
                    graph,
                    completeness: ProcessObservationCompleteness::Incomplete { reason },
                },
                reason,
            }),
            replay: VecDeque::new(),
            receiver: None,
            last_position: 0,
        }
    }

    pub async fn recv(&mut self) -> Option<ProcessObservationItem> {
        if let Some(item) = self.next.take() {
            return Some(item);
        }
        if let Some(item) = self.replay.pop_front() {
            if let ProcessObservationItem::Event { cursor, .. } = &item {
                self.last_position = cursor
                    .parse()
                    .map_or(self.last_position, |(_, _, pos, _)| pos);
            }
            return Some(item);
        }
        loop {
            let receiver = self.receiver.as_mut()?;
            match receiver.recv().await {
                Ok(PublishedNotification::Event(cursor, record)) => {
                    let Some((_, _, position, _)) = cursor.parse() else {
                        continue;
                    };
                    if position <= self.last_position {
                        continue;
                    }
                    if position != self.last_position + 1 {
                        self.receiver = None;
                        return Some(self.hub.gap_at_latest(
                            &self.process_id,
                            self.incarnation,
                            Some(cursor),
                            ProcessObservationGapReason::SubscriberLagged,
                        ));
                    }
                    self.last_position = position;
                    return Some(ProcessObservationItem::Event { cursor, record });
                }
                Ok(PublishedNotification::Replaced(reason)) => {
                    self.receiver = None;
                    return Some(self.hub.gap_at_latest(
                        &self.process_id,
                        self.incarnation,
                        None,
                        reason,
                    ));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.receiver = None;
                    return Some(self.hub.gap_at_latest(
                        &self.process_id,
                        self.incarnation,
                        None,
                        ProcessObservationGapReason::SubscriberLagged,
                    ));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    self.receiver = None;
                    return Some(self.hub.gap_at_latest(
                        &self.process_id,
                        self.incarnation,
                        None,
                        ProcessObservationGapReason::PublisherReplaced,
                    ));
                }
            }
        }
    }

    /// Receive the next item with this subscription's process identity on the wire.
    pub async fn recv_remote(
        &mut self,
    ) -> Option<lash_remote_protocol::RemoteProcessObservationItem> {
        self.recv()
            .await
            .map(|item| item.into_remote(self.process_id.clone(), self.incarnation))
    }
}

impl Drop for ProcessObservationSubscription {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            self.hub.evict_if_terminal(&self.process_id, &state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_remote_protocol::{
        RemoteProcessObservationGapReason as RemoteGap, RemoteProcessObservationItem as RemoteItem,
        RemoteProcessObservationRequest,
    };
    use lash_trace::{
        TraceContext, TraceLanguageExecution, TraceLanguageExecutionGeneration,
        TraceLanguageExecutionIdentity, TraceLanguageExecutionMap, TraceLanguageExecutionMapNode,
        TraceLanguageExecutionPayload, TraceRuntimeScope,
    };

    fn record(process_id: &str, incarnation: u64, attempt: u32, occurrence: u64) -> TraceRecord {
        let payload = if occurrence == 0 {
            TraceLanguageExecutionPayload::ExecutionStarted {
                execution_map: TraceLanguageExecutionMap {
                    nodes: vec![TraceLanguageExecutionMapNode {
                        id: "node".to_string(),
                        site: lash_sansio::WorkflowExecutionSite::new(
                            "main",
                            [0],
                            lash_sansio::ExecutionNodeKind::Call,
                            "call()",
                        ),
                        kind: lash_sansio::ExecutionNodeKind::Call,
                        label: "call()".to_string(),
                        branch_memberships: Vec::new(),
                        label_metadata: None,
                    }],
                    edges: Vec::new(),
                },
            }
        } else {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id: "node".to_string(),
                node_kind: lash_sansio::ExecutionNodeKind::Call,
                label: "call()".to_string(),
                occurrence,
                call_id: None,
            }
        };
        TraceRecord::new(
            TraceContext::default(),
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event: TraceLanguageExecution {
                    event_key: format!("{process_id}:{incarnation}:{attempt}:{occurrence}"),
                    identity: TraceLanguageExecutionIdentity {
                        scope: TraceRuntimeScope::none(),
                        subject: TraceRuntimeSubject::Process {
                            process_id: ProcessId::from(process_id),
                        },
                        source_identity: "source".to_string(),
                        module_ref: "module".to_string(),
                        entry_kind: "main".to_string(),
                        entry_ref: None,
                        entry_name: "main".to_string(),
                        restate_invocation_id: None,
                        generation: Some(TraceLanguageExecutionGeneration::new(
                            attempt,
                            incarnation,
                        )),
                    },
                    payload,
                },
            },
        )
    }

    fn finished_record(process_id: &str) -> TraceRecord {
        let mut finished = record(process_id, 1, 1, 1);
        let TraceEvent::LanguageExecution { event, .. } = &mut finished.event else {
            unreachable!()
        };
        event.payload = TraceLanguageExecutionPayload::ExecutionFinished {
            status: lash_trace::TraceLanguageExecutionStatus::Completed,
            error: None,
        };
        finished
    }

    fn append(hub: &ProcessObservationHub, process_id: &str, incarnation: u64, occurrence: u64) {
        hub.append(&record(process_id, incarnation, 1, occurrence))
            .expect("publish observation");
    }

    async fn initial_cursor(
        hub: &std::sync::Arc<ProcessObservationHub>,
        process_id: &str,
        incarnation: u64,
    ) -> ProcessObservationCursor {
        let mut subscriber = hub.subscribe(&ProcessId::from(process_id), incarnation, None);
        let Some(ProcessObservationItem::Snapshot { cursor, .. }) = subscriber.recv().await else {
            panic!("expected initial snapshot");
        };
        cursor
    }

    async fn assert_gap_wire(
        mut subscriber: ProcessObservationSubscription,
        process_id: &str,
        incarnation: u64,
        expected: ProcessObservationGapReason,
    ) {
        let item = subscriber.recv().await.expect("typed gap");
        let ProcessObservationItem::Gap {
            reason, projection, ..
        } = &item
        else {
            panic!("expected typed gap");
        };
        assert_eq!(*reason, expected);
        assert_eq!(
            projection.completeness,
            ProcessObservationCompleteness::Incomplete { reason: expected }
        );
        let remote = item.into_remote(ProcessId::from(process_id), incarnation);
        let wire = remote.encode_json().expect("encode remote gap");
        assert_eq!(
            lash_remote_protocol::RemoteProcessObservationItem::decode_json(&wire)
                .expect("decode remote gap"),
            remote,
        );
    }

    #[tokio::test]
    async fn process_subscription_replays_with_snapshot_boundary_and_wire_round_trip() {
        let hub = std::sync::Arc::new(ProcessObservationHub::default());
        let id = ProcessId::from("process:one");
        append(&hub, id.as_str(), 1, 0);
        let mut uninterrupted = hub.subscribe(&id, 1, None);
        let snapshot = uninterrupted.recv().await.expect("initial snapshot");
        let ProcessObservationItem::Snapshot { cursor, .. } = &snapshot else {
            panic!("initial snapshot");
        };
        let cursor = cursor.clone();
        let wire = snapshot
            .clone()
            .into_remote(id.clone(), 1)
            .encode_json()
            .expect("snapshot wire");
        assert!(matches!(
            lash_remote_protocol::RemoteProcessObservationItem::decode_json(&wire),
            Ok(lash_remote_protocol::RemoteProcessObservationItem::Snapshot { .. })
        ));
        append(&hub, id.as_str(), 1, 1);
        let Some(ProcessObservationItem::Event {
            cursor: after_one,
            record: first,
        }) = uninterrupted.recv().await
        else {
            panic!("first node event");
        };
        assert_eq!(
            after_one.parse().expect("cursor").2,
            cursor.parse().expect("cursor").2 + 1
        );
        append(&hub, id.as_str(), 1, 2);
        let Some(ProcessObservationItem::Event {
            cursor: after_two,
            record: second,
        }) = uninterrupted.recv().await
        else {
            panic!("second node event");
        };
        let first_graph = match snapshot {
            ProcessObservationItem::Snapshot { graph, .. } => graph,
            _ => unreachable!(),
        };
        let uninterrupted_graph = TraceLashlangGraphStore::fold(
            Some(&first_graph),
            &[first.as_ref().clone(), second.as_ref().clone()],
        )
        .expect("uninterrupted fold");

        drop(uninterrupted);
        let mut resumed = hub.subscribe(&id, 1, Some(&after_one));
        let Some(ProcessObservationItem::Snapshot {
            cursor: boundary,
            graph,
            ..
        }) = resumed.recv().await
        else {
            panic!("resumed snapshot");
        };
        assert_eq!(boundary, after_one);
        let Some(ProcessObservationItem::Event {
            cursor: replay_cursor,
            record: replay,
        }) = resumed.recv().await
        else {
            panic!("retained replay");
        };
        assert_eq!(replay_cursor, after_two);
        assert_eq!(replay, second);
        let replay_graph = TraceLashlangGraphStore::fold(Some(&graph), &[replay.as_ref().clone()])
            .expect("replayed fold");
        assert_eq!(replay_graph, uninterrupted_graph);
        let remote = ProcessObservationItem::Event {
            cursor: replay_cursor,
            record: first,
        }
        .into_remote(id, 1);
        let wire = remote.encode_json().expect("event wire");
        assert_eq!(
            lash_remote_protocol::RemoteProcessObservationItem::decode_json(&wire)
                .expect("event decode"),
            remote
        );
    }

    #[tokio::test]
    async fn process_subscription_gap_matrix_is_complete_locally_and_on_wire() {
        let id = ProcessId::from("process:one");
        let hub = std::sync::Arc::new(ProcessObservationHub::default());
        assert_gap_wire(
            hub.subscribe(&id, 1, None),
            id.as_str(),
            1,
            ProcessObservationGapReason::RoutingUnavailable,
        )
        .await;

        let overflow = std::sync::Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 1,
            ttl: Duration::from_secs(120),
        }));
        append(&overflow, id.as_str(), 1, 0);
        let cursor = initial_cursor(&overflow, id.as_str(), 1).await;
        append(&overflow, id.as_str(), 1, 1);
        append(&overflow, id.as_str(), 1, 2);
        assert_gap_wire(
            overflow.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::Overflow,
        )
        .await;

        let expiry = std::sync::Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 2,
            ttl: Duration::ZERO,
        }));
        append(&expiry, id.as_str(), 1, 0);
        let cursor = expiry
            .states
            .lock_recover()
            .get(&id)
            .expect("publisher")
            .lock_recover()
            .cursor(&id, 0);
        assert_gap_wire(
            expiry.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::Expired,
        )
        .await;

        let replacement = std::sync::Arc::new(ProcessObservationHub::default());
        append(&replacement, id.as_str(), 1, 0);
        let cursor = initial_cursor(&replacement, id.as_str(), 1).await;
        let mut stale = replacement.subscribe(&id, 1, None);
        assert!(matches!(
            stale.recv().await,
            Some(ProcessObservationItem::Snapshot { .. })
        ));
        replacement
            .append(&record(id.as_str(), 1, 2, 0))
            .expect("new publisher");
        assert_gap_wire(
            stale,
            id.as_str(),
            1,
            ProcessObservationGapReason::PublisherReplaced,
        )
        .await;
        assert_gap_wire(
            replacement.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::PublisherReplaced,
        )
        .await;
        let same_key_cursor = initial_cursor(&replacement, id.as_str(), 1).await;
        replacement
            .append(&record(id.as_str(), 1, 2, 0))
            .expect("same-key publisher restart");
        assert_gap_wire(
            replacement.subscribe(&id, 1, Some(&same_key_cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::PublisherReplaced,
        )
        .await;

        let reuse = std::sync::Arc::new(ProcessObservationHub::default());
        append(&reuse, id.as_str(), 1, 0);
        let cursor = initial_cursor(&reuse, id.as_str(), 1).await;
        append(&reuse, id.as_str(), 2, 0);
        assert_gap_wire(
            reuse.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::ProcessIdReused,
        )
        .await;

        let other = ProcessId::from("process:other");
        append(&reuse, other.as_str(), 1, 0);
        assert_gap_wire(
            reuse.subscribe(&other, 1, Some(&cursor)),
            other.as_str(),
            1,
            ProcessObservationGapReason::CrossProcess,
        )
        .await;
        let invalid = ProcessObservationCursor::from_token("invalid");
        assert_gap_wire(
            reuse.subscribe(&other, 1, Some(&invalid)),
            other.as_str(),
            1,
            ProcessObservationGapReason::InvalidCursor,
        )
        .await;

        let lag = std::sync::Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 1,
            ttl: Duration::from_secs(120),
        }));
        append(&lag, id.as_str(), 1, 0);
        let mut subscriber = lag.subscribe(&id, 1, None);
        assert!(matches!(
            subscriber.recv().await,
            Some(ProcessObservationItem::Snapshot { .. })
        ));
        append(&lag, id.as_str(), 1, 1);
        append(&lag, id.as_str(), 1, 2);
        assert_gap_wire(
            subscriber,
            id.as_str(),
            1,
            ProcessObservationGapReason::SubscriberLagged,
        )
        .await;
    }

    #[tokio::test]
    async fn publisher_joined_mid_run_never_claims_a_complete_snapshot() {
        let id = ProcessId::from("process:joined-late");
        let hub = Arc::new(ProcessObservationHub::default());
        append(&hub, id.as_str(), 1, 1);
        let mut subscriber = hub.subscribe(&id, 1, None);
        let item = subscriber.recv().await.expect("snapshot");
        let ProcessObservationItem::Snapshot {
            graph,
            completeness,
            ..
        } = &item
        else {
            panic!("expected snapshot");
        };
        assert_eq!(
            graph.completeness,
            TraceLashlangGraphCompleteness::IncompleteMap
        );
        assert_eq!(
            completeness,
            &ProcessObservationCompleteness::Incomplete {
                reason: ProcessObservationGapReason::PublisherJoinedMidRun
            }
        );
        let wire = item
            .into_remote(id, 1)
            .encode_json()
            .expect("wire snapshot");
        assert!(matches!(
            lash_remote_protocol::RemoteProcessObservationItem::decode_json(&wire),
            Ok(lash_remote_protocol::RemoteProcessObservationItem::Snapshot { projection, .. })
                if projection.completeness
                    == lash_remote_protocol::RemoteProcessObservationCompleteness::Incomplete {
                        reason: lash_remote_protocol::RemoteProcessObservationGapReason::PublisherJoinedMidRun
                    }
        ));
    }

    #[tokio::test]
    async fn live_process_id_reuse_is_not_a_publisher_restart() {
        let id = ProcessId::from("process:reused-live");
        let hub = Arc::new(ProcessObservationHub::default());
        append(&hub, id.as_str(), 1, 0);
        let mut subscriber = hub.subscribe(&id, 1, None);
        assert!(matches!(
            subscriber.recv().await,
            Some(ProcessObservationItem::Snapshot { .. })
        ));
        append(&hub, id.as_str(), 2, 0);
        assert_gap_wire(
            subscriber,
            id.as_str(),
            1,
            ProcessObservationGapReason::ProcessIdReused,
        )
        .await;
    }

    #[tokio::test]
    async fn worker_restart_epoch_refuses_the_old_cursor() {
        let id = ProcessId::from("process:worker-restart");
        let before = Arc::new(ProcessObservationHub::default());
        append(&before, id.as_str(), 1, 0);
        let cursor = initial_cursor(&before, id.as_str(), 1).await;
        let restarted = Arc::new(ProcessObservationHub::default());
        assert_gap_wire(
            restarted.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::RoutingUnavailable,
        )
        .await;
        append(&restarted, id.as_str(), 1, 0);
        assert_gap_wire(
            restarted.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::PublisherReplaced,
        )
        .await;
    }

    #[tokio::test]
    async fn another_worker_publisher_has_an_independent_epoch() {
        let id = ProcessId::from("process:other-worker");
        let first_worker = Arc::new(ProcessObservationHub::default());
        first_worker
            .append(&record(id.as_str(), 1, 1, 0))
            .expect("first worker");
        let cursor = initial_cursor(&first_worker, id.as_str(), 1).await;
        let other_worker = Arc::new(ProcessObservationHub::default());
        other_worker
            .append(&record(id.as_str(), 1, 2, 0))
            .expect("other worker");
        assert_gap_wire(
            other_worker.subscribe(&id, 1, Some(&cursor)),
            id.as_str(),
            1,
            ProcessObservationGapReason::PublisherReplaced,
        )
        .await;
    }

    #[tokio::test]
    async fn finished_processes_release_their_hub_entries_without_subscribers() {
        let hub = Arc::new(ProcessObservationHub::default());
        for index in 0..64 {
            let id = format!("process:finished:{index}");
            append(&hub, &id, 1, 0);
            hub.append(&finished_record(&id))
                .expect("terminal observation");
        }
        assert_eq!(hub.states.lock_recover().len(), 0);

        let watched = ProcessId::from("process:finished:watched");
        append(&hub, watched.as_str(), 1, 0);
        let mut subscriber = hub.subscribe(&watched, 1, None);
        assert!(matches!(
            subscriber.recv().await,
            Some(ProcessObservationItem::Snapshot { .. })
        ));
        hub.append(&finished_record(watched.as_str()))
            .expect("terminal observation");
        assert_eq!(
            hub.states.lock_recover().len(),
            1,
            "a live subscriber keeps its finished process"
        );
        assert!(matches!(
            subscriber.recv().await,
            Some(ProcessObservationItem::Event { .. })
        ));
        drop(subscriber);
        assert_eq!(hub.states.lock_recover().len(), 0);
    }

    #[tokio::test]
    async fn idle_unfinished_processes_are_released_after_the_ttl() {
        let hub = Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 8,
            ttl: Duration::from_millis(1),
        }));
        for index in 0..16 {
            append(&hub, &format!("process:suspended:{index}"), 1, 0);
        }
        let watched = ProcessId::from("process:suspended:watched");
        append(&hub, watched.as_str(), 1, 0);
        let _subscriber = hub.subscribe(&watched, 1, None);
        tokio::time::sleep(Duration::from_millis(5)).await;
        append(&hub, "process:live", 1, 0);
        let mut remaining = hub
            .states
            .lock_recover()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(
            remaining,
            [ProcessId::from("process:live"), watched],
            "idle states without subscribers are released; a held one stays"
        );
    }

    async fn through_remote_facade(
        core: &crate::LashCore,
        process_id: &str,
        incarnation: u64,
        cursor: Option<String>,
    ) -> RemoteItem {
        let request = RemoteProcessObservationRequest {
            process_id: ProcessId::from(process_id),
            incarnation,
            cursor,
        };
        let request = RemoteProcessObservationRequest::decode_json(
            &request.encode_json().expect("encode request"),
        )
        .expect("decode request");
        let process_ref = lash_core::ProcessRef::new(
            request.process_id.clone(),
            lash_core::ProcessIncarnation::from_registration_sequence(request.incarnation),
        );
        let cursor = request
            .cursor
            .as_deref()
            .map(ProcessObservationCursor::from_token);
        let mut local = core
            .processes()
            .subscribe_observation(&process_ref, cursor.as_ref());
        let mut subscription = core
            .processes()
            .subscribe_observation_remote(&request)
            .expect("facade route");
        let item = subscription.recv_remote().await.expect("first item");
        let decoded = RemoteItem::decode_json(&item.encode_json().expect("encode item"))
            .expect("client decode");
        assert_eq!(
            local
                .recv()
                .await
                .expect("local item")
                .into_remote(request.process_id, request.incarnation),
            decoded,
        );
        decoded
    }

    fn assert_remote_gap(item: RemoteItem, expected: RemoteGap) {
        assert!(
            matches!(&item, RemoteItem::Gap { reason, .. } if reason == &expected),
            "expected {expected:?}, received {item:?}"
        );
    }

    #[tokio::test]
    async fn remote_facade_gap_matrix_and_drop_resubscribe() {
        let mut core = crate::tests::standard_core();
        let hub = Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 1,
            ttl: Duration::from_secs(120),
        }));
        core.process_observation_hub = Arc::clone(&hub);
        let id = "process:facade-wire";
        assert_remote_gap(
            through_remote_facade(&core, id, 1, None).await,
            RemoteGap::RoutingUnavailable,
        );
        append(&hub, id, 1, 0);
        let RemoteItem::Snapshot {
            cursor, projection, ..
        } = through_remote_facade(&core, id, 1, None).await
        else {
            panic!("snapshot")
        };
        assert!(projection.graph.is_some());
        assert_eq!(
            through_remote_facade(&core, id, 1, None).await,
            through_remote_facade(&core, id, 1, None).await,
            "resubscribing after the first subscription drops rebuilds the snapshot"
        );
        assert_remote_gap(
            through_remote_facade(&core, id, 1, Some("invalid".into())).await,
            RemoteGap::InvalidCursor,
        );
        append(&hub, "process:other", 1, 0);
        assert_remote_gap(
            through_remote_facade(&core, "process:other", 1, Some(cursor.clone())).await,
            RemoteGap::CrossProcess,
        );
        append(&hub, id, 1, 1);
        append(&hub, id, 1, 2);
        assert_remote_gap(
            through_remote_facade(&core, id, 1, Some(cursor.clone())).await,
            RemoteGap::Overflow,
        );
        let current_cursor = match through_remote_facade(&core, id, 1, None).await {
            RemoteItem::Snapshot { cursor, .. } => cursor,
            _ => panic!("current snapshot"),
        };
        append(&hub, id, 2, 0);
        assert_remote_gap(
            through_remote_facade(&core, id, 1, Some(current_cursor)).await,
            RemoteGap::ProcessIdReused,
        );
        let zero = RemoteProcessObservationRequest {
            process_id: ProcessId::from(id),
            incarnation: 0,
            cursor: None,
        };
        assert!(
            RemoteProcessObservationRequest::decode_json(&zero.encode_json().expect("zero wire"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn remote_facade_reports_expiry_lag_and_replaced_publisher() {
        let id = "process:facade-gaps";
        let mut core = crate::tests::standard_core();
        let expiring = Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 4,
            ttl: Duration::from_millis(1),
        }));
        core.process_observation_hub = Arc::clone(&expiring);
        append(&expiring, id, 1, 0);
        let cursor = match through_remote_facade(&core, id, 1, None).await {
            RemoteItem::Snapshot { cursor, .. } => cursor,
            _ => panic!("snapshot"),
        };
        tokio::time::sleep(Duration::from_millis(3)).await;
        append(&expiring, id, 1, 1);
        tokio::time::sleep(Duration::from_millis(3)).await;
        append(&expiring, id, 1, 2);
        assert_remote_gap(
            through_remote_facade(&core, id, 1, Some(cursor.clone())).await,
            RemoteGap::Expired,
        );

        let restarted = Arc::new(ProcessObservationHub::default());
        core.process_observation_hub = Arc::clone(&restarted);
        append(&restarted, id, 1, 0);
        assert_remote_gap(
            through_remote_facade(&core, id, 1, Some(cursor)).await,
            RemoteGap::PublisherReplaced,
        );

        let lagging = Arc::new(ProcessObservationHub::new(ProcessObservationConfig {
            capacity: 1,
            ttl: Duration::from_secs(120),
        }));
        core.process_observation_hub = Arc::clone(&lagging);
        append(&lagging, id, 1, 0);
        let request = RemoteProcessObservationRequest {
            process_id: ProcessId::from(id),
            incarnation: 1,
            cursor: None,
        };
        let request = RemoteProcessObservationRequest::decode_json(
            &request.encode_json().expect("request wire"),
        )
        .expect("client request");
        let mut subscriber = core
            .processes()
            .subscribe_observation_remote(&request)
            .expect("facade route");
        assert!(matches!(
            subscriber.recv_remote().await,
            Some(RemoteItem::Snapshot { .. })
        ));
        append(&lagging, id, 1, 1);
        append(&lagging, id, 1, 2);
        assert_remote_gap(
            RemoteItem::decode_json(
                &subscriber
                    .recv_remote()
                    .await
                    .expect("lag gap")
                    .encode_json()
                    .expect("gap wire"),
            )
            .expect("client gap"),
            RemoteGap::SubscriberLagged,
        );
    }
}
