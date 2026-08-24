use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::{
    TraceEvent, TraceLabelMetadata, TraceLanguageExecution,
    TraceLanguageExecutionIdentity as LanguageIdentity,
    TraceLanguageExecutionMap as LanguageExecutionMap, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceRecord, TraceRuntimeScope,
    TraceRuntimeSubject, TraceSink, TraceSinkError,
};

/// Trace-derived Lashlang execution graph snapshot for hosts and debugging tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLashlangGraph {
    pub graph_key: String,
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    pub module_ref: String,
    pub entry_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_ref: Option<String>,
    pub entry_name: String,
    pub status: LanguageExecutionStatus,
    pub nodes: Vec<TraceLashlangGraphNode>,
    pub edges: Vec<TraceLashlangGraphEdge>,
    pub children: Vec<TraceLashlangGraphChildLink>,
}

/// One occurrence's observed Lashlang graph node state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TraceLashlangNodeObservation {
    #[default]
    Unobserved,
    Running {
        occurrence: u64,
        start: String,
    },
    Completed {
        occurrence: u64,
        start: String,
        end: String,
        duration_ms: i64,
    },
    Failed {
        occurrence: u64,
        start: String,
        end: String,
        duration_ms: i64,
        error: String,
    },
}

/// Observed branch-edge selection state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangEdgeSelection {
    #[default]
    Unknown,
    Selected,
    Rejected,
}

/// Trace-derived Lashlang graph node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLashlangGraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_metadata: Option<TraceLabelMetadata>,
    #[serde(flatten)]
    pub observation: TraceLashlangNodeObservation,
}

impl TraceLashlangGraphNode {
    fn unobserved(
        id: impl Into<String>,
        kind: impl Into<String>,
        label: impl Into<String>,
        label_metadata: Option<TraceLabelMetadata>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            label_metadata,
            observation: TraceLashlangNodeObservation::Unobserved,
        }
    }
}

/// Trace-derived Lashlang graph edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLashlangGraphEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub label: String,
    pub selection: TraceLashlangEdgeSelection,
}

/// Link from an observed parent Lashlang node to a child execution graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLashlangGraphChildLink {
    pub parent_graph_key: String,
    pub parent_node_id: String,
    pub child_graph_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_module_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_name: Option<String>,
}

/// In-memory store that reduces Lashlang execution trace records into graph snapshots.
#[derive(Default)]
pub struct TraceLashlangGraphStore {
    inner: Mutex<TraceLashlangGraphState>,
}

#[derive(Default)]
struct TraceLashlangGraphState {
    seen_event_keys: BTreeSet<String>,
    graphs: BTreeMap<String, TraceLashlangGraphAccumulator>,
}

#[derive(Clone, Debug)]
struct TraceLashlangGraphAccumulator {
    graph_key: String,
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    module_ref: String,
    entry_kind: String,
    entry_ref: Option<String>,
    entry_name: String,
    status: LanguageExecutionStatus,
    nodes: BTreeMap<String, TraceLashlangGraphNode>,
    edges: BTreeMap<String, TraceLashlangGraphEdge>,
    children: Vec<TraceLashlangGraphChildLink>,
}

impl TraceLashlangGraphStore {
    /// Returns a snapshot for one observed Lashlang graph key.
    pub fn graph(&self, graph_key: &str) -> Option<TraceLashlangGraph> {
        self.inner
            .lock_recover()
            .graphs
            .get(graph_key)
            .map(TraceLashlangGraphAccumulator::to_graph)
    }

    /// Returns snapshots for all observed executions in stable graph-key order.
    pub fn graphs(&self) -> Vec<TraceLashlangGraph> {
        self.inner
            .lock_recover()
            .graphs
            .values()
            .map(TraceLashlangGraphAccumulator::to_graph)
            .collect()
    }

    /// Clears all reduced graph projections and replay de-duplication keys.
    pub fn clear(&self) {
        *self.inner.lock_recover() = TraceLashlangGraphState::default();
    }
}

impl TraceSink for TraceLashlangGraphStore {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        let TraceEvent::LanguageExecution { language, event } = &record.event else {
            return Ok(());
        };
        // Any dialect's executions reduce into this projection. The events
        // describe the *substrate*'s node and edge lifecycle, which is the
        // Lashlang VM under every dialect — the `language` field describes the
        // source that ran, and dropping a session's graph because its source
        // was TypeScript would empty the execution view of every TypeScript
        // session. The filter existed when `lashlang` was the only value this
        // field could take.
        let _ = language;
        let event_key = &event.event_key;
        let mut state = self.inner.lock_recover();
        if !state.seen_event_keys.insert(event_key.to_string()) {
            return Ok(());
        }
        reduce_lashlang_execution_event(&mut state, event, &record.timestamp);
        Ok(())
    }
}

