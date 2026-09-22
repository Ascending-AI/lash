//! Best-effort, process-scoped replay of identified execution observations.
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lash_sansio::ProcessId;
use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceEvent, TraceLanguageExecutionPayload, TraceLashlangGraph, TraceLashlangGraphStore,
    TraceRecord, TraceRuntimeSubject, TraceSink, TraceSinkError,
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
        projection: ProcessObservationProjection,
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
            }
        }
        match self {
            Self::Snapshot { cursor, projection } => {
                let Some(graph) = projection.graph else {
                    return RemoteItem::Gap {
                        process_id,
                        incarnation,
                        requested_cursor: None,
                        latest_cursor: Some(cursor.0),
                        projection: RemoteProjection {
                            graph: None,
                            completeness: RemoteCompleteness::Incomplete {
                                reason: RemoteReason::RoutingUnavailable,
                            },
                        },
                        reason: RemoteReason::RoutingUnavailable,
                    };
                };
                RemoteItem::Snapshot {
                    process_id,
                    incarnation,
                    cursor: cursor.0,
                    projection: RemoteProjection {
                        graph: Some(graph),
                        completeness: RemoteCompleteness::Complete,
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

struct ProcessState {
    incarnation: u64,
    epoch: String,
    position: u64,
    base_position: u64,
    base_graph: Option<TraceLashlangGraph>,
    current_graph: Option<TraceLashlangGraph>,
    ring: VecDeque<Published>,
    trim_reason: ProcessObservationGapReason,
    sender: broadcast::Sender<(ProcessObservationCursor, TraceRecord)>,
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
            ring: VecDeque::new(),
            trim_reason: ProcessObservationGapReason::Overflow,
            sender,
        }
    }

    fn cursor(&self, process_id: &ProcessId, position: u64) -> ProcessObservationCursor {
        ProcessObservationCursor::new(process_id, self.incarnation, &self.epoch, position)
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
                self.base_graph = TraceLashlangGraphStore::fold(
                    self.base_graph.as_ref(),
                    std::slice::from_ref(&old.record),
                )
                .ok();
                self.trim_reason = if expired {
                    ProcessObservationGapReason::Expired
                } else {
                    ProcessObservationGapReason::Overflow
                };
            }
        }
    }

    fn graph_at(&self, position: u64) -> Option<TraceLashlangGraph> {
        if position == self.position {
            return self.current_graph.clone();
        }
        let mut graph = self.base_graph.clone();
        for item in self.ring.iter().filter(|item| item.position <= position) {
            graph =
                TraceLashlangGraphStore::fold(graph.as_ref(), std::slice::from_ref(&item.record))
                    .ok();
        }
        graph
    }
}

