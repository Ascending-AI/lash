use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    TRACE_SCHEMA_VERSION, TraceBranchSelection, TraceEvent, TraceLabelMetadata,
    TraceLanguageExecution, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionIdentity as LanguageIdentity,
    TraceLanguageExecutionMap as LanguageExecutionMap, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceRecord, TraceRuntimeScope,
    TraceRuntimeSubject, TraceSink, TraceSinkError, ensure_trace_schema_version,
};

mod model;
pub use model::*;

/// In-memory store backed by the same pure fold exposed to hosts.
#[derive(Default)]
pub struct TraceLashlangGraphStore {
    inner: Mutex<BTreeMap<String, TraceLashlangGraph>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TraceLashlangGraphFoldError {
    #[error("the fold requires at least one language-execution event")]
    NoLanguageExecutionEvents,
    #[error("the fold input spans graph keys `{first}` and `{other}`")]
    MixedGraphKeys { first: String, other: String },
    #[error("the previous snapshot is for `{previous}`, not `{event}`")]
    PreviousGraphMismatch { previous: String, event: String },
    #[error("history limit must be greater than zero")]
    ZeroHistoryLimit,
}

impl TraceLashlangGraphStore {
    /// Returns a snapshot for one observed Lashlang graph key.
    pub fn graph(&self, graph_key: &str) -> Option<TraceLashlangGraph> {
        self.inner.lock_recover().get(graph_key).cloned()
    }

    /// Returns snapshots for all observed executions in stable graph-key order.
    pub fn graphs(&self) -> Vec<TraceLashlangGraph> {
        self.inner.lock_recover().values().cloned().collect()
    }

    /// Clears all reduced graph projections and replay de-duplication keys.
    pub fn clear(&self) {
        self.inner.lock_recover().clear();
    }

    /// Pure deterministic bounded fold.
    ///
    /// The snapshot retains canonical events, so folding partitions is byte
    /// identical to folding their concatenation. Input order is irrelevant;
    /// terminal transitions dominate starts for one occurrence; a later
    /// occurrence remains visible; identical duplicates disappear; divergent
    /// duplicates become typed conflicts. When the limit is exceeded, only
    /// identities after the canonical watermark remain eligible, making later
    /// batches obey the same truncation decision as a batch fold.
    pub fn fold(
        previous: Option<&TraceLashlangGraph>,
        records: &[TraceRecord],
    ) -> Result<TraceLashlangGraph, TraceLashlangGraphFoldError> {
        Self::fold_with_history_limit(previous, records, DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT)
    }

    pub fn fold_with_history_limit(
        previous: Option<&TraceLashlangGraph>,
        records: &[TraceRecord],
        history_limit: usize,
    ) -> Result<TraceLashlangGraph, TraceLashlangGraphFoldError> {
        fold_lashlang_graph(previous, records, history_limit)
    }
}

impl TraceSink for TraceLashlangGraphStore {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let TraceEvent::LanguageExecution { language, event } = &record.event else {
            return Ok(());
        };
        // Any dialect's executions reduce into this projection. The events
        // describe the Lashlang VM's node and edge lifecycle under every
        // dialect. The `language` field describes the source that ran, and
        // dropping a session's graph because its source was TypeScript would
        // empty every TypeScript session's execution view. The filter existed
        // when `lashlang` was the only value this field could take.
        let _ = language;
        let graph_key = event.identity.graph_key();
        let mut graphs = self.inner.lock_recover();
        let next = Self::fold(graphs.get(&graph_key), std::slice::from_ref(record))
            .expect("one language-execution record always forms a valid fold input");
        graphs.insert(graph_key, next);
        Ok(())
    }
}