impl TraceLashlangGraphAccumulator {
    fn new(identity: &LanguageIdentity) -> Self {
        Self {
            graph_key: identity.graph_key(),
            scope: identity.scope.clone(),
            subject: identity.subject.clone(),
            module_ref: identity.module_ref.clone(),
            entry_kind: identity.entry_kind.clone(),
            entry_ref: identity.entry_ref.clone(),
            entry_name: identity.entry_name.clone(),
            status: LanguageExecutionStatus::Running,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            children: Vec::new(),
        }
    }

    fn to_graph(&self) -> TraceLashlangGraph {
        TraceLashlangGraph {
            graph_key: self.graph_key.clone(),
            scope: self.scope.clone(),
            subject: self.subject.clone(),
            module_ref: self.module_ref.clone(),
            entry_kind: self.entry_kind.clone(),
            entry_ref: self.entry_ref.clone(),
            entry_name: self.entry_name.clone(),
            status: self.status,
            nodes: self.nodes.values().cloned().collect(),
            edges: self.edges.values().cloned().collect(),
            children: self.children.clone(),
        }
    }
}

fn reduce_lashlang_execution_event(
    state: &mut TraceLashlangGraphState,
    event: &TraceLanguageExecution,
    timestamp: &str,
) {
    let identity = &event.identity;
    match &event.payload {
        TraceLanguageExecutionPayload::ExecutionStarted { execution_map } => {
            seed_lashlang_graph(graph_mut(state, identity), execution_map)
        }
        TraceLanguageExecutionPayload::ExecutionFinished { status, .. } => {
            graph_mut(state, identity).status = *status;
            // A finished or cancelled execution can still leave nodes running.
            // Their terminal observation policy is deliberately unresolved.
        }
        TraceLanguageExecutionPayload::NodeStarted {
            node_id,
            node_kind,
            label,
            occurrence,
        } => {
            let node = node_mut(
                state,
                TraceLashlangNodeIdentity {
                    identity,
                    node_id,
                    node_kind,
                    label,
                },
            );
            node.observation = TraceLashlangNodeObservation::Running {
                occurrence: *occurrence,
                start: timestamp.to_string(),
            };
        }
        TraceLanguageExecutionPayload::NodeCompleted {
            node_id,
            node_kind,
            label,
            occurrence,
        } => {
            let node = node_mut(
                state,
                TraceLashlangNodeIdentity {
                    identity,
                    node_id,
                    node_kind,
                    label,
                },
            );
            let start = current_occurrence_start(node, *occurrence, timestamp);
            node.observation = TraceLashlangNodeObservation::Completed {
                occurrence: *occurrence,
                duration_ms: duration_ms(&start, timestamp),
                start,
                end: timestamp.to_string(),
            };
        }
        TraceLanguageExecutionPayload::NodeFailed {
            node_id,
            node_kind,
            label,
            occurrence,
            error,
        } => {
            let node = node_mut(
                state,
                TraceLashlangNodeIdentity {
                    identity,
                    node_id,
                    node_kind,
                    label,
                },
            );
            let start = current_occurrence_start(node, *occurrence, timestamp);
            node.observation = TraceLashlangNodeObservation::Failed {
                occurrence: *occurrence,
                duration_ms: duration_ms(&start, timestamp),
                start,
                end: timestamp.to_string(),
                error: error.clone(),
            };
        }
        TraceLanguageExecutionPayload::BranchSelected {
            node_id,
            occurrence,
            edge_id,
            ..
        } => {
            let graph = graph_mut(state, identity);
            if let Some(node) = graph.nodes.get_mut(node_id) {
                node.observation = zero_duration_completion(*occurrence, timestamp);
            }
            let selected_edge = graph
                .edges
                .get(edge_id)
                .map(|edge| (edge.from.clone(), edge.to.clone()));
            if let Some(edge) = graph.edges.get_mut(edge_id) {
                edge.selection = TraceLashlangEdgeSelection::Selected;
            }
            if let Some((selected_from, selected_to)) = selected_edge {
                if let Some(selected_node) = graph.nodes.get_mut(&selected_to)
                    && selected_node.kind == "branch_arm"
                {
                    selected_node.observation = zero_duration_completion(*occurrence, timestamp);
                }
                for edge in graph.edges.values_mut() {
                    if edge.from == selected_from
                        && matches!(edge.label.as_str(), "then" | "else")
                        && edge.id != *edge_id
                    {
                        edge.selection = TraceLashlangEdgeSelection::Rejected;
                    }
                }
            }
        }
        TraceLanguageExecutionPayload::ChildStarted {
            parent_node_id,
            child,
            ..
        } => {
            let graph = graph_mut(state, identity);
            let child_graph_key = child.graph_key();
            if !graph.children.iter().any(|link| {
                link.parent_node_id == *parent_node_id && link.child_graph_key == child_graph_key
            }) {
                graph.children.push(TraceLashlangGraphChildLink {
                    parent_graph_key: identity.graph_key(),
                    parent_node_id: parent_node_id.clone(),
                    child_graph_key,
                    child_module_ref: child.module_ref.clone(),
                    child_entry_ref: child.entry_ref.clone(),
                    child_entry_name: child.entry_name.clone(),
                });
            }
        }
    }
}