/// One publisher-local observation route. A new core build creates a new epoch.
pub struct ProcessObservationHub {
    config: ProcessObservationConfig,
    states: Mutex<HashMap<ProcessId, ProcessState>>,
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
        }
    }

    pub fn subscribe(
        self: &std::sync::Arc<Self>,
        process_id: &ProcessId,
        incarnation: u64,
        from: Option<&ProcessObservationCursor>,
    ) -> ProcessObservationSubscription {
        let mut states = self.states.lock_recover();
        let Some(state) = states.get_mut(process_id) else {
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
        state.trim(Instant::now(), self.config);
        let latest = state.cursor(process_id, state.position);
        let reason = match from.map(ProcessObservationCursor::parse) {
            Some(None) => Some(ProcessObservationGapReason::InvalidCursor),
            Some(Some((_, _, _, named_process))) if named_process != process_id.as_str() => {
                Some(ProcessObservationGapReason::CrossProcess)
            }
            Some(Some((_, named_incarnation, _, _))) if named_incarnation != incarnation => {
                Some(ProcessObservationGapReason::ProcessIdReused)
            }
            _ if state.incarnation != incarnation => {
                Some(ProcessObservationGapReason::ProcessIdReused)
            }
            Some(Some((epoch, _, _, _))) if epoch != state.epoch => {
                Some(ProcessObservationGapReason::PublisherReplaced)
            }
            Some(Some((_, _, position, _))) if position > state.position => {
                Some(ProcessObservationGapReason::RoutingUnavailable)
            }
            Some(Some((_, _, position, _))) if position < state.base_position => {
                Some(state.trim_reason)
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
                state.current_graph.clone(),
            );
        }
        let position = from
            .and_then(|cursor| cursor.parse().map(|(_, _, pos, _)| pos))
            .unwrap_or(state.position);
        let Some(graph) = state.graph_at(position) else {
            return ProcessObservationSubscription::gap(
                self.clone(),
                process_id.clone(),
                incarnation,
                from.cloned(),
                ProcessObservationGapReason::RoutingUnavailable,
                Some(latest),
                state.current_graph.clone(),
            );
        };
        let receiver = state.sender.subscribe();
        let replay = state
            .ring
            .iter()
            .filter(|item| item.position > position)
            .map(|item| ProcessObservationItem::Event {
                cursor: state.cursor(process_id, item.position),
                record: Box::new(item.record.clone()),
            })
            .collect();
        ProcessObservationSubscription {
            hub: self.clone(),
            process_id: process_id.clone(),
            incarnation,
            next: Some(ProcessObservationItem::Snapshot {
                cursor: state.cursor(process_id, position),
                projection: ProcessObservationProjection {
                    graph: Some(graph),
                    completeness: ProcessObservationCompleteness::Complete,
                },
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
        let states = self.states.lock_recover();
        let state = states
            .get(process_id)
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
        let mut states = self.states.lock_recover();
        let state = states
            .entry(process_id.clone())
            .or_insert_with(|| ProcessState::new(incarnation, self.config.capacity));
        if state.incarnation != incarnation
            || state.current_graph.as_ref().is_some_and(|graph| {
                graph.graph_key != event.identity.graph_key()
                    || matches!(
                        &event.payload,
                        TraceLanguageExecutionPayload::ExecutionStarted { .. }
                    )
            })
        {
            *state = ProcessState::new(incarnation, self.config.capacity);
        }
        let Ok(graph) = TraceLashlangGraphStore::fold(
            state.current_graph.as_ref(),
            std::slice::from_ref(record),
        ) else {
            return Ok(());
        };
        state.current_graph = Some(graph);
        state.position += 1;
        let cursor = state.cursor(process_id, state.position);
        state.ring.push_back(Published {
            position: state.position,
            at: Instant::now(),
            record: record.clone(),
        });
        state.trim(Instant::now(), self.config);
        let _ = state.sender.send((cursor, record.clone()));
        Ok(())
    }
}

pub struct ProcessObservationSubscription {
    hub: std::sync::Arc<ProcessObservationHub>,
    process_id: ProcessId,
    incarnation: u64,
    next: Option<ProcessObservationItem>,
    replay: VecDeque<ProcessObservationItem>,
    receiver: Option<broadcast::Receiver<(ProcessObservationCursor, TraceRecord)>>,
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
                Ok((cursor, record)) => {
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
                    return Some(ProcessObservationItem::Event {
                        cursor,
                        record: Box::new(record),
                    });
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

#[cfg(test)]
mod tests {
    use super::*;
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
                            "call",
                            "call()",
                        ),
                        kind: "call".to_string(),
                        label: "call()".to_string(),
                        label_metadata: None,
                    }],
                    edges: Vec::new(),
                },
            }
        } else {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id: "node".to_string(),
                node_kind: "call".to_string(),
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
            ProcessObservationItem::Snapshot { projection, .. } => projection.graph.expect("graph"),
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
            projection,
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
        let replay_graph =
            TraceLashlangGraphStore::fold(projection.graph.as_ref(), &[replay.as_ref().clone()])
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
}
