use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use lash_sansio::sync::MutexExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    TRACE_SCHEMA_VERSION, TraceEvent, TraceLanguageExecution,
    TraceLanguageExecutionIdentity as LanguageIdentity,
    TraceLanguageExecutionMap as LanguageExecutionMap, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceRecord, TraceSink,
    TraceSinkError,
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
    #[expect(
        clippy::expect_used,
        reason = "the event graph key is selected from this same single-record fold input"
    )]
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
        resolve_child_graph_keys(&mut graphs);
        Ok(())
    }
}

fn resolve_child_graph_keys(graphs: &mut BTreeMap<String, TraceLashlangGraph>) {
    let process_graphs = graphs
        .values()
        .filter_map(|graph| {
            let crate::TraceRuntimeSubject::Process { process_id } = &graph.subject else {
                return None;
            };
            let generation = graph
                .history
                .first()
                .and_then(|item| item.event.identity.generation)?;
            Some((
                process_id.clone(),
                generation.incarnation(),
                generation.attempt(),
                graph.graph_key.clone(),
            ))
        })
        .collect::<Vec<_>>();
    for graph in graphs.values_mut() {
        for child in &mut graph.children {
            let matches = process_graphs
                .iter()
                .filter(|(process_id, incarnation, attempt, _)| {
                    process_id == &child.child_process_id
                        && *incarnation == child.child_incarnation
                        && child
                            .child_attempt
                            .is_none_or(|expected| expected == *attempt)
                })
                .collect::<Vec<_>>();
            if matches.len() == 1 {
                child.child_graph_key = Some(matches[0].3.clone());
            }
        }
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
        .or_else(|| previous.and_then(|graph| graph.history.first().map(|item| &item.event)));
    let graph_key = first_event
        .map(|event| event.identity.graph_key())
        .or_else(|| previous.map(|graph| graph.graph_key.clone()))
        .ok_or(TraceLashlangGraphFoldError::NoLanguageExecutionEvents)?;
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
    let mut node_retention = previous
        .map(|graph| {
            graph
                .node_retention
                .iter()
                .cloned()
                .map(|retention| (retention.node_id.clone(), retention))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

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
        if let Some((node_id, occurrence)) = node_occurrence(&event_identity)
            && node_retention
                .get(node_id)
                .is_some_and(|retention| occurrence <= retention.truncation_watermark)
        {
            merge_late_retained_event(
                node_retention
                    .get_mut(node_id)
                    .expect("retention was checked above"),
                timestamp,
                &event,
            );
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

    let node_ids = history
        .keys()
        .filter_map(|identity| identity.node_id.clone())
        .collect::<BTreeSet<_>>();
    for node_id in node_ids {
        loop {
            let occurrences = history
                .keys()
                .filter_map(|identity| {
                    (identity.node_id.as_deref() == Some(node_id.as_str()))
                        .then_some(identity.occurrence)
                        .flatten()
                })
                .collect::<BTreeSet<_>>();
            if occurrences.len() <= history_limit {
                break;
            }
            let occurrence = *occurrences.first().expect("non-empty occurrence set");
            let dropped_keys = history
                .keys()
                .filter(|identity| {
                    identity.node_id.as_deref() == Some(node_id.as_str())
                        && identity.occurrence == Some(occurrence)
                })
                .cloned()
                .collect::<Vec<_>>();
            let dropped = dropped_keys
                .iter()
                .filter_map(|identity| history.remove(identity))
                .collect::<Vec<_>>();
            let prior = node_retention.remove(&node_id);
            node_retention.insert(
                node_id.clone(),
                merge_node_retention(
                    prior,
                    &node_id,
                    occurrence,
                    &dropped,
                    execution_map.as_ref(),
                    &identity,
                ),
            );
        }
    }
    conflict_variants.retain(|identity, _| {
        node_occurrence(identity).is_none_or(|(node_id, occurrence)| {
            node_retention
                .get(node_id)
                .is_none_or(|retention| occurrence > retention.truncation_watermark)
        })
    });
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
        node_retention.into_values().collect(),
        status,
    ))
}

#[expect(
    clippy::expect_used,
    reason = "the caller rejects an empty fold before selecting the canonical identity"
)]
fn canonical_language_identity(
    previous: Option<&TraceLashlangGraph>,
    incoming: &[(DateTime<Utc>, TraceLanguageExecution)],
) -> LanguageIdentity {
    previous
        .map(|graph| LanguageIdentity {
            scope: graph.scope.clone(),
            subject: graph.subject.clone(),
            source_identity: graph.source_identity.clone(),
            module_ref: graph.module_ref.clone(),
            entry_kind: graph.entry_kind.clone(),
            entry_ref: graph.entry_ref.clone(),
            entry_name: graph.entry_name.clone(),
            restate_invocation_id: None,
            generation: graph
                .history
                .first()
                .and_then(|item| item.event.identity.generation),
        })
        .into_iter()
        .chain(incoming.iter().map(|(_, event)| event.identity.clone()))
        .reduce(canonical_identity_fields)
        .expect("caller established a previous snapshot or incoming event")
}