fn seed_lashlang_graph(
    graph: &mut TraceLashlangGraphAccumulator,
    execution_map: &LanguageExecutionMap,
) {
    graph.status = LanguageExecutionStatus::Running;
    for node in &execution_map.nodes {
        graph.nodes.entry(node.id.clone()).or_insert_with(|| {
            TraceLashlangGraphNode::unobserved(
                node.id.clone(),
                node.kind.clone(),
                node.label.clone(),
                node.label_metadata.clone(),
            )
        });
    }
    for edge in &execution_map.edges {
        graph
            .edges
            .entry(edge.id.clone())
            .or_insert_with(|| TraceLashlangGraphEdge {
                id: edge.id.clone(),
                from: edge.from.clone(),
                to: edge.to.clone(),
                label: edge.label.clone(),
                selection: TraceLashlangEdgeSelection::Unknown,
            });
    }
}

#[derive(Clone, Copy)]
struct TraceLashlangNodeIdentity<'event> {
    identity: &'event LanguageIdentity,
    node_id: &'event str,
    node_kind: &'event str,
    label: &'event str,
}

fn graph_mut<'a>(
    state: &'a mut TraceLashlangGraphState,
    identity: &LanguageIdentity,
) -> &'a mut TraceLashlangGraphAccumulator {
    let graph_key = identity.graph_key();
    state
        .graphs
        .entry(graph_key)
        .or_insert_with(|| TraceLashlangGraphAccumulator::new(identity))
}

fn node_mut<'a>(
    state: &'a mut TraceLashlangGraphState,
    identity: TraceLashlangNodeIdentity<'_>,
) -> &'a mut TraceLashlangGraphNode {
    graph_mut(state, identity.identity)
        .nodes
        .entry(identity.node_id.to_string())
        .or_insert_with(|| {
            TraceLashlangGraphNode::unobserved(
                identity.node_id,
                identity.node_kind,
                identity.label,
                None,
            )
        })
}

fn current_occurrence_start(
    node: &TraceLashlangGraphNode,
    occurrence: u64,
    fallback: &str,
) -> String {
    match &node.observation {
        TraceLashlangNodeObservation::Running {
            occurrence: running_occurrence,
            start,
        } if *running_occurrence == occurrence => start.clone(),
        TraceLashlangNodeObservation::Unobserved
        | TraceLashlangNodeObservation::Running { .. }
        | TraceLashlangNodeObservation::Completed { .. }
        | TraceLashlangNodeObservation::Failed { .. } => fallback.to_string(),
    }
}

fn zero_duration_completion(occurrence: u64, timestamp: &str) -> TraceLashlangNodeObservation {
    TraceLashlangNodeObservation::Completed {
        occurrence,
        start: timestamp.to_string(),
        end: timestamp.to_string(),
        duration_ms: 0,
    }
}

