use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::{
    TraceEvent, TraceLabelMetadata, TraceLanguageExecutionEvent as LanguageEvent,
    TraceLanguageExecutionIdentity as LanguageIdentity,
    TraceLanguageExecutionMap as LanguageExecutionMap,
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

/// Observed Lashlang graph node state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceLashlangNodeStatus {
    #[default]
    Unobserved,
    Running,
    Completed,
    Failed,
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
    pub status: TraceLashlangNodeStatus,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub duration_ms: Option<i64>,
    pub latest_error: Option<String>,
    pub occurrence: Option<u64>,
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
            status: TraceLashlangNodeStatus::Unobserved,
            first_timestamp: None,
            last_timestamp: None,
            duration_ms: None,
            latest_error: None,
            occurrence: None,
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
        if language != "lashlang" {
            return Ok(());
        }
        let event_key = lashlang_execution_event_key(event);
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
    event: &LanguageEvent,
    timestamp: &str,
) {
    match event {
        LanguageEvent::ExecutionStarted {
            identity,
            execution_map,
            ..
        } => seed_lashlang_graph(graph_mut(state, identity), execution_map),
        LanguageEvent::ExecutionFinished {
            identity, status, ..
        } => {
            graph_mut(state, identity).status = *status;
        }
        LanguageEvent::NodeStarted {
            identity,
            node_id,
            node_kind,
            label,
            occurrence,
            ..
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
            if node.first_timestamp.is_none() {
                node.first_timestamp = Some(timestamp.to_string());
            }
            node.last_timestamp = Some(timestamp.to_string());
            node.status = TraceLashlangNodeStatus::Running;
            node.occurrence = Some(*occurrence);
        }
        LanguageEvent::NodeCompleted {
            identity,
            node_id,
            node_kind,
            label,
            occurrence,
            ..
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
            node.last_timestamp = Some(timestamp.to_string());
            node.duration_ms = duration_ms(node.first_timestamp.as_deref(), Some(timestamp));
            node.status = TraceLashlangNodeStatus::Completed;
            node.occurrence = Some(*occurrence);
        }
        LanguageEvent::NodeFailed {
            identity,
            node_id,
            node_kind,
            label,
            occurrence,
            error,
            ..
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
            node.last_timestamp = Some(timestamp.to_string());
            node.duration_ms = duration_ms(node.first_timestamp.as_deref(), Some(timestamp));
            node.status = TraceLashlangNodeStatus::Failed;
            node.latest_error = Some(error.clone());
            node.occurrence = Some(*occurrence);
        }
        LanguageEvent::BranchSelected {
            identity,
            node_id,
            occurrence,
            edge_id,
            ..
        } => {
            let graph = graph_mut(state, identity);
            if let Some(node) = graph.nodes.get_mut(node_id) {
                node.status = TraceLashlangNodeStatus::Completed;
                node.last_timestamp = Some(timestamp.to_string());
                node.occurrence = Some(*occurrence);
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
                    if selected_node.first_timestamp.is_none() {
                        selected_node.first_timestamp = Some(timestamp.to_string());
                    }
                    selected_node.last_timestamp = Some(timestamp.to_string());
                    selected_node.duration_ms =
                        duration_ms(selected_node.first_timestamp.as_deref(), Some(timestamp));
                    selected_node.status = TraceLashlangNodeStatus::Completed;
                    selected_node.occurrence = Some(*occurrence);
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
        LanguageEvent::ChildStarted {
            identity,
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

fn lashlang_execution_event_key(event: &LanguageEvent) -> &str {
    match event {
        LanguageEvent::ExecutionStarted { event_key, .. }
        | LanguageEvent::ExecutionFinished { event_key, .. }
        | LanguageEvent::NodeStarted { event_key, .. }
        | LanguageEvent::NodeCompleted { event_key, .. }
        | LanguageEvent::NodeFailed { event_key, .. }
        | LanguageEvent::BranchSelected { event_key, .. }
        | LanguageEvent::ChildStarted { event_key, .. } => event_key,
    }
}

fn duration_ms(first: Option<&str>, last: Option<&str>) -> Option<i64> {
    let first = chrono::DateTime::parse_from_rfc3339(first?).ok()?;
    let last = chrono::DateTime::parse_from_rfc3339(last?).ok()?;
    Some((last - first).num_milliseconds().max(0))
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

    fn append_at(store: &TraceLashlangGraphStore, event: LanguageEvent, ms: i64) {
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

    fn started_event(event_key: &str) -> LanguageEvent {
        LanguageEvent::ExecutionStarted {
            event_key: event_key.to_string(),
            identity: identity(),
            execution_map: LanguageExecutionMap {
                module_ref: "module-1".to_string(),
                entry_kind: "main".to_string(),
                entry_ref: None,
                entry_name: "main".to_string(),
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
        }
    }

    fn node_started(event_key: &str, occurrence: u64) -> LanguageEvent {
        LanguageEvent::NodeStarted {
            event_key: event_key.to_string(),
            identity: identity(),
            node_id: "branch".to_string(),
            node_kind: "branch".to_string(),
            label: "if ready".to_string(),
            occurrence,
        }
    }

    fn node_completed(event_key: &str, occurrence: u64) -> LanguageEvent {
        LanguageEvent::NodeCompleted {
            event_key: event_key.to_string(),
            identity: identity(),
            node_id: "branch".to_string(),
            node_kind: "branch".to_string(),
            label: "if ready".to_string(),
            occurrence,
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
        assert_eq!(graph.nodes[0].status, TraceLashlangNodeStatus::Unobserved);
        assert_eq!(
            graph.edges[0].selection,
            TraceLashlangEdgeSelection::Unknown
        );
    }

    #[test]
    fn graph_store_ignores_other_language_execution_events() {
        let store = TraceLashlangGraphStore::default();
        store
            .append(&TraceRecord::new(
                TraceContext::default().for_session("session-1"),
                TraceEvent::LanguageExecution {
                    language: "typescript".to_string(),
                    event: started_event("start"),
                },
            ))
            .expect("append other-language execution event");

        assert!(store.graphs().is_empty());
    }

    #[test]
    fn graph_store_preserves_static_label_metadata() {
        let store = TraceLashlangGraphStore::default();
        let mut event = started_event("start");
        if let LanguageEvent::ExecutionStarted { execution_map, .. } = &mut event {
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
        assert_eq!(graph.nodes[0].status, TraceLashlangNodeStatus::Running);
    }

    #[test]
    fn graph_store_updates_completed_node_duration() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, node_started("start-node", 1), 1_000);
        append_at(&store, node_completed("complete-node", 2), 1_750);

        let graph = store
            .graph("effect:session-1:turn-1:exec-1")
            .expect("graph");
        let node = &graph.nodes[0];
        assert_eq!(node.status, TraceLashlangNodeStatus::Completed);
        assert_eq!(node.duration_ms, Some(750));
        assert_eq!(node.occurrence, Some(2));
    }

    #[test]
    fn graph_store_marks_selected_and_rejected_branch_edges() {
        let store = TraceLashlangGraphStore::default();

        append_at(&store, started_event("start"), 1_000);
        append_at(
            &store,
            LanguageEvent::BranchSelected {
                event_key: "branch".to_string(),
                identity: identity(),
                node_id: "branch".to_string(),
                occurrence: 1,
                edge_id: "then-edge".to_string(),
                selected: TraceBranchSelection::Then,
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
            LanguageEvent::ChildStarted {
                event_key: "child".to_string(),
                identity: identity(),
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