fn canonical_identity_fields(left: LanguageIdentity, right: LanguageIdentity) -> LanguageIdentity {
    LanguageIdentity {
        scope: canonical_value(left.scope, right.scope),
        subject: canonical_value(left.subject, right.subject),
        source_identity: left.source_identity.min(right.source_identity),
        module_ref: left.module_ref.min(right.module_ref),
        entry_kind: left.entry_kind.min(right.entry_kind),
        entry_ref: canonical_value(left.entry_ref, right.entry_ref),
        entry_name: left.entry_name.min(right.entry_name),
        restate_invocation_id: canonical_value(
            left.restate_invocation_id,
            right.restate_invocation_id,
        ),
        generation: canonical_value(left.generation, right.generation),
    }
}

fn materialize_graph(
    identity: LanguageIdentity,
    execution_map: Option<LanguageExecutionMap>,
    history: Vec<TraceLashlangGraphHistoryEvent>,
    conflicts: Vec<TraceLashlangGraphConflict>,
    history_limit: usize,
    mut node_retention: Vec<TraceLashlangNodeRetention>,
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
        for retention in &mut node_retention {
            if let Some(static_node) = map.nodes.iter().find(|node| node.id == retention.node_id) {
                retention.node.kind = static_node.kind.clone();
                retention.node.label = static_node.label.clone();
                retention.node.label_metadata = static_node.label_metadata.clone();
                if let Some(archived) = &mut retention.archived_node {
                    archived.kind = static_node.kind.clone();
                    archived.label = static_node.label.clone();
                    archived.label_metadata = static_node.label_metadata.clone();
                }
            }
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
    let mut occurrences = BTreeMap::<OccurrenceKey, OccurrenceFold>::new();
    let mut children = BTreeMap::new();
    for retention in &node_retention {
        let key = (retention.node.id.clone(), retention.node.kind.clone());
        if let Some(node) = nodes.get_mut(&key) {
            node.branch_selection = retention.node.branch_selection;
            node.observation = retention.node.observation.clone();
            node.summary = retention.node.summary.clone();
        } else {
            nodes.insert(key, retention.node.clone());
        }
        for edge_id in &retention.selected_edge_ids {
            if let Some(edge) = edges.get_mut(edge_id) {
                edge.selection = TraceLashlangEdgeSelection::Selected;
            }
        }
        for child in &retention.children {
            children.insert(child_link_key(child), child.clone());
        }
    }
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
                    .explicit_terminal = Some(OccurrenceTerminal::Completed(item.timestamp));
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
                    .explicit_terminal =
                    Some(OccurrenceTerminal::Failed(item.timestamp, error.clone()));
            }
            TraceLanguageExecutionPayload::BranchSelected {
                node_id,
                occurrence,
                edge_id,
                selected,
            } => {
                let node = nodes
                    .entry((node_id.clone(), "branch".to_string()))
                    .or_insert_with(|| {
                        TraceLashlangGraphNode::unobserved(node_id, "branch", node_id, None)
                    });
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
                    .provisional_terminal = Some(OccurrenceTerminal::Completed(item.timestamp));
                if let Some(edge) = edges.get_mut(edge_id) {
                    edge.selection = TraceLashlangEdgeSelection::Selected;
                }
            }
            TraceLanguageExecutionPayload::ChildStarted {
                parent_node_id,
                child,
                ..
            } => {
                nodes
                    .entry((parent_node_id.clone(), "call".to_string()))
                    .or_insert_with(|| {
                        TraceLashlangGraphNode::unobserved(
                            parent_node_id,
                            "call",
                            parent_node_id,
                            None,
                        )
                    });
                let link = TraceLashlangGraphChildLink {
                    parent_graph_key: identity.graph_key(),
                    parent_node_id: parent_node_id.clone(),
                    child_graph_key: child.graph_key(),
                    child_process_id: child.process_id.clone(),
                    child_incarnation: child.incarnation,
                    child_attempt: child.attempt,
                    child_module_ref: child.module_ref.clone(),
                    child_entry_ref: child.entry_ref.clone(),
                    child_entry_name: child.entry_name.clone(),
                };
                children.insert(child_link_key(&link), link);
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
        node_retention,
        conflicts,
        history,
        execution_map,
    }
}

