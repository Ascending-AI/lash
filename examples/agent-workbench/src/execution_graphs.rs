use lash::ProcessId;
use lash::SessionId;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use lash::tracing::{TraceLashlangGraph, TraceRuntimeScope, TraceRuntimeSubject};
use serde::Serialize;
use serde_json::Value;

use crate::{AppError, compact_payload};

#[derive(Debug, Serialize)]
pub(crate) struct LashlangGraphIndex {
    pub(crate) graphs: Vec<LashlangGraphSummary>,
    pub(crate) lineage_edges: Vec<LashlangGraphLineageEdge>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LashlangGraphSummary {
    pub(crate) graph_key: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) kind: String,
    pub(crate) scope: TraceRuntimeScope,
    pub(crate) subject: TraceRuntimeSubject,
    pub(crate) module_ref: String,
    pub(crate) entry_kind: String,
    pub(crate) entry_ref: Option<String>,
    pub(crate) entry_name: String,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) child_count: usize,
    pub(crate) process: Option<LashlangGraphProcessSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LashlangGraphProcessSummary {
    pub(crate) process_id: ProcessId,
    pub(crate) status_label: String,
    pub(crate) lifecycle: lash::process::ProcessStatus,
    pub(crate) terminal: bool,
    pub(crate) label: String,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) input: Value,
    pub(crate) error: Option<String>,
    pub(crate) child_session_id: Option<SessionId>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LashlangGraphLineageEdge {
    pub(crate) parent_graph_key: String,
    pub(crate) parent_node_id: String,
    pub(crate) bridge_graph_key: String,
    pub(crate) bridge_process_id: Option<ProcessId>,
    pub(crate) bridge_status: String,
    pub(crate) bridge_title: String,
    pub(crate) child_graph_key: Option<String>,
    pub(crate) child_session_id: Option<SessionId>,
    pub(crate) pending: bool,
    pub(crate) terminal: bool,
    pub(crate) error: Option<String>,
}

pub(crate) async fn index_for_session(
    process_observer: &lash::process::ProcessWorkObserver,
    current_session_id: &SessionId,
    graphs: Vec<TraceLashlangGraph>,
) -> Result<LashlangGraphIndex, AppError> {
    let mut projection = GraphProjection::new(process_observer, current_session_id, graphs).await?;
    projection.compute_visibility().await;
    projection.index().await
}

pub(crate) async fn visible_graph_by_key(
    process_observer: &lash::process::ProcessWorkObserver,
    current_session_id: &SessionId,
    graphs: Vec<TraceLashlangGraph>,
    graph_key: &str,
) -> Result<TraceLashlangGraph, AppError> {
    let mut projection = GraphProjection::new(process_observer, current_session_id, graphs).await?;
    projection.compute_visibility().await;
    projection
        .graph_if_visible(graph_key)
        .cloned()
        .ok_or_else(|| AppError::not_found(format!("no Lashlang graph for `{graph_key}`")))
}

struct GraphProjection<'a> {
    process_observer: &'a lash::process::ProcessWorkObserver,
    graphs: Vec<TraceLashlangGraph>,
    graph_by_key: BTreeMap<String, usize>,
    process_graphs: BTreeMap<ProcessId, Vec<(usize, Option<u32>)>>,
    effect_graphs_by_session: BTreeMap<Option<SessionId>, Vec<usize>>,
    processes: BTreeMap<ProcessId, Option<lash::process::ObservedProcess>>,
    visible_keys: BTreeSet<String>,
    #[cfg(test)]
    expansions: BTreeMap<String, usize>,
}