pub fn fold_lashlang_graph(
    previous: Option<&TraceLashlangGraph>,
    records: &[TraceRecord],
    history_limit: usize,
) -> Result<TraceLashlangGraph, TraceLashlangGraphFoldError> {
    if history_limit == 0 {
        return Err(TraceLashlangGraphFoldError::ZeroHistoryLimit);
    }
    let incoming = records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::LanguageExecution { event, .. } => Some((record.timestamp, event.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let first_event = incoming
        .first()
        .map(|(_, event)| event)
        .or_else(|| previous.and_then(|graph| graph.history.first().map(|item| &item.event)))
        .ok_or(TraceLashlangGraphFoldError::NoLanguageExecutionEvents)?;
    let graph_key = first_event.identity.graph_key();
    if let Some(previous) = previous
        && previous.graph_key != graph_key
    {
        return Err(TraceLashlangGraphFoldError::PreviousGraphMismatch {
            previous: previous.graph_key.clone(),
            event: graph_key,
        });
    }
    for (_, event) in &incoming {
        let other = event.identity.graph_key();
        if other != graph_key {
            return Err(TraceLashlangGraphFoldError::MixedGraphKeys {
                first: graph_key.clone(),
                other,
            });
        }
    }

    let identity = canonical_language_identity(previous, &incoming);
    let status = incoming.iter().fold(
        previous.map_or(LanguageExecutionStatus::Running, |graph| graph.status),
        |status, (_, event)| match &event.payload {
            TraceLanguageExecutionPayload::ExecutionFinished { status: next, .. } => {
                canonical_execution_status(status, *next)
            }
            _ => status,
        },
    );
    let mut history = previous
        .map(|graph| {
            graph
                .history
                .iter()
                .cloned()
                .map(|item| (item.identity.clone(), item))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut conflict_variants = previous
        .map(|graph| {
            graph
                .conflicts
                .iter()
                .map(|conflict| {
                    (
                        conflict.identity.clone(),
                        conflict.variants.iter().cloned().collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut execution_map = previous.and_then(|graph| graph.execution_map.clone());
    let mut truncation_watermark = previous.and_then(|graph| graph.truncation_watermark.clone());

    for (timestamp, mut event) in incoming {
        // Publication-local labels are not logical identity. The fold stores
        // only the canonical observation so two delivery paths with different
        // publisher keys still deduplicate.
        event.event_key.clear();
        let event_identity = event_identity(&event);
        if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map: map } =
            &event.payload
        {
            // The map is a bounded static fact, not retained history. Merge it
            // even after its event identity falls below the history watermark
            // so a late seed produces the same snapshot as a batch fold.
            execution_map = Some(match execution_map {
                None => map.clone(),
                Some(current) if current == *map => current,
                Some(current) => canonical_value(current, map.clone()),
            });
        }
        if truncation_watermark
            .as_ref()
            .is_some_and(|watermark| event_identity <= *watermark)
        {
            continue;
        }
        let candidate = TraceLashlangGraphHistoryEvent {
            identity: event_identity.clone(),
            timestamp,
            event,
        };
        match history.entry(event_identity.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get() != &candidate {
                    let variants = conflict_variants.entry(event_identity).or_default();
                    insert_bounded_variant(variants, history_digest(entry.get()));
                    insert_bounded_variant(variants, history_digest(&candidate));
                    let current = entry.get().clone();
                    entry.insert(canonical_history_event(current, candidate));
                }
            }
        }
    }

    while history.len() > history_limit {
        let Some((dropped, _)) = history.pop_first() else {
            break;
        };
        truncation_watermark = Some(match truncation_watermark {
            Some(current) => current.max(dropped),
            None => dropped,
        });
    }
    if let Some(watermark) = &truncation_watermark {
        conflict_variants.retain(|identity, _| identity > watermark);
    }
    while conflict_variants.len() > history_limit {
        conflict_variants.pop_first();
    }
    let conflicts = conflict_variants
        .into_iter()
        .map(|(identity, variants)| TraceLashlangGraphConflict {
            identity,
            kind: TraceLashlangGraphConflictKind::ConflictingDuplicate,
            variants: variants.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    let history = history.into_values().collect::<Vec<_>>();
    Ok(materialize_graph(
        identity,
        execution_map,
        history,
        conflicts,
        history_limit,
        truncation_watermark,
        status,
    ))
}

fn canonical_language_identity(
    previous: Option<&TraceLashlangGraph>,
    incoming: &[(DateTime<Utc>, TraceLanguageExecution)],
) -> LanguageIdentity {
    if let Some(previous) = previous {
        return LanguageIdentity {
            scope: previous.scope.clone(),
            subject: previous.subject.clone(),
            source_identity: previous.source_identity.clone(),
            module_ref: previous.module_ref.clone(),
            entry_kind: previous.entry_kind.clone(),
            entry_ref: previous.entry_ref.clone(),
            entry_name: previous.entry_name.clone(),
            restate_invocation_id: incoming
                .iter()
                .filter_map(|(_, event)| event.identity.restate_invocation_id.clone())
                .min(),
            generation: incoming
                .iter()
                .find_map(|(_, event)| event.identity.generation)
                .or_else(|| {
                    previous
                        .history
                        .first()
                        .and_then(|item| item.event.identity.generation)
                }),
        };
    }
    incoming
        .iter()
        .map(|(_, event)| event.identity.clone())
        .min_by_key(canonical_bytes)
        .expect("caller established one incoming event")
}

fn materialize_graph(
    identity: LanguageIdentity,
    execution_map: Option<LanguageExecutionMap>,
    history: Vec<TraceLashlangGraphHistoryEvent>,
    conflicts: Vec<TraceLashlangGraphConflict>,
    history_limit: usize,
    truncation_watermark: Option<TraceLashlangEventIdentity>,
    status: LanguageExecutionStatus,
) -> TraceLashlangGraph {
    let mut nodes = BTreeMap::new();
    let mut edges = BTreeMap::new();
    if let Some(map) = &execution_map {
        for node in &map.nodes {
            nodes.insert(
                (node.id.clone(), node.site.kind.clone()),
                TraceLashlangGraphNode::unobserved(
                    node.id.clone(),
                    node.kind.clone(),
                    node.label.clone(),
                    node.label_metadata.clone(),
                ),
            );
        }
        for edge in &map.edges {
            edges.insert(
                edge.id.clone(),
                TraceLashlangGraphEdge {
                    id: edge.id.clone(),
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    label: edge.label.clone(),
                    selection: TraceLashlangEdgeSelection::Unknown,
                },
            );
        }
    }
    let mut occurrences =
        BTreeMap::<(String, String, u64, Option<u32>, Option<u64>), OccurrenceFold>::new();
    let mut children = BTreeMap::new();
    for item in &history {
        match &item.event.payload {
            TraceLanguageExecutionPayload::ExecutionStarted { .. } => {}
            TraceLanguageExecutionPayload::ExecutionFinished { .. } => {}
            TraceLanguageExecutionPayload::NodeStarted {
                node_id,
                node_kind,
                label,
                occurrence,
                ..
            } => {
                nodes
                    .entry((node_id.clone(), node_kind.clone()))
                    .or_insert_with(|| {
                        TraceLashlangGraphNode::unobserved(node_id, node_kind, label, None)
                    });
                occurrences
                    .entry((
                        node_id.clone(),
                        node_kind.clone(),
                        *occurrence,
                        item.event.identity.attempt(),
                        item.event.identity.incarnation(),
                    ))
                    .or_default()
                    .start = Some(item.timestamp);
            }
            TraceLanguageExecutionPayload::NodeCompleted {
                node_id,
                node_kind,
                label,
                occurrence,
                ..
            } => {
                nodes
                    .entry((node_id.clone(), node_kind.clone()))
                    .or_insert_with(|| {
                        TraceLashlangGraphNode::unobserved(node_id, node_kind, label, None)
                    });
                occurrences
                    .entry((
                        node_id.clone(),
                        node_kind.clone(),
                        *occurrence,
                        item.event.identity.attempt(),
                        item.event.identity.incarnation(),
                    ))
                    .or_default()
                    .terminal = Some(OccurrenceTerminal::Completed(item.timestamp));
            }
            TraceLanguageExecutionPayload::NodeFailed {
                node_id,
                node_kind,
                label,
                occurrence,
                error,
                ..
            } => {
                nodes
                    .entry((node_id.clone(), node_kind.clone()))
                    .or_insert_with(|| {
                        TraceLashlangGraphNode::unobserved(node_id, node_kind, label, None)
                    });
                occurrences
                    .entry((
                        node_id.clone(),
                        node_kind.clone(),
                        *occurrence,
                        item.event.identity.attempt(),
                        item.event.identity.incarnation(),
                    ))
                    .or_default()
                    .terminal = Some(OccurrenceTerminal::Failed(item.timestamp, error.clone()));
            }
            TraceLanguageExecutionPayload::BranchSelected {
                node_id,
                occurrence,
                edge_id,
                selected,
            } => {
                if let Some(node) = nodes.get_mut(&(node_id.clone(), "branch".to_string())) {
                    node.branch_selection = Some(*selected);
                    occurrences
                        .entry((
                            node_id.clone(),
                            "branch".to_string(),
                            *occurrence,
                            item.event.identity.attempt(),
                            item.event.identity.incarnation(),
                        ))
                        .or_default()
                        .terminal = Some(OccurrenceTerminal::Completed(item.timestamp));
                }
                if let Some(edge) = edges.get_mut(edge_id) {
                    edge.selection = TraceLashlangEdgeSelection::Selected;
                }
            }
            TraceLanguageExecutionPayload::ChildStarted {
                parent_node_id,
                child,
                ..
            } => {
                let child_graph_key = child.graph_key();
                children.insert(
                    (parent_node_id.clone(), child_graph_key.clone()),
                    TraceLashlangGraphChildLink {
                        parent_graph_key: identity.graph_key(),
                        parent_node_id: parent_node_id.clone(),
                        child_graph_key,
                        child_module_ref: child.module_ref.clone(),
                        child_entry_ref: child.entry_ref.clone(),
                        child_entry_name: child.entry_name.clone(),
                    },
                );
            }
        }
    }
    apply_occurrences(&mut nodes, &occurrences);
    TraceLashlangGraph {
        schema_version: TRACE_SCHEMA_VERSION,
        graph_key: identity.graph_key(),
        scope: identity.scope,
        subject: identity.subject,
        source_identity: identity.source_identity,
        module_ref: identity.module_ref,
        entry_kind: identity.entry_kind,
        entry_ref: identity.entry_ref,
        entry_name: identity.entry_name,
        status,
        completeness: if execution_map.is_some() {
            TraceLashlangGraphCompleteness::Complete
        } else {
            TraceLashlangGraphCompleteness::IncompleteMap
        },
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        children: children.into_values().collect(),
        history_limit,
        truncation_watermark,
        conflicts,
        history,
        execution_map,
    }
}

#[derive(Default)]
struct OccurrenceFold {
    start: Option<DateTime<Utc>>,
    terminal: Option<OccurrenceTerminal>,
}

enum OccurrenceTerminal {
    Completed(DateTime<Utc>),
    Failed(DateTime<Utc>, String),
}

fn apply_occurrences(
    nodes: &mut BTreeMap<(String, String), TraceLashlangGraphNode>,
    occurrences: &BTreeMap<(String, String, u64, Option<u32>, Option<u64>), OccurrenceFold>,
) {
    for ((node_id, node_kind), node) in nodes {
        let matching = occurrences
            .iter()
            .filter(|((id, kind, ..), _)| id == node_id && kind == node_kind)
            .collect::<Vec<_>>();
        node.summary.retained_occurrences = matching.len() as u64;
        node.summary.started_count = matching
            .iter()
            .filter(|(_, occurrence)| occurrence.start.is_some())
            .count() as u64;
        let terminals = matching
            .iter()
            .filter_map(|((_, _, occurrence, _, _), folded)| {
                folded.terminal.as_ref().map(|terminal| {
                    let (status, end) = match terminal {
                        OccurrenceTerminal::Completed(end) => {
                            (LanguageExecutionStatus::Completed, end.to_owned())
                        }
                        OccurrenceTerminal::Failed(end, _) => {
                            (LanguageExecutionStatus::Failed, end.to_owned())
                        }
                    };
                    TraceLashlangNodeTerminalSummary {
                        occurrence: *occurrence,
                        status,
                        end,
                    }
                })
            })
            .collect::<Vec<_>>();
        node.summary.terminal_count = terminals.len() as u64;
        node.summary.first_terminal = terminals.first().cloned();
        node.summary.last_terminal = terminals.last().cloned();
        let Some(((_, _, occurrence, _, _), folded)) = matching.last() else {
            continue;
        };
        node.observation = match &folded.terminal {
            Some(OccurrenceTerminal::Completed(end)) => TraceLashlangNodeObservation::Completed {
                occurrence: *occurrence,
                start: folded.start,
                end: end.to_owned(),
                duration_ms: folded
                    .start
                    .map(|start| end.signed_duration_since(start).num_milliseconds().max(0)),
            },
            Some(OccurrenceTerminal::Failed(end, error)) => TraceLashlangNodeObservation::Failed {
                occurrence: *occurrence,
                start: folded.start,
                end: end.to_owned(),
                duration_ms: folded
                    .start
                    .map(|start| end.signed_duration_since(start).num_milliseconds().max(0)),
                error: error.clone(),
            },
            None => folded
                .start
                .map(|start| TraceLashlangNodeObservation::Running {
                    occurrence: *occurrence,
                    start,
                })
                .unwrap_or_default(),
        };
    }
}

fn event_identity(event: &TraceLanguageExecution) -> TraceLashlangEventIdentity {
    let (node_id, occurrence, transition) = match &event.payload {
        TraceLanguageExecutionPayload::ExecutionStarted { .. } => {
            (None, None, TraceLashlangEventTransition::ExecutionStarted)
        }
        TraceLanguageExecutionPayload::ExecutionFinished { .. } => {
            (None, None, TraceLashlangEventTransition::ExecutionFinished)
        }
        TraceLanguageExecutionPayload::NodeStarted {
            node_id,
            occurrence,
            ..
        } => (
            Some(node_id.clone()),
            Some(*occurrence),
            TraceLashlangEventTransition::NodeStarted,
        ),
        TraceLanguageExecutionPayload::NodeCompleted {
            node_id,
            occurrence,
            ..
        }
        | TraceLanguageExecutionPayload::NodeFailed {
            node_id,
            occurrence,
            ..
        } => (
            Some(node_id.clone()),
            Some(*occurrence),
            TraceLashlangEventTransition::NodeTerminal,
        ),
        TraceLanguageExecutionPayload::BranchSelected {
            node_id,
            occurrence,
            ..
        } => (
            Some(node_id.clone()),
            Some(*occurrence),
            TraceLashlangEventTransition::BranchSelected,
        ),
        TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id,
            occurrence,
            ..
        } => (
            Some(parent_node_id.clone()),
            Some(*occurrence),
            TraceLashlangEventTransition::ChildStarted,
        ),
    };
    TraceLashlangEventIdentity {
        generation: event.identity.generation,
        node_id,
        occurrence,
        transition,
    }
}

fn canonical_value<T: Serialize>(left: T, right: T) -> T {
    if canonical_bytes(&left) <= canonical_bytes(&right) {
        left
    } else {
        right
    }
}

fn canonical_history_event(
    left: TraceLashlangGraphHistoryEvent,
    right: TraceLashlangGraphHistoryEvent,
) -> TraceLashlangGraphHistoryEvent {
    match (&left.event.payload, &right.event.payload) {
        (
            TraceLanguageExecutionPayload::ExecutionFinished {
                status: left_status,
                ..
            },
            TraceLanguageExecutionPayload::ExecutionFinished {
                status: right_status,
                ..
            },
        ) if left_status != right_status => {
            if canonical_execution_status(*left_status, *right_status) == *left_status {
                left
            } else {
                right
            }
        }
        _ => canonical_value(left, right),
    }
}

fn canonical_execution_status(
    left: LanguageExecutionStatus,
    right: LanguageExecutionStatus,
) -> LanguageExecutionStatus {
    match (left.is_terminal(), right.is_terminal()) {
        (true, false) => left,
        (false, true) => right,
        (true, true) => canonical_value(left, right),
        (false, false) => LanguageExecutionStatus::Running,
    }
}

fn canonical_bytes(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).expect("trace graph fold values serialize")
}

fn history_digest(value: &TraceLashlangGraphHistoryEvent) -> String {
    format!("sha256:{:x}", Sha256::digest(canonical_bytes(value)))
}

fn insert_bounded_variant(variants: &mut BTreeSet<String>, variant: String) {
    variants.insert(variant);
    while variants.len() > 2 {
        let middle = variants.iter().nth(1).cloned();
        if let Some(middle) = middle {
            variants.remove(&middle);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use lash_sansio::ProcessId;
    use lash_sansio::SessionId;
    use lash_sansio::TurnId;

    use super::*;
    use crate::{
        TraceBranchSelection, TraceContext, TraceLabelMetadata, TraceLanguageChildExecution,
        TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    };

    fn identity() -> LanguageIdentity {
        LanguageIdentity {
            scope: TraceRuntimeScope {
                session_id: Some(SessionId::from("session-1".to_string())),
                turn_id: Some(TurnId::from("turn-1")),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                address: lash_sansio::EffectAddress::new(
                    lash_sansio::ExecutionScope::turn("session-1", "turn-1"),
                    "exec-replay-1",
                )
                .expect("valid trace test effect address"),
                effect_id: "exec-1".to_string(),
            },
            module_ref: "module-1".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            restate_invocation_id: None,
            generation: None,
        }
    }

    const EFFECT_GRAPH_KEY: &str = r#"effect:{"version":2,"kind":"turn","session_id":"session-1","execution_id":"turn-1"}:"exec-replay-1""#;

    fn record_at(event: TraceLanguageExecution, ms: i64) -> TraceRecord {
        TraceRecord::new_with_timestamp(
            TraceContext::default().for_session("session-1"),
            TraceEvent::LanguageExecution {
                language: "lashlang".to_string(),
                event,
            },
            Utc.timestamp_millis_opt(ms).single().expect("timestamp"),
        )
    }

    fn append_at(store: &TraceLashlangGraphStore, event: TraceLanguageExecution, ms: i64) {
        store
            .append(&record_at(event, ms))
            .expect("append lashlang execution event");
    }

    fn started_event(event_key: &str) -> TraceLanguageExecution {
        TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::ExecutionStarted {
                execution_map: LanguageExecutionMap {
                    nodes: vec![
                        TraceLanguageExecutionMapNode {
                            id: "branch".to_string(),
                            site: lash_sansio::WorkflowExecutionSite::new(
                                "main",
                                [0],
                                "branch",
                                "if ready",
                            ),
                            kind: "branch".to_string(),
                            label: "if ready".to_string(),
                            label_metadata: None,
                        },
                        TraceLanguageExecutionMapNode {
                            id: "then".to_string(),
                            site: lash_sansio::WorkflowExecutionSite::new(
                                "main",
                                [0, 1, 0],
                                "call",
                                "notify()",
                            ),
                            kind: "call".to_string(),
                            label: "notify()".to_string(),
                            label_metadata: None,
                        },
                        TraceLanguageExecutionMapNode {
                            id: "else".to_string(),
                            site: lash_sansio::WorkflowExecutionSite::new(
                                "main",
                                [0, 2, 0],
                                "call",
                                "skip()",
                            ),
                            kind: "call".to_string(),
                            label: "skip()".to_string(),
                            label_metadata: None,
                        },
                    ],
                    // `sequence` is what the producer emits for a control edge
                    // (`WorkflowEdgeKind::Sequence`); the fixture used to
                    // invent `then` / `else` labels so the deleted string
                    // inference had something to match.
                    edges: vec![
                        TraceLanguageExecutionMapEdge {
                            id: "then-edge".to_string(),
                            from: "branch".to_string(),
                            to: "then".to_string(),
                            label: "sequence".to_string(),
                        },
                        TraceLanguageExecutionMapEdge {
                            id: "else-edge".to_string(),
                            from: "branch".to_string(),
                            to: "else".to_string(),
                            label: "sequence".to_string(),
                        },
                    ],
                },
            },
        }
    }

    fn node_started(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
        TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::NodeStarted {
                node_id: "branch".to_string(),
                node_kind: "branch".to_string(),
                label: "if ready".to_string(),
                occurrence,
                call_id: None,
            },
        }
    }

    fn node_completed(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
        TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::NodeCompleted {
                node_id: "branch".to_string(),
                node_kind: "branch".to_string(),
                label: "if ready".to_string(),
                occurrence,
                call_id: None,
            },
        }
    }

    fn node_failed(event_key: &str, occurrence: u64, error: &str) -> TraceLanguageExecution {
        TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::NodeFailed {
                node_id: "branch".to_string(),
                node_kind: "branch".to_string(),
                label: "if ready".to_string(),
                occurrence,
                call_id: None,
                error: error.to_string(),
            },
        }
    }

    fn execution_finished(
        event_key: &str,
        status: LanguageExecutionStatus,
    ) -> TraceLanguageExecution {
        TraceLanguageExecution {
            event_key: event_key.to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::ExecutionFinished {
                status,
                error: None,
            },
        }
    }

    #[test]
    fn graph_store_seeds_static_map_on_execution_start() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, started_event("start"), 1_000);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        assert_eq!(graph.status, LanguageExecutionStatus::Running);
        assert_eq!(
            graph.nodes[0].observation,
            TraceLashlangNodeObservation::Unobserved
        );
        assert_eq!(
            graph.edges[0].selection,
            TraceLashlangEdgeSelection::Unknown
        );
    }

    #[test]
    fn graph_store_keeps_distinct_site_kinds_for_one_structural_node() {
        let store = TraceLashlangGraphStore::default();
        let mut event = started_event("start");
        if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map } =
            &mut event.payload
        {
            execution_map.nodes.push(TraceLanguageExecutionMapNode {
                id: "branch".to_string(),
                site: lash_sansio::WorkflowExecutionSite::new(
                    "main",
                    [0],
                    "resource_operation",
                    "condition",
                ),
                kind: "resource_operation".to_string(),
                label: "condition".to_string(),
                label_metadata: None,
            });
        }

        append_at(&store, event, 1_000);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let sites = graph
            .nodes
            .iter()
            .filter(|node| node.id == "branch")
            .map(|node| node.kind.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(sites, BTreeSet::from(["branch", "resource_operation"]));
    }

    /// A TypeScript session's executions reduce into the same projection.
    ///
    /// A record carries the dialect of the source that ran. Ignoring any
    /// dialect other than Lashlang would empty every TypeScript session's
    /// execution view, although both dialects run on the same VM.
    #[test]
    fn graph_store_reduces_every_dialects_execution_events() {
        let store = TraceLashlangGraphStore::default();
        store
            .append(&TraceRecord::new(
                TraceContext::default().for_session("session-1"),
                TraceEvent::LanguageExecution {
                    language: "typescript".to_string(),
                    event: started_event("start"),
                },
            ))
            .expect("append a TypeScript execution event");

        assert!(!store.graphs().is_empty());
    }

    #[test]
    fn graph_store_preserves_static_label_metadata() {
        let store = TraceLashlangGraphStore::default();
        let mut event = started_event("start");
        if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map, .. } =
            &mut event.payload
        {
            execution_map.nodes[0].label_metadata = Some(TraceLabelMetadata {
                title: "Choose path".to_string(),
                description: Some("Branch detail".to_string()),
            });
        }

        append_at(&store, event, 1_000);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        assert_eq!(
            graph.nodes[0].label_metadata,
            Some(TraceLabelMetadata {
                title: "Choose path".to_string(),
                description: Some("Branch detail".to_string()),
            })
        );
    }

    #[test]
    fn graph_store_deduplicates_by_logical_identity_not_event_key() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("same-key", 1), 1_000);
        append_at(&store, node_completed("same-key", 1), 1_250);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        assert!(matches!(
            graph.nodes[0].observation,
            TraceLashlangNodeObservation::Completed { occurrence: 1, .. }
        ));
    }

    #[test]
    fn graph_store_updates_completed_node_duration() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("start-node", 1), 1_000);
        append_at(&store, node_completed("complete-node", 1), 1_750);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let node = &graph.nodes[0];
        assert!(matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed {
                occurrence: 1,
                duration_ms: Some(750),
                ..
            }
        ));
    }

    #[test]
    fn graph_store_reentered_node_resets_error_and_measures_current_occurrence() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("first-start", 1), 1_000);
        append_at(
            &store,
            node_failed("first-failure", 1, "first failed"),
            1_250,
        );
        append_at(&store, node_started("second-start", 2), 2_000);
        append_at(&store, node_completed("second-complete", 2), 2_400);

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let node = &graph.nodes[0];
        assert!(matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed {
                occurrence: 2,
                duration_ms: Some(400),
                ..
            }
        ));
        let serialized = serde_json::to_value(node).expect("serialize completed node");
        assert_eq!(serialized.get("error"), None);
    }

    #[test]
    fn graph_store_terminal_event_for_different_occurrence_uses_terminal_timestamp() {
        for terminal in [
            node_completed("complete-node", 2),
            node_failed("fail-node", 2, "failed"),
        ] {
            let store = TraceLashlangGraphStore::default();
            append_at(&store, node_started("start-node", 1), 1_000);
            append_at(&store, terminal, 1_750);

            let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
            let (occurrence, start, _end, duration_ms) = match &graph.nodes[0].observation {
                TraceLashlangNodeObservation::Completed {
                    occurrence,
                    start,
                    end,
                    duration_ms,
                }
                | TraceLashlangNodeObservation::Failed {
                    occurrence,
                    start,
                    end,
                    duration_ms,
                    ..
                } => (occurrence, start, end, duration_ms),
                observation => panic!("node was not terminal: {observation:#?}"),
            };
            assert_eq!(*occurrence, 2);
            assert_eq!(*start, None);
            assert_eq!(*duration_ms, None);
        }
    }

    #[test]
    fn graph_store_branch_selection_completes_unstarted_node_with_zero_duration() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, started_event("start"), 1_000);
        append_at(
            &store,
            TraceLanguageExecution {
                event_key: "branch".to_string(),
                identity: identity(),
                payload: TraceLanguageExecutionPayload::BranchSelected {
                    node_id: "branch".to_string(),
                    occurrence: 1,
                    edge_id: "then-edge".to_string(),
                    selected: TraceBranchSelection::Then,
                },
            },
            1_100,
        );

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let node = graph
            .nodes
            .iter()
            .find(|node| node.id == "branch")
            .expect("branch node");
        let TraceLashlangNodeObservation::Completed {
            occurrence,
            start,
            end: _,
            duration_ms,
        } = &node.observation
        else {
            panic!("branch node was not completed: {node:#?}");
        };
        assert_eq!(*occurrence, 1);
        assert_eq!(*start, None);
        assert_eq!(*duration_ms, None);

        let serialized = serde_json::to_value(node).expect("serialize branch node");
        assert_eq!(serialized["status"], "completed");
        assert!(serialized.get("duration_ms").is_none());
        assert!(serialized.get("observation").is_none());
    }

    #[test]
    fn graph_store_records_the_typed_branch_arm_and_marks_the_selected_edge() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, started_event("start"), 1_000);
        append_at(
            &store,
            TraceLanguageExecution {
                event_key: "branch".to_string(),
                identity: identity(),
                payload: TraceLanguageExecutionPayload::BranchSelected {
                    node_id: "branch".to_string(),
                    occurrence: 1,
                    edge_id: "then-edge".to_string(),
                    selected: TraceBranchSelection::Then,
                },
            },
            1_100,
        );

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        let branch = graph
            .nodes
            .iter()
            .find(|node| node.id == "branch")
            .expect("branch node");
        let serialized = serde_json::to_value(branch).expect("serialize branch node");
        assert_eq!(serialized["branch_selection"], serde_json::json!("then"));
        // The selection rides beside a flattened observation, so pin the
        // round trip rather than only the encode.
        assert_eq!(
            &serde_json::from_value::<TraceLashlangGraphNode>(serialized)
                .expect("decode branch node"),
            branch,
        );
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "then-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Selected)
        );
        // The sibling edge stays unmarked: no live producer labels a branch
        // arm, so the reducer cannot tell an unselected arm from an ordinary
        // sequencing or data-dependency edge leaving the same node (ADR 0037).
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "else-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Unknown)
        );
    }

    #[test]
    fn graph_store_records_child_links() {
        let store = TraceLashlangGraphStore::default();

        append_at(
            &store,
            TraceLanguageExecution {
                event_key: "child".to_string(),
                identity: identity(),
                payload: TraceLanguageExecutionPayload::ChildStarted {
                    parent_node_id: "spawn".to_string(),
                    occurrence: 1,
                    child: TraceLanguageChildExecution {
                        scope: TraceRuntimeScope::new("session-1"),
                        subject: TraceRuntimeSubject::Process {
                            process_id: ProcessId::from("process:child".to_string()),
                        },
                        module_ref: Some("module-1".to_string()),
                        entry_ref: Some("process:0".to_string()),
                        entry_name: Some("child".to_string()),
                    },
                },
            },
            1_000,
        );

        let graph = store.graph(EFFECT_GRAPH_KEY).expect("graph");
        assert_eq!(graph.children[0].parent_node_id, "spawn");
        assert_eq!(graph.children[0].child_graph_key, "process:process:child");
        assert_eq!(graph.children[0].child_entry_name.as_deref(), Some("child"));
    }

    #[test]
    fn regression_fold_is_independent_of_the_three_temporal_arrival_orders() {
        let orders = [
            vec![
                (started_event("seed"), 900),
                (node_started("node-start", 1), 1_000),
                (node_completed("node-complete", 1), 1_250),
            ],
            vec![
                (node_started("node-start", 1), 1_000),
                (node_completed("node-complete", 1), 1_250),
                (started_event("seed"), 900),
            ],
            vec![
                (node_completed("node-complete", 1), 1_250),
                (node_started("node-start", 1), 1_000),
                (started_event("seed"), 900),
            ],
        ];
        let snapshots = orders.map(|events| {
            let store = TraceLashlangGraphStore::default();
            for (event, timestamp) in events {
                append_at(&store, event, timestamp);
            }
            serde_json::to_vec(&store.graph(EFFECT_GRAPH_KEY).expect("graph"))
                .expect("serialize graph")
        });

        assert_eq!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[0], snapshots[2]);
    }

    #[test]
    fn missing_and_late_seed_are_explicit_and_do_not_change_observations() {
        let events = [
            record_at(node_started("start", 1), 1_000),
            record_at(node_completed("complete", 1), 1_250),
        ];
        let missing = TraceLashlangGraphStore::fold(None, &events).expect("fold without map");
        assert_eq!(
            missing.completeness,
            TraceLashlangGraphCompleteness::IncompleteMap
        );

        let late =
            TraceLashlangGraphStore::fold(Some(&missing), &[record_at(started_event("seed"), 900)])
                .expect("late seed");
        assert_eq!(late.completeness, TraceLashlangGraphCompleteness::Complete);
        assert!(matches!(
            late.nodes[0].observation,
            TraceLashlangNodeObservation::Completed {
                duration_ms: Some(250),
                ..
            }
        ));
    }

    #[test]
    fn identical_duplicate_is_a_noop_and_conflicting_duplicate_is_typed() {
        let start = record_at(node_started("publisher-a", 1), 1_000);
        let mut duplicate = start.clone();
        let TraceEvent::LanguageExecution { event, .. } = &mut duplicate.event else {
            unreachable!()
        };
        event.event_key = "publisher-b".to_string();
        let deduplicated =
            TraceLashlangGraphStore::fold(None, &[start.clone(), duplicate]).expect("duplicate");
        assert_eq!(deduplicated.history.len(), 1);
        assert!(deduplicated.conflicts.is_empty());

        let conflict = TraceLashlangGraphStore::fold(
            None,
            &[start, record_at(node_started("publisher-c", 1), 1_001)],
        )
        .expect("conflict");
        assert_eq!(conflict.conflicts.len(), 1);
        assert_eq!(
            conflict.conflicts[0].kind,
            TraceLashlangGraphConflictKind::ConflictingDuplicate
        );
        assert_eq!(conflict.conflicts[0].variants.len(), 2);
    }

    #[test]
    fn terminal_occurrence_never_downgrades_and_next_occurrence_is_visible() {
        let graph = TraceLashlangGraphStore::fold(
            None,
            &[
                record_at(node_completed("complete-1", 1), 1_250),
                record_at(node_started("late-start-1", 1), 1_000),
                record_at(node_started("start-2", 2), 2_000),
            ],
        )
        .expect("fold occurrences");
        assert!(matches!(
            graph.nodes[0].observation,
            TraceLashlangNodeObservation::Running { occurrence: 2, .. }
        ));
        assert_eq!(graph.nodes[0].summary.retained_occurrences, 2);
        assert_eq!(graph.nodes[0].summary.terminal_count, 1);
    }

    #[test]
    fn terminal_execution_status_beats_a_conflicting_running_status() {
        let events = [
            record_at(
                execution_finished("terminal", LanguageExecutionStatus::Completed),
                2_000,
            ),
            record_at(
                execution_finished("running", LanguageExecutionStatus::Running),
                1_000,
            ),
        ];
        for ordered in [events.clone(), [events[1].clone(), events[0].clone()]] {
            let graph = TraceLashlangGraphStore::fold(None, &ordered).expect("fold statuses");
            assert_eq!(graph.status, LanguageExecutionStatus::Completed);
            assert_eq!(graph.conflicts.len(), 1);
        }
    }

    #[test]
    fn branch_selection_and_terminal_observation_are_distinct_facts() {
        let selection = TraceLanguageExecution {
            event_key: "branch".to_string(),
            identity: identity(),
            payload: TraceLanguageExecutionPayload::BranchSelected {
                node_id: "branch".to_string(),
                occurrence: 1,
                edge_id: "then-edge".to_string(),
                selected: TraceBranchSelection::Then,
            },
        };
        let graph = TraceLashlangGraphStore::fold(
            None,
            &[
                record_at(started_event("seed"), 900),
                record_at(selection, 1_100),
                record_at(node_completed("complete", 1), 1_200),
            ],
        )
        .expect("fold branch facts");
        assert!(graph.conflicts.is_empty());
        assert_eq!(
            graph.nodes[0].branch_selection,
            Some(TraceBranchSelection::Then)
        );
        assert!(graph.nodes[0].observation.is_terminal());
    }

    #[test]
    fn every_permutation_and_incremental_partition_is_byte_identical() {
        let events = [
            record_at(started_event("seed"), 900),
            record_at(node_started("start", 1), 1_000),
            record_at(node_completed("complete", 1), 1_250),
        ];
        let permutations = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let expected = TraceLashlangGraphStore::fold(None, &events).expect("batch");
        let expected_bytes = serde_json::to_vec(&expected).expect("serialize batch");
        for permutation in permutations {
            let batch = permutation.map(|index| events[index].clone());
            let actual = TraceLashlangGraphStore::fold(None, &batch).expect("permuted batch");
            assert_eq!(
                serde_json::to_vec(&actual).expect("serialize permutation"),
                expected_bytes
            );
        }
        for split in 1..events.len() {
            let first =
                TraceLashlangGraphStore::fold(None, &events[..split]).expect("first partition");
            let second = TraceLashlangGraphStore::fold(Some(&first), &events[split..])
                .expect("second partition");
            assert_eq!(
                serde_json::to_vec(&second).expect("serialize incremental"),
                expected_bytes
            );
        }
    }

    #[test]
    fn history_is_bounded_with_a_canonical_truncation_watermark() {
        let events = [
            record_at(node_started("one", 1), 1_000),
            record_at(node_started("two", 2), 2_000),
            record_at(node_started("three", 3), 3_000),
        ];
        let batch = TraceLashlangGraphStore::fold_with_history_limit(None, &events, 2)
            .expect("bounded batch");
        assert_eq!(batch.history.len(), 2);
        assert_eq!(
            batch
                .truncation_watermark
                .as_ref()
                .and_then(|watermark| watermark.occurrence),
            Some(1)
        );
        let first = TraceLashlangGraphStore::fold_with_history_limit(None, &events[1..], 2)
            .expect("high identities first");
        let incremental =
            TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &events[..1], 2)
                .expect("late truncated identity");
        assert_eq!(
            serde_json::to_vec(&incremental).expect("serialize incremental"),
            serde_json::to_vec(&batch).expect("serialize batch")
        );
    }

    #[test]
    fn late_static_map_below_the_watermark_matches_batch_folding() {
        let seed = record_at(started_event("seed"), 900);
        let node_events = [
            record_at(node_started("one", 1), 1_000),
            record_at(node_started("two", 2), 2_000),
        ];
        let batch = TraceLashlangGraphStore::fold_with_history_limit(
            None,
            &[seed.clone(), node_events[0].clone(), node_events[1].clone()],
            1,
        )
        .expect("bounded batch");
        let first = TraceLashlangGraphStore::fold_with_history_limit(None, &node_events, 1)
            .expect("node events first");
        let incremental =
            TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &[seed], 1)
                .expect("late static map");

        assert_eq!(incremental, batch);
        assert_eq!(
            incremental.completeness,
            TraceLashlangGraphCompleteness::Complete
        );
    }

    #[test]
    fn terminal_graph_status_survives_history_truncation() {
        let finished = record_at(
            execution_finished("finished", LanguageExecutionStatus::Completed),
            3_000,
        );
        let node_events = [
            record_at(node_started("one", 1), 1_000),
            record_at(node_started("two", 2), 2_000),
        ];
        let batch = TraceLashlangGraphStore::fold_with_history_limit(
            None,
            &[
                finished.clone(),
                node_events[0].clone(),
                node_events[1].clone(),
            ],
            1,
        )
        .expect("bounded batch");
        let first = TraceLashlangGraphStore::fold_with_history_limit(None, &node_events, 1)
            .expect("node events first");
        let incremental =
            TraceLashlangGraphStore::fold_with_history_limit(Some(&first), &[finished], 1)
                .expect("late terminal status");

        assert_eq!(incremental, batch);
        assert_eq!(incremental.status, LanguageExecutionStatus::Completed);
    }

    #[test]
    fn terminal_classification_is_exhaustive() {
        assert!(!LanguageExecutionStatus::Running.is_terminal());
        assert!(LanguageExecutionStatus::Completed.is_terminal());
        assert!(LanguageExecutionStatus::Failed.is_terminal());
        assert!(LanguageExecutionStatus::Cancelled.is_terminal());

        assert!(!TraceLashlangNodeObservation::Unobserved.is_terminal());
        assert!(
            !TraceLashlangNodeObservation::Running {
                occurrence: 1,
                start: Utc.timestamp_millis_opt(1).single().expect("timestamp"),
            }
            .is_terminal()
        );
        assert!(
            TraceLashlangNodeObservation::Completed {
                occurrence: 1,
                start: None,
                end: Utc.timestamp_millis_opt(2).single().expect("timestamp"),
                duration_ms: None,
            }
            .is_terminal()
        );
        assert!(
            TraceLashlangNodeObservation::Failed {
                occurrence: 1,
                start: None,
                end: Utc.timestamp_millis_opt(2).single().expect("timestamp"),
                duration_ms: None,
                error: "failed".to_string(),
            }
            .is_terminal()
        );
    }

    #[test]
    fn graph_decode_checks_version_before_shape_and_tolerates_additive_fields() {
        let graph =
            TraceLashlangGraphStore::fold(None, &[record_at(node_started("start", 1), 1_000)])
                .expect("graph");
        let mut value = serde_json::to_value(&graph).expect("encode graph");
        value["future_field"] = serde_json::json!(true);
        assert_eq!(
            serde_json::from_value::<TraceLashlangGraph>(value.clone()).expect("additive field"),
            graph
        );
        value["schema_version"] = serde_json::json!(TRACE_SCHEMA_VERSION - 1);
        value["completeness"] = serde_json::json!("future_variant");
        let error = serde_json::from_value::<TraceLashlangGraph>(value)
            .expect_err("predecessor must be refused before shape");
        assert!(
            error
                .to_string()
                .contains("unsupported trace schema version")
        );
    }

    #[test]
    fn graph_decode_refuses_unknown_closed_variant_at_the_current_version() {
        let graph =
            TraceLashlangGraphStore::fold(None, &[record_at(node_started("start", 1), 1_000)])
                .expect("graph");
        let value = serde_json::to_value(graph).expect("encode graph");
        for (field, changed) in [
            ("completeness", {
                let mut changed = value.clone();
                changed["completeness"] = serde_json::json!("future_variant");
                changed
            }),
            ("graph status", {
                let mut changed = value.clone();
                changed["status"] = serde_json::json!("future_variant");
                changed
            }),
            ("node observation status", {
                let mut changed = value.clone();
                changed["nodes"][0]["status"] = serde_json::json!("future_variant");
                changed
            }),
        ] {
            let error = serde_json::from_value::<TraceLashlangGraph>(changed)
                .expect_err(&format!("unknown {field} variant must be refused"));
            assert!(
                error.to_string().contains("future_variant"),
                "unexpected {field} error: {error}"
            );
        }
    }
}