fn duration_ms(first: &str, last: &str) -> i64 {
    let Ok(first) = chrono::DateTime::parse_from_rfc3339(first) else {
        return 0;
    };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else {
        return 0;
    };
    (last - first).num_milliseconds().max(0)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        TraceBranchSelection, TraceContext, TraceLabelMetadata, TraceLanguageChildExecution,
        TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
    };

    fn identity() -> LanguageIdentity {
        LanguageIdentity {
            scope: TraceRuntimeScope {
                session_id: "session-1".to_string(),
                turn_id: Some("turn-1".to_string()),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                effect_id: "exec-1".to_string(),
                kind: "exec_code".to_string(),
            },
            module_ref: "module-1".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
        }
    }

    fn append_at(store: &TraceLashlangGraphStore, event: TraceLanguageExecution, ms: i64) {
        store
            .append(&TraceRecord::new_with_timestamp(
                TraceContext::default().for_session("session-1"),
                TraceEvent::LanguageExecution {
                    language: "lashlang".to_string(),
                    event,
                },
                Utc.timestamp_millis_opt(ms).single().expect("timestamp"),
            ))
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
                            kind: "branch".to_string(),
                            label: "if ready".to_string(),
                            label_metadata: None,
                        },
                        TraceLanguageExecutionMapNode {
                            id: "then".to_string(),
                            kind: "branch_arm".to_string(),
                            label: "then".to_string(),
                            label_metadata: None,
                        },
                        TraceLanguageExecutionMapNode {
                            id: "else".to_string(),
                            kind: "branch_arm".to_string(),
                            label: "else".to_string(),
                            label_metadata: None,
                        },
                    ],
                    edges: vec![
                        TraceLanguageExecutionMapEdge {
                            id: "then-edge".to_string(),
                            from: "branch".to_string(),
                            to: "then".to_string(),
                            label: "then".to_string(),
                        },
                        TraceLanguageExecutionMapEdge {
                            id: "else-edge".to_string(),
                            from: "branch".to_string(),
                            to: "else".to_string(),
                            label: "else".to_string(),
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
                error: error.to_string(),
            },
        }
    }

    #[test]
    fn graph_store_seeds_static_map_on_execution_start() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, started_event("start"), 1_000);

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
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

    /// A TypeScript session's executions reduce into the same projection.
    ///
    /// This test used to assert the opposite, from when `lashlang` was the only
    /// value the field could take. Now that a record carries the dialect of the
    /// source that ran, ignoring anything else would empty the execution view
    /// of every TypeScript session — the projection describes substrate node
    /// and edge lifecycle, which is the same VM under both dialects.
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

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        assert_eq!(
            graph.nodes[0].label_metadata,
            Some(TraceLabelMetadata {
                title: "Choose path".to_string(),
                description: Some("Branch detail".to_string()),
            })
        );
    }

    #[test]
    fn graph_store_ignores_duplicate_event_keys() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("same-key", 1), 1_000);
        append_at(&store, node_completed("same-key", 1), 1_250);

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        assert!(matches!(
            graph.nodes[0].observation,
            TraceLashlangNodeObservation::Running { occurrence: 1, .. }
        ));
    }

    #[test]
    fn graph_store_updates_completed_node_duration() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("start-node", 1), 1_000);
        append_at(&store, node_completed("complete-node", 1), 1_750);

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        let node = &graph.nodes[0];
        assert!(matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed {
                occurrence: 1,
                duration_ms: 750,
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

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        let node = &graph.nodes[0];
        assert!(matches!(
            node.observation,
            TraceLashlangNodeObservation::Completed {
                occurrence: 2,
                duration_ms: 400,
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

            let graph = store
                .graph("effect:session-1:turn-1:exec-1")
                .expect("graph");
            let (occurrence, start, end, duration_ms) = match &graph.nodes[0].observation {
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
            assert_eq!(start, end);
            assert_eq!(*duration_ms, 0);
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

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        let node = graph
            .nodes
            .iter()
            .find(|node| node.id == "branch")
            .expect("branch node");
        let TraceLashlangNodeObservation::Completed {
            occurrence,
            start,
            end,
            duration_ms,
        } = &node.observation
        else {
            panic!("branch node was not completed: {node:#?}");
        };
        assert_eq!(*occurrence, 1);
        assert_eq!(start, end);
        assert_eq!(*duration_ms, 0);

        let serialized = serde_json::to_value(node).expect("serialize branch node");
        assert_eq!(serialized["status"], "completed");
        assert_eq!(serialized["duration_ms"], 0);
        assert!(serialized.get("observation").is_none());
    }

    #[test]
    fn graph_store_marks_selected_and_rejected_branch_edges() {
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

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "then-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Selected)
        );
        assert_eq!(
            graph
                .edges
                .iter()
                .find(|edge| edge.id == "else-edge")
                .map(|edge| edge.selection),
            Some(TraceLashlangEdgeSelection::Rejected)
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
                            process_id: "process:child".to_string(),
                        },
                        module_ref: Some("module-1".to_string()),
                        entry_ref: Some("process:0".to_string()),
                        entry_name: Some("child".to_string()),
                    },
                },
            },
            1_000,
        );

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        assert_eq!(graph.children[0].parent_node_id, "spawn");
        assert_eq!(graph.children[0].child_graph_key, "process:process:child");
        assert_eq!(graph.children[0].child_entry_name.as_deref(), Some("child"));
    }
}