impl<'a> GraphProjection<'a> {
    async fn new(
        process_observer: &'a lash::process::ProcessWorkObserver,
        current_session_id: &SessionId,
        graphs: Vec<TraceLashlangGraph>,
    ) -> Result<Self, AppError> {
        let snapshot = process_observer
            .snapshot_for_session(current_session_id)
            .await
            // Audited: process observation reads the global registry, which has no session tombstone contract.
            .map_err(AppError::internal)?;
        let mut processes = BTreeMap::new();
        for item in snapshot.items {
            processes.insert(item.process.process_id.clone(), Some(item.process));
        }
        let visible_process_ids = snapshot
            .visible_processes
            .into_iter()
            .collect::<BTreeSet<_>>();

        let graph_by_key = graphs
            .iter()
            .enumerate()
            .map(|(index, graph)| (graph.graph_key.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let mut process_graphs: BTreeMap<ProcessId, Vec<(usize, Option<u32>)>> = BTreeMap::new();
        let mut effect_graphs_by_session: BTreeMap<Option<SessionId>, Vec<usize>> = BTreeMap::new();
        for (index, graph) in graphs.iter().enumerate() {
            if matches!(&graph.subject, TraceRuntimeSubject::Effect { .. }) {
                effect_graphs_by_session
                    .entry(graph.scope.session_id.clone())
                    .or_default()
                    .push(index);
            } else if let TraceRuntimeSubject::Process { process_id } = &graph.subject
                && let Some(event) = graph.history.first()
            {
                process_graphs
                    .entry(process_id.clone())
                    .or_default()
                    .push((index, event.event.identity.attempt()));
            }
        }
        for indices in effect_graphs_by_session.values_mut() {
            indices.sort_by(|left, right| {
                let left = &graphs[*left];
                let right = &graphs[*right];
                graph_sort_key(&left.scope, &left.graph_key)
                    .cmp(&graph_sort_key(&right.scope, &right.graph_key))
            });
        }

        let mut visible_keys = BTreeSet::new();
        for graph in &graphs {
            if graph.scope.session_id.as_ref() == Some(current_session_id) {
                visible_keys.insert(graph.graph_key.clone());
            }
            if let TraceRuntimeSubject::Process { process_id } = &graph.subject
                && visible_process_ids.contains(process_id)
            {
                visible_keys.insert(graph.graph_key.clone());
            }
        }

        Ok(Self {
            process_observer,
            graphs,
            graph_by_key,
            process_graphs,
            effect_graphs_by_session,
            processes,
            visible_keys,
            #[cfg(test)]
            expansions: BTreeMap::new(),
        })
    }

    async fn compute_visibility(&mut self) {
        let mut pending = self
            .visible_keys
            .iter()
            .filter_map(|key| self.graph_by_key.get(key).copied())
            .collect::<VecDeque<_>>();
        while let Some(graph_index) = pending.pop_front() {
            #[cfg(test)]
            {
                *self
                    .expansions
                    .entry(self.graphs[graph_index].graph_key.clone())
                    .or_default() += 1;
            }
            for child_index in 0..self.graphs[graph_index].children.len() {
                let child = self.graphs[graph_index].children[child_index].clone();
                for graph_key in self.resolved_child_graph_keys(&child) {
                    self.enqueue_visible_graph(graph_key, &mut pending);
                }
                let Some(process) = self.observed_process(&child.child_process_id).await else {
                    continue;
                };
                let Some(child_session_id) = process.child_session_id else {
                    continue;
                };
                let child_graph_keys = self
                    .child_session_effect_graphs(&child_session_id)
                    .into_iter()
                    .map(|graph| graph.graph_key.clone())
                    .collect::<Vec<_>>();
                for graph_key in child_graph_keys {
                    self.enqueue_visible_graph(graph_key, &mut pending);
                }
            }
        }
    }

    fn enqueue_visible_graph(&mut self, graph_key: String, pending: &mut VecDeque<usize>) {
        if self.visible_keys.insert(graph_key.clone())
            && let Some(index) = self.graph_by_key.get(&graph_key)
        {
            pending.push_back(*index);
        }
    }

    async fn index(&mut self) -> Result<LashlangGraphIndex, AppError> {
        let visible = self.visible_graphs_sorted();
        let mut graph_summaries = Vec::with_capacity(visible.len());
        for graph in visible {
            let process = self.graph_process_summary(&graph).await;
            graph_summaries.push(LashlangGraphSummary {
                graph_key: graph.graph_key.clone(),
                title: graph_title(&graph),
                status: format!("{:?}", graph.status).to_ascii_lowercase(),
                kind: graph_kind(&graph),
                scope: graph.scope.clone(),
                subject: graph.subject.clone(),
                module_ref: graph.module_ref.clone(),
                entry_kind: graph.entry_kind.clone(),
                entry_ref: graph.entry_ref.clone(),
                entry_name: graph.entry_name.clone(),
                node_count: graph.nodes.len(),
                edge_count: graph.edges.len(),
                child_count: graph.children.len(),
                process,
            });
        }

        let visible = self.visible_graphs_sorted();
        let mut lineage_edges = Vec::new();
        for graph in visible {
            for child in graph.children {
                self.append_lineage_edges(&child, &mut lineage_edges).await;
            }
        }
        lineage_edges.sort_by(|left, right| {
            left.parent_graph_key
                .cmp(&right.parent_graph_key)
                .then_with(|| left.parent_node_id.cmp(&right.parent_node_id))
                .then_with(|| left.bridge_graph_key.cmp(&right.bridge_graph_key))
                .then_with(|| left.child_graph_key.cmp(&right.child_graph_key))
        });

        Ok(LashlangGraphIndex {
            graphs: graph_summaries,
            lineage_edges,
        })
    }

    fn graph_if_visible(&self, graph_key: &str) -> Option<&TraceLashlangGraph> {
        if !self.visible_keys.contains(graph_key) {
            return None;
        }
        self.graph_by_key
            .get(graph_key)
            .and_then(|index| self.graphs.get(*index))
    }

    fn visible_graphs_sorted(&self) -> Vec<TraceLashlangGraph> {
        let mut graphs = self
            .visible_keys
            .iter()
            .filter_map(|key| self.graph_by_key.get(key))
            .filter_map(|index| self.graphs.get(*index))
            .cloned()
            .collect::<Vec<_>>();
        graphs.sort_by(|left, right| {
            graph_sort_key(&right.scope, &right.graph_key)
                .cmp(&graph_sort_key(&left.scope, &left.graph_key))
        });
        graphs
    }

    async fn graph_process_summary(
        &mut self,
        graph: &TraceLashlangGraph,
    ) -> Option<LashlangGraphProcessSummary> {
        let TraceRuntimeSubject::Process { process_id } = &graph.subject else {
            return None;
        };
        self.observed_process(process_id)
            .await
            .map(|process| process_summary_from_observed(&process))
    }

    async fn append_lineage_edges(
        &mut self,
        child: &lash::tracing::TraceLashlangGraphChildLink,
        out: &mut Vec<LashlangGraphLineageEdge>,
    ) {
        let process_id = child.child_process_id.clone();
        let bridge_graph_key = child
            .child_graph_key
            .clone()
            .unwrap_or_else(|| format!("process:{process_id}"));
        let process = self.observed_process(&process_id).await;
        if let Some(process) = process.as_ref()
            && let Some(child_session_id) = process.child_session_id.clone()
        {
            let child_graphs = self.child_session_effect_graphs(&child_session_id);
            if child_graphs.is_empty() {
                out.push(LashlangGraphLineageEdge {
                    parent_graph_key: child.parent_graph_key.clone(),
                    parent_node_id: child.parent_node_id.clone(),
                    bridge_graph_key: bridge_graph_key.clone(),
                    bridge_process_id: Some(process_id.clone()),
                    bridge_status: process.status_label().to_string(),
                    bridge_title: lineage_bridge_title(child, Some(process), &process_id),
                    child_graph_key: None,
                    child_session_id: Some(child_session_id),
                    pending: !process.terminal(),
                    terminal: process.terminal(),
                    error: process.error.clone(),
                });
            } else {
                for graph in child_graphs {
                    out.push(LashlangGraphLineageEdge {
                        parent_graph_key: child.parent_graph_key.clone(),
                        parent_node_id: child.parent_node_id.clone(),
                        bridge_graph_key: bridge_graph_key.clone(),
                        bridge_process_id: Some(process_id.clone()),
                        bridge_status: process.status_label().to_string(),
                        bridge_title: lineage_bridge_title(child, Some(process), &process_id),
                        child_graph_key: Some(graph.graph_key.clone()),
                        child_session_id: Some(child_session_id.clone()),
                        pending: false,
                        terminal: process.terminal(),
                        error: process.error.clone(),
                    });
                }
            }
            return;
        }

        let child_graph_keys = self.resolved_child_graph_keys(child);
        let child_graph_observed = !child_graph_keys.is_empty();
        let terminal = process
            .as_ref()
            .map(|process| process.terminal())
            .unwrap_or(false);
        let targets = if child_graph_keys.is_empty() {
            vec![None]
        } else {
            child_graph_keys.into_iter().map(Some).collect()
        };
        for child_graph_key in targets {
            let status_key = child_graph_key.as_deref().unwrap_or(&bridge_graph_key);
            out.push(LashlangGraphLineageEdge {
                parent_graph_key: child.parent_graph_key.clone(),
                parent_node_id: child.parent_node_id.clone(),
                bridge_graph_key: bridge_graph_key.clone(),
                bridge_process_id: Some(process_id.clone()),
                bridge_status: process
                    .as_ref()
                    .map(|process| process.status_label().to_string())
                    .unwrap_or_else(|| self.graph_presence_status(status_key)),
                bridge_title: lineage_bridge_title(child, process.as_ref(), &process_id),
                child_graph_key,
                child_session_id: None,
                pending: !child_graph_observed && !terminal,
                terminal,
                error: process.as_ref().and_then(|process| process.error.clone()),
            });
        }
    }

    fn resolved_child_graph_keys(
        &self,
        child: &lash::tracing::TraceLashlangGraphChildLink,
    ) -> Vec<String> {
        if let Some(graph_key) = &child.child_graph_key
            && self.graph_by_key.contains_key(graph_key)
        {
            return vec![graph_key.clone()];
        }
        self.process_graphs
            .get(&child.child_process_id)
            .into_iter()
            .flatten()
            .filter(|(_, attempt)| {
                child
                    .child_attempt
                    .is_none_or(|expected| *attempt == Some(expected))
            })
            .map(|(index, _)| self.graphs[*index].graph_key.clone())
            .collect()
    }

    fn child_session_effect_graphs(&self, session_id: &SessionId) -> Vec<&TraceLashlangGraph> {
        self.effect_graphs_by_session
            .get(&Some(session_id.clone()))
            .into_iter()
            .flatten()
            .filter_map(|index| self.graphs.get(*index))
            .collect()
    }

    fn graph_presence_status(&self, graph_key: &str) -> String {
        if self.graph_by_key.contains_key(graph_key) {
            "observed".to_string()
        } else {
            "pending".to_string()
        }
    }

    async fn observed_process(
        &mut self,
        process_id: &ProcessId,
    ) -> Option<lash::process::ObservedProcess> {
        if !self.processes.contains_key(process_id) {
            let process = self
                .process_observer
                .process(process_id)
                .await
                .ok()
                .flatten();
            self.processes.insert(process_id.clone(), process);
        }
        self.processes.get(process_id).cloned().flatten()
    }
}

fn process_summary_from_observed(
    process: &lash::process::ObservedProcess,
) -> LashlangGraphProcessSummary {
    LashlangGraphProcessSummary {
        process_id: process.process_id.clone(),
        status_label: process.status_label().to_string(),
        lifecycle: process.lifecycle,
        terminal: process.terminal(),
        label: process.label().to_string(),
        created_at_ms: process.created_at_ms,
        updated_at_ms: process.updated_at_ms,
        input: compact_payload(serde_json::to_value(&process.input).unwrap_or(Value::Null)),
        error: process.error.clone(),
        child_session_id: process.child_session_id.clone(),
    }
}

fn graph_sort_key(scope: &TraceRuntimeScope, graph_key: &str) -> (usize, usize, String) {
    (
        scope.turn_index.unwrap_or_default(),
        scope.protocol_iteration.unwrap_or_default(),
        graph_key.to_string(),
    )
}

fn graph_kind(graph: &TraceLashlangGraph) -> String {
    match &graph.subject {
        TraceRuntimeSubject::Effect { .. } if graph.entry_name == "main" => {
            "foreground".to_string()
        }
        TraceRuntimeSubject::Effect { .. } => "effect".to_string(),
        TraceRuntimeSubject::Process { .. } => "process".to_string(),
    }
}

fn graph_title(graph: &TraceLashlangGraph) -> String {
    match &graph.subject {
        TraceRuntimeSubject::Effect { .. } if graph.entry_name == "main" => {
            "foreground execution".to_string()
        }
        TraceRuntimeSubject::Effect { .. } => graph.entry_name.clone(),
        TraceRuntimeSubject::Process { process_id } => {
            if graph.entry_name.trim().is_empty() {
                process_id.to_string()
            } else {
                graph.entry_name.clone()
            }
        }
    }
}

fn lineage_bridge_title(
    child: &lash::tracing::TraceLashlangGraphChildLink,
    process: Option<&lash::process::ObservedProcess>,
    process_id: &ProcessId,
) -> String {
    process
        .map(|process| process.label().to_string())
        .or_else(|| child.child_entry_name.clone())
        .unwrap_or_else(|| process_id.to_string())
}

#[cfg(test)]
mod tests {}