#[derive(Default)]
struct OccurrenceFold {
    start: Option<DateTime<Utc>>,
    explicit_terminal: Option<OccurrenceTerminal>,
    provisional_terminal: Option<OccurrenceTerminal>,
}

type OccurrenceKey = (String, String, u64, Option<u32>, Option<u64>);

enum OccurrenceTerminal {
    Completed(DateTime<Utc>),
    Failed(DateTime<Utc>, String),
}

fn apply_occurrences(
    nodes: &mut BTreeMap<(String, String), TraceLashlangGraphNode>,
    occurrences: &BTreeMap<OccurrenceKey, OccurrenceFold>,
) {
    for ((node_id, node_kind), node) in nodes {
        let matching = occurrences
            .iter()
            .filter(|((id, kind, ..), _)| id == node_id && kind == node_kind)
            .collect::<Vec<_>>();
        node.summary.retained_occurrences += matching.len() as u64;
        node.summary.started_count += matching
            .iter()
            .filter(|(_, occurrence)| occurrence.start.is_some())
            .count() as u64;
        let terminals = matching
            .iter()
            .filter_map(|((_, _, occurrence, _, _), folded)| {
                folded_terminal(folded).map(|terminal| {
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
        node.summary.terminal_count += terminals.len() as u64;
        node.summary.first_terminal = [
            node.summary.first_terminal.clone(),
            terminals.first().cloned(),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|terminal| terminal.occurrence);
        node.summary.last_terminal = [
            node.summary.last_terminal.clone(),
            terminals.last().cloned(),
        ]
        .into_iter()
        .flatten()
        .max_by_key(|terminal| terminal.occurrence);
        let Some(((_, _, occurrence, _, _), folded)) = matching.last() else {
            continue;
        };
        node.observation = match folded_terminal(folded) {
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

fn folded_terminal(folded: &OccurrenceFold) -> Option<&OccurrenceTerminal> {
    folded
        .explicit_terminal
        .as_ref()
        .or(folded.provisional_terminal.as_ref())
}

fn node_occurrence(identity: &TraceLashlangEventIdentity) -> Option<(&str, u64)> {
    Some((identity.node_id.as_deref()?, identity.occurrence?))
}

fn child_link_key(
    child: &TraceLashlangGraphChildLink,
) -> (String, lash_sansio::ProcessId, u64, Option<u32>) {
    (
        child.parent_node_id.clone(),
        child.child_process_id.clone(),
        child.child_incarnation,
        child.child_attempt,
    )
}

fn merge_node_retention(
    prior: Option<TraceLashlangNodeRetention>,
    node_id: &str,
    occurrence: u64,
    dropped: &[TraceLashlangGraphHistoryEvent],
    execution_map: Option<&LanguageExecutionMap>,
    identity: &LanguageIdentity,
) -> TraceLashlangNodeRetention {
    let archived_node = prior
        .as_ref()
        .map(|retention| Box::new(retention.node.clone()));
    let prior = prior.into_iter().collect::<Vec<_>>();
    let graph = materialize_graph(
        identity.clone(),
        execution_map.cloned(),
        dropped.to_vec(),
        Vec::new(),
        1,
        prior,
        LanguageExecutionStatus::Running,
    );
    let node = graph
        .nodes
        .into_iter()
        .find(|node| node.id == node_id)
        .expect("an evicted node event materializes its node");
    TraceLashlangNodeRetention {
        node_id: node_id.to_string(),
        truncation_watermark: occurrence,
        archived_node,
        watermark_history: dropped.to_vec(),
        node,
        selected_edge_ids: graph
            .edges
            .into_iter()
            .filter(|edge| edge.selection == TraceLashlangEdgeSelection::Selected)
            .map(|edge| edge.id)
            .collect(),
        children: graph
            .children
            .into_iter()
            .filter(|child| child.parent_node_id == node_id)
            .collect(),
    }
}

fn merge_late_retained_event(
    retention: &mut TraceLashlangNodeRetention,
    timestamp: DateTime<Utc>,
    event: &TraceLanguageExecution,
) {
    if event_identity(event).occurrence == Some(retention.truncation_watermark) {
        let mut canonical_event = event.clone();
        canonical_event.event_key.clear();
        let identity = event_identity(&canonical_event);
        let candidate = TraceLashlangGraphHistoryEvent {
            identity: identity.clone(),
            timestamp,
            event: canonical_event,
        };
        if let Some(index) = retention
            .watermark_history
            .iter()
            .position(|current| current.identity == identity)
        {
            let current = retention.watermark_history[index].clone();
            retention.watermark_history[index] = canonical_history_event(current, candidate);
        } else {
            retention.watermark_history.push(candidate);
            retention
                .watermark_history
                .sort_by(|left, right| left.identity.cmp(&right.identity));
        }
        let identity = retention.watermark_history[0].event.identity.clone();
        let watermark_graph = materialize_graph(
            identity,
            None,
            retention.watermark_history.clone(),
            Vec::new(),
            1,
            Vec::new(),
            LanguageExecutionStatus::Running,
        );
        let mut watermark_node = watermark_graph
            .nodes
            .into_iter()
            .find(|node| node.id == retention.node_id)
            .expect("watermark history materializes its node");
        if let Some(archived) = &retention.archived_node {
            merge_node_summary(&mut watermark_node.summary, &archived.summary);
        }
        retention.node = watermark_node;
        for edge in watermark_graph.edges {
            if edge.selection == TraceLashlangEdgeSelection::Selected
                && !retention.selected_edge_ids.contains(&edge.id)
            {
                retention.selected_edge_ids.push(edge.id);
            }
        }
        retention.selected_edge_ids.sort();
        for child in watermark_graph.children {
            if !retention
                .children
                .iter()
                .any(|current| child_link_key(current) == child_link_key(&child))
            {
                retention.children.push(child);
            }
        }
        retention.children.sort_by_key(child_link_key);
        return;
    }
    match &event.payload {
        TraceLanguageExecutionPayload::NodeStarted { occurrence, .. } => {
            let start = match &retention.node.observation {
                TraceLashlangNodeObservation::Running {
                    occurrence: retained,
                    start,
                } if retained == occurrence => Some((*start).min(timestamp)),
                TraceLashlangNodeObservation::Completed {
                    occurrence: retained,
                    start,
                    ..
                }
                | TraceLashlangNodeObservation::Failed {
                    occurrence: retained,
                    start,
                    ..
                } if retained == occurrence => {
                    Some(start.map_or(timestamp, |start| start.min(timestamp)))
                }
                _ => None,
            };
            if let Some(start) = start {
                let previously_missing = match &retention.node.observation {
                    TraceLashlangNodeObservation::Completed { start, .. }
                    | TraceLashlangNodeObservation::Failed { start, .. } => start.is_none(),
                    _ => false,
                };
                retention.node.observation = match &retention.node.observation {
                    TraceLashlangNodeObservation::Running { occurrence, .. } => {
                        TraceLashlangNodeObservation::Running {
                            occurrence: *occurrence,
                            start,
                        }
                    }
                    TraceLashlangNodeObservation::Completed {
                        occurrence, end, ..
                    } => TraceLashlangNodeObservation::Completed {
                        occurrence: *occurrence,
                        start: Some(start),
                        end: *end,
                        duration_ms: Some(
                            end.signed_duration_since(start).num_milliseconds().max(0),
                        ),
                    },
                    TraceLashlangNodeObservation::Failed {
                        occurrence,
                        end,
                        error,
                        ..
                    } => TraceLashlangNodeObservation::Failed {
                        occurrence: *occurrence,
                        start: Some(start),
                        end: *end,
                        duration_ms: Some(
                            end.signed_duration_since(start).num_milliseconds().max(0),
                        ),
                        error: error.clone(),
                    },
                    TraceLashlangNodeObservation::Unobserved => return,
                };
                if previously_missing {
                    retention.node.summary.started_count += 1;
                }
            }
        }
        TraceLanguageExecutionPayload::NodeCompleted { occurrence, .. }
        | TraceLanguageExecutionPayload::NodeFailed { occurrence, .. } => {
            if retention.node.observation.is_terminal() {
                return;
            }
            let start = match retention.node.observation {
                TraceLashlangNodeObservation::Running {
                    occurrence: retained,
                    start,
                } if retained == *occurrence => Some(start),
                _ => None,
            };
            let (status, observation) = match &event.payload {
                TraceLanguageExecutionPayload::NodeCompleted { .. } => (
                    LanguageExecutionStatus::Completed,
                    TraceLashlangNodeObservation::Completed {
                        occurrence: *occurrence,
                        start,
                        end: timestamp,
                        duration_ms: start.map(|start| {
                            timestamp
                                .signed_duration_since(start)
                                .num_milliseconds()
                                .max(0)
                        }),
                    },
                ),
                TraceLanguageExecutionPayload::NodeFailed { error, .. } => (
                    LanguageExecutionStatus::Failed,
                    TraceLashlangNodeObservation::Failed {
                        occurrence: *occurrence,
                        start,
                        end: timestamp,
                        duration_ms: start.map(|start| {
                            timestamp
                                .signed_duration_since(start)
                                .num_milliseconds()
                                .max(0)
                        }),
                        error: error.clone(),
                    },
                ),
                _ => unreachable!(),
            };
            retention.node.observation = observation;
            retention.node.summary.terminal_count += 1;
            let terminal = TraceLashlangNodeTerminalSummary {
                occurrence: *occurrence,
                status,
                end: timestamp,
            };
            retention.node.summary.first_terminal = Some(terminal.clone());
            retention.node.summary.last_terminal = Some(terminal);
        }
        TraceLanguageExecutionPayload::BranchSelected {
            edge_id, selected, ..
        } => {
            retention.node.branch_selection = Some(*selected);
            if !retention.selected_edge_ids.contains(edge_id) {
                retention.selected_edge_ids.push(edge_id.clone());
                retention.selected_edge_ids.sort();
            }
        }
        TraceLanguageExecutionPayload::ChildStarted { child, .. } => {
            let link = TraceLashlangGraphChildLink {
                parent_graph_key: event.identity.graph_key(),
                parent_node_id: retention.node_id.clone(),
                child_graph_key: child.graph_key(),
                child_process_id: child.process_id.clone(),
                child_incarnation: child.incarnation,
                child_attempt: child.attempt,
                child_module_ref: child.module_ref.clone(),
                child_entry_ref: child.entry_ref.clone(),
                child_entry_name: child.entry_name.clone(),
            };
            if !retention
                .children
                .iter()
                .any(|current| child_link_key(current) == child_link_key(&link))
            {
                retention.children.push(link);
                retention.children.sort_by_key(child_link_key);
            }
        }
        TraceLanguageExecutionPayload::ExecutionStarted { .. }
        | TraceLanguageExecutionPayload::ExecutionFinished { .. } => {}
    }
}

fn merge_node_summary(target: &mut TraceLashlangNodeSummary, archived: &TraceLashlangNodeSummary) {
    target.retained_occurrences += archived.retained_occurrences;
    target.started_count += archived.started_count;
    target.terminal_count += archived.terminal_count;
    target.first_terminal = [
        target.first_terminal.clone(),
        archived.first_terminal.clone(),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|terminal| terminal.occurrence);
    target.last_terminal = [target.last_terminal.clone(), archived.last_terminal.clone()]
        .into_iter()
        .flatten()
        .max_by_key(|terminal| terminal.occurrence);
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

#[expect(
    clippy::expect_used,
    reason = "the fold only serializes its own infallible in-memory trace value types"
)]
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
mod tests;
