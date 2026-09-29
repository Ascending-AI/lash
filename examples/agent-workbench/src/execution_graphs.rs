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
    #[cfg(test)]
    fallback_candidates: std::cell::Cell<usize>,
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
            #[cfg(test)]
            fallback_candidates: std::cell::Cell::new(0),
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
                #[cfg(test)]
                self.fallback_candidates
                    .set(self.fallback_candidates.get() + 1);
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
mod tests {
    use super::*;
    use lash::TurnId;
    use lash::process::ProcessInput as RuntimeInput;
    use lash::tracing::{TraceLanguageExecutionStatus, TraceLashlangGraphChildLink};
    use serde_json::json;
    use std::sync::Arc;

    const SEED: u64 = 0xf9_0008;

    /// A process observer over the Restate double's backend, and the
    /// backend's registry the test writes process rows into.
    async fn test_process_observer() -> (
        lash::process::ProcessWorkObserver,
        Arc<dyn lash::process::ProcessRegistry>,
        lash_restate_test::RestateTestBackend,
    ) {
        let double = crate::tests::test_double_backend(SEED).await;
        let backend = double.lash_backend();
        let registry = backend.process_registry() as Arc<dyn lash::process::ProcessRegistry>;
        let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
            .model(
                lash::ModelSpec::builder("test-model")
                    .context_window_tokens(4096)
                    .build()
                    .expect("model spec"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(crate::test_core_owner())
            .expect("build core");
        let observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        (observer, registry, double)
    }

    fn test_graph(
        graph_key: &str,
        session_id: &SessionId,
        subject: TraceRuntimeSubject,
        children: Vec<TraceLashlangGraphChildLink>,
    ) -> TraceLashlangGraph {
        TraceLashlangGraph {
            schema_version: lash::tracing::TRACE_SCHEMA_VERSION,
            graph_key: graph_key.to_string(),
            scope: TraceRuntimeScope::new(session_id),
            subject,
            source_identity: format!("{graph_key}:source"),
            module_ref: format!("{graph_key}:module"),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Running,
            completeness: lash::tracing::TraceLashlangGraphCompleteness::IncompleteMap,
            nodes: Vec::new(),
            edges: Vec::new(),
            children,
            history_limit: lash::tracing::DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT,
            node_retention: Vec::new(),
            conflicts: Vec::new(),
            history: Vec::new(),
            execution_map: None,
        }
    }

    #[test]
    fn foreground_graph_title_is_dialect_neutral() {
        let graph = test_graph(
            "effect:session:turn:exec",
            &SessionId::from("session"),
            TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn("session", "turn-1"),
                    "exec",
                )
                .expect("valid test effect address"),
                effect_id: "exec".to_string(),
            },
            Vec::new(),
        );

        assert_eq!(graph_title(&graph), "foreground execution");
    }

    #[tokio::test]
    async fn graph_index_resolves_subagent_bridge_to_child_session_effect_graph() {
        let (observer, registry, _double) = test_process_observer().await;
        let child_session_id = "child-session";
        let create_request = lash::SessionCreateRequest::child_session(
            "root",
            lash::SessionStartPoint::Empty,
            lash::plugins::PluginOptions::default(),
        )
        .with_session_id(child_session_id);
        let subagent_process_id = registry
            .register_process(lash::process::ProcessRegistration::new(
                RuntimeInput::SessionTurn {
                    definition_key: "agent-workbench-subagent:v1".to_string(),
                    create_request: Box::new(create_request),
                    turn_input: Box::new(lash::TurnInput::text("run child")),
                    result: lash::process::SessionTurnOutcome::Turn,
                },
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new("root")),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register subagent process")
            .id;

        let parent_graph = TraceLashlangGraph {
            schema_version: lash::tracing::TRACE_SCHEMA_VERSION,
            graph_key: "effect:root:turn-1:exec-1".to_string(),
            scope: TraceRuntimeScope {
                session_id: Some(SessionId::from("root")),
                turn_id: Some(TurnId::from("turn-1")),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn("root", "turn-1"),
                    "exec-1",
                )
                .expect("valid parent effect address"),
                effect_id: "exec-1".to_string(),
            },
            source_identity: "parent-source".to_string(),
            module_ref: "parent-module".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Running,
            completeness: lash::tracing::TraceLashlangGraphCompleteness::IncompleteMap,
            nodes: Vec::new(),
            edges: Vec::new(),
            children: vec![TraceLashlangGraphChildLink {
                parent_graph_key: "effect:root:turn-1:exec-1".to_string(),
                parent_node_id: "spawn".to_string(),
                child_graph_key: None,
                child_process_id: subagent_process_id.clone(),
                child_attempt: None,
                child_module_ref: None,
                child_entry_ref: None,
                child_entry_name: Some("subagent".to_string()),
            }],
            history_limit: lash::tracing::DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT,
            node_retention: Vec::new(),
            conflicts: Vec::new(),
            history: Vec::new(),
            execution_map: None,
        };
        let child_graph = TraceLashlangGraph {
            schema_version: lash::tracing::TRACE_SCHEMA_VERSION,
            graph_key: "effect:child-session:turn-1:exec-1".to_string(),
            scope: TraceRuntimeScope {
                session_id: Some(SessionId::from(child_session_id.to_string())),
                turn_id: Some(TurnId::from("turn-1")),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn(child_session_id, "turn-1"),
                    "exec-1",
                )
                .expect("valid child effect address"),
                effect_id: "exec-1".to_string(),
            },
            source_identity: "child-source".to_string(),
            module_ref: "child-module".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Completed,
            completeness: lash::tracing::TraceLashlangGraphCompleteness::IncompleteMap,
            nodes: Vec::new(),
            edges: Vec::new(),
            children: Vec::new(),
            history_limit: lash::tracing::DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT,
            node_retention: Vec::new(),
            conflicts: Vec::new(),
            history: Vec::new(),
            execution_map: None,
        };
        let mut projection = GraphProjection::new(
            &observer,
            &SessionId::from("root"),
            vec![parent_graph.clone(), child_graph.clone()],
        )
        .await
        .expect("projection");
        let mut lineage_edges = Vec::new();

        projection
            .append_lineage_edges(&parent_graph.children[0], &mut lineage_edges)
            .await;

        assert_eq!(lineage_edges.len(), 1);
        let edge = &lineage_edges[0];
        assert_eq!(edge.bridge_process_id.as_ref(), Some(&subagent_process_id));
        assert_eq!(
            edge.bridge_graph_key,
            format!("process:{subagent_process_id}")
        );
        assert_eq!(edge.child_session_id.as_deref(), Some(child_session_id));
        assert_eq!(
            edge.child_graph_key.as_deref(),
            Some(child_graph.graph_key.as_str())
        );
        assert!(!edge.pending);
    }

    #[tokio::test]
    async fn graph_index_filters_to_current_session_and_reachable_children() {
        let (observer, registry, _double) = test_process_observer().await;
        let current_session_id = &SessionId::from("current-session");
        let child_session_id = &SessionId::from("child-session");
        let old_session_id = &SessionId::from("old-session");
        let create_request = lash::SessionCreateRequest::child_session(
            current_session_id,
            lash::SessionStartPoint::Empty,
            lash::plugins::PluginOptions::default(),
        )
        .with_session_id(child_session_id);
        let subagent_process_id = registry
            .register_process(lash::process::ProcessRegistration::new(
                RuntimeInput::SessionTurn {
                    definition_key: "agent-workbench-subagent:v1".to_string(),
                    create_request: Box::new(create_request),
                    turn_input: Box::new(lash::TurnInput::text("run child")),
                    result: lash::process::SessionTurnOutcome::Turn,
                },
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    current_session_id,
                )),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register subagent process")
            .id;
        registry
            .add_observer(
                &SessionId::from(current_session_id),
                &subagent_process_id,
                lash::process::ProcessObserverBy::host("workbench-current"),
            )
            .await
            .expect("observe current process");
        let old_process_id = registry
            .register_process(lash::process::ProcessRegistration::new(
                RuntimeInput::External {
                    metadata: json!({ "old": true }),
                },
                lash::process::ProcessProvenance::host(),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register old process")
            .id;
        registry
            .add_observer(
                &SessionId::from(old_session_id),
                &old_process_id,
                lash::process::ProcessObserverBy::host("workbench-old"),
            )
            .await
            .expect("observe old process");

        let parent_graph = test_graph(
            "effect:current-session:turn-1:exec-1",
            current_session_id,
            TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn(current_session_id, "turn-1"),
                    "exec-1",
                )
                .expect("valid current-session effect address"),
                effect_id: "exec-1".to_string(),
            },
            vec![TraceLashlangGraphChildLink {
                parent_graph_key: "effect:current-session:turn-1:exec-1".to_string(),
                parent_node_id: "spawn".to_string(),
                child_graph_key: None,
                child_process_id: subagent_process_id.clone(),
                child_attempt: None,
                child_module_ref: None,
                child_entry_ref: None,
                child_entry_name: Some("subagent".to_string()),
            }],
        );
        let process_graph = test_graph(
            &format!("process:{subagent_process_id}"),
            old_session_id,
            TraceRuntimeSubject::Process {
                process_id: subagent_process_id.clone(),
            },
            Vec::new(),
        );
        let child_graph = test_graph(
            "effect:child-session:turn-1:exec-1",
            child_session_id,
            TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn(child_session_id, "turn-1"),
                    "exec-1",
                )
                .expect("valid child-session effect address"),
                effect_id: "exec-1".to_string(),
            },
            Vec::new(),
        );
        let old_graph = test_graph(
            &format!("process:{old_process_id}"),
            old_session_id,
            TraceRuntimeSubject::Process {
                process_id: old_process_id.clone(),
            },
            Vec::new(),
        );

        let mut projection = GraphProjection::new(
            &observer,
            current_session_id,
            vec![parent_graph, process_graph, child_graph, old_graph],
        )
        .await
        .expect("projection");
        projection.compute_visibility().await;
        let keys = projection.visible_keys;

        assert!(keys.contains("effect:current-session:turn-1:exec-1"));
        assert!(keys.contains(&format!("process:{subagent_process_id}")));
        assert!(keys.contains("effect:child-session:turn-1:exec-1"));
        assert!(!keys.contains(&format!("process:{old_process_id}")));
    }
    fn effect_graph(key: &str, session: &str) -> TraceLashlangGraph {
        test_graph(
            key,
            &SessionId::from(session),
            TraceRuntimeSubject::Effect {
                address: lash::runtime::EffectAddress::new(
                    lash::runtime::ExecutionScope::turn(session, "turn"),
                    key,
                )
                .expect("effect address"),
                effect_id: key.to_string(),
            },
            Vec::new(),
        )
    }

    fn child_link(
        parent: &str,
        target: Option<&str>,
        process: &str,
        attempt: Option<u32>,
    ) -> TraceLashlangGraphChildLink {
        TraceLashlangGraphChildLink {
            parent_graph_key: parent.to_string(),
            parent_node_id: format!("spawn-{process}"),
            child_graph_key: target.map(str::to_string),
            child_process_id: ProcessId::parse(process)
                .unwrap_or_else(|_| ProcessId::fixture(process)),
            child_attempt: attempt,
            child_module_ref: None,
            child_entry_ref: None,
            child_entry_name: None,
        }
    }

    fn process_graph(
        key: &str,
        process: &str,
        history_attempt: Option<Option<u32>>,
    ) -> TraceLashlangGraph {
        let mut graph = test_graph(
            key,
            &SessionId::from("other"),
            TraceRuntimeSubject::Process {
                process_id: ProcessId::parse(process)
                    .unwrap_or_else(|_| ProcessId::fixture(process)),
            },
            Vec::new(),
        );
        if let Some(attempt) = history_attempt {
            graph.history.push(
                serde_json::from_value(json!({
                    "identity": { "attempt": attempt, "transition": "execution_finished" },
                    "timestamp": "2026-09-29T00:00:00Z",
                    "event": {
                        "event_key": key,
                        "identity": {
                            "scope": graph.scope,
                            "subject": graph.subject,
                            "source_identity": graph.source_identity,
                            "module_ref": graph.module_ref,
                            "entry_kind": "main",
                            "entry_name": "main",
                            "attempt": attempt,
                        },
                        "kind": "execution_finished",
                        "status": "completed",
                    },
                }))
                .expect("history event"),
            );
        }
        graph
    }

    fn traversal_fixtures() -> Vec<Vec<TraceLashlangGraph>> {
        let chain = (0..64)
            .map(|index| {
                let key = format!("chain-{index:02}");
                let mut graph = effect_graph(&key, if index == 0 { "root" } else { "other" });
                if index < 63 {
                    graph.children.push(child_link(
                        &key,
                        Some(&format!("chain-{:02}", index + 1)),
                        "absent",
                        None,
                    ));
                }
                graph
            })
            .collect::<Vec<_>>();
        let mut diamond = ["root", "left", "right", "leaf", "inaccessible"]
            .map(|key| effect_graph(key, if key == "root" { "root" } else { "other" }))
            .to_vec();
        diamond[0].children = vec![
            child_link("root", Some("left"), "absent", None),
            child_link("root", Some("right"), "absent", None),
        ];
        diamond[1].children = vec![child_link("left", Some("leaf"), "absent", None)];
        diamond[2].children = vec![child_link("right", Some("leaf"), "absent", None)];
        let mut cycle = diamond.clone();
        cycle[3].children = vec![child_link("leaf", Some("root"), "absent", None)];
        vec![chain, diamond, cycle]
    }

    async fn reference_visibility(projection: &mut GraphProjection<'_>) {
        loop {
            let mut changed = false;
            for key in projection.visible_keys.clone() {
                let children = projection.graphs[projection.graph_by_key[&key]]
                    .children
                    .clone();
                for child in children {
                    let targets = if let Some(key) = &child.child_graph_key
                        && projection.graph_by_key.contains_key(key)
                    {
                        vec![key.clone()]
                    } else {
                        projection.graphs.iter().filter(|graph| {
                            matches!(&graph.subject, TraceRuntimeSubject::Process { process_id } if process_id == child.child_process_id)
                                && graph.history.first().is_some_and(|event| child.child_attempt.is_none_or(|attempt| event.event.identity.attempt() == Some(attempt)))
                        }).map(|graph| graph.graph_key.clone()).collect()
                    };
                    for target in targets {
                        changed |= projection.visible_keys.insert(target);
                    }
                    if let Some(process) =
                        projection.observed_process(&child.child_process_id).await
                        && let Some(session) = process.child_session_id
                    {
                        let targets = projection
                            .child_session_effect_graphs(&session)
                            .iter()
                            .map(|graph| graph.graph_key.clone())
                            .collect::<Vec<_>>();
                        for target in targets {
                            changed |= projection.visible_keys.insert(target);
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    async fn assert_projection_matches_reference(
        observer: &lash::process::ProcessWorkObserver,
        graphs: Vec<TraceLashlangGraph>,
    ) {
        let session = SessionId::from("root");
        let mut reference = GraphProjection::new(observer, &session, graphs.clone())
            .await
            .expect("reference");
        reference_visibility(&mut reference).await;
        let expected =
            serde_json::to_value(reference.index().await.expect("reference index")).expect("JSON");
        let actual = serde_json::to_value(
            index_for_session(observer, &session, graphs.clone())
                .await
                .expect("index"),
        )
        .expect("JSON");
        assert_eq!(actual, expected);
        for graph in &graphs {
            let detail =
                visible_graph_by_key(observer, &session, graphs.clone(), &graph.graph_key).await;
            if reference.visible_keys.contains(&graph.graph_key) {
                assert_eq!(
                    serde_json::to_value(detail.expect("visible detail")).unwrap(),
                    serde_json::to_value(graph).unwrap()
                );
            } else {
                assert!(detail.is_err(), "{} must be inaccessible", graph.graph_key);
            }
        }
    }

    #[tokio::test]
    async fn visibility_expands_each_graph_once() {
        let (observer, _, _double) = test_process_observer().await;
        for graphs in traversal_fixtures() {
            let mut projection = GraphProjection::new(&observer, &SessionId::from("root"), graphs)
                .await
                .expect("projection");
            projection.compute_visibility().await;
            assert_eq!(
                projection
                    .expansions
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                projection.visible_keys
            );
            assert!(
                projection.expansions.values().all(|count| *count == 1),
                "expansions: {:?}",
                projection.expansions
            );
            assert_eq!(projection.fallback_candidates.get(), 0);
        }
    }

    #[tokio::test]
    async fn fallback_only_inspects_process_candidates() {
        let (observer, _, _double) = test_process_observer().await;
        let mut graphs = vec![
            process_graph("attempt-two", "target", Some(Some(2))),
            process_graph("empty", "target", None),
            process_graph("attempt-one", "target", Some(Some(1))),
            process_graph("unset", "target", Some(None)),
        ];
        // The first history event owns the attempt, even if a later one differs.
        let later = graphs[2].history[0].clone();
        graphs[0].history.push(later);
        graphs.extend(
            (0..40).map(|index| {
                process_graph(&format!("unrelated-{index}"), "unrelated", Some(Some(1)))
            }),
        );
        let projection = GraphProjection::new(&observer, &SessionId::from("root"), graphs)
            .await
            .expect("projection");
        for (explicit, attempt, expected, inspected) in [
            (Some("missing"), Some(1), vec!["attempt-one"], 3),
            (None, Some(2), vec!["attempt-two"], 3),
            (None, None, vec!["attempt-two", "attempt-one", "unset"], 3),
            (None, Some(3), vec![], 3),
            (Some("empty"), Some(1), vec!["empty"], 0),
            (Some("unrelated-0"), Some(2), vec!["unrelated-0"], 0),
        ] {
            projection.fallback_candidates.set(0);
            assert_eq!(
                projection
                    .resolved_child_graph_keys(&child_link("root", explicit, "target", attempt)),
                expected
            );
            assert_eq!(projection.fallback_candidates.get(), inspected);
        }
        projection.fallback_candidates.set(0);
        assert!(
            projection
                .resolved_child_graph_keys(&child_link("root", None, "absent", None))
                .is_empty()
        );
        assert_eq!(projection.fallback_candidates.get(), 0);
    }

    #[tokio::test]
    async fn visibility_matches_reference_index_and_detail() {
        let (observer, _, _double) = test_process_observer().await;
        let mut fixtures = traversal_fixtures();
        let mut root = effect_graph("root", "root");
        root.children = vec![
            child_link("root", Some("missing"), "target", Some(1)),
            child_link("root", None, "target", None),
        ];
        let mut first = process_graph("attempt-one", "target", Some(Some(1)));
        first.scope.turn_index = Some(2);
        let mut second = process_graph("attempt-two", "target", Some(Some(2)));
        second.scope.protocol_iteration = Some(3);
        fixtures.push(vec![
            root,
            second,
            process_graph("empty", "target", None),
            first,
            process_graph("unset", "target", Some(None)),
            effect_graph("inaccessible", "old"),
        ]);
        for graphs in fixtures {
            assert_projection_matches_reference(&observer, graphs).await;
        }
    }

    #[tokio::test]
    async fn visibility_keeps_later_child_session_paths() {
        let (observer, registry, _double) = test_process_observer().await;
        let process_id = registry
            .register_process(lash::process::ProcessRegistration::new(
                RuntimeInput::SessionTurn {
                    definition_key: "agent-workbench-subagent:v1".to_string(),
                    create_request: Box::new(
                        lash::SessionCreateRequest::child_session(
                            "root",
                            lash::SessionStartPoint::Empty,
                            lash::plugins::PluginOptions::default(),
                        )
                        .with_session_id("child"),
                    ),
                    turn_input: Box::new(lash::TurnInput::text("run child")),
                    result: lash::process::SessionTurnOutcome::Turn,
                },
                lash::process::ProcessProvenance::host(),
                lash::process::Lifetime::Detached,
            ))
            .await
            .expect("register process")
            .id;
        let mut root = effect_graph("root", "root");
        root.children = vec![
            child_link("root", Some("bridge"), "absent", None),
            child_link("root", Some("later"), "absent", None),
        ];
        let bridge = process_graph("bridge", process_id.as_str(), None);
        let mut later = effect_graph("later", "other");
        later.children = vec![child_link(
            "later",
            Some("bridge"),
            process_id.as_str(),
            None,
        )];
        let mut first_effect = effect_graph("child-first", "child");
        first_effect.scope.turn_index = Some(1);
        first_effect.children = vec![child_link(
            "child-first",
            Some("descendant"),
            "absent",
            None,
        )];
        let graphs = vec![
            root,
            bridge,
            later,
            first_effect,
            effect_graph("child-second", "child"),
            effect_graph("descendant", "other"),
            effect_graph("inaccessible", "old"),
        ];
        let mut projection =
            GraphProjection::new(&observer, &SessionId::from("root"), graphs.clone())
                .await
                .expect("projection");
        projection.compute_visibility().await;
        assert_eq!(
            projection.visible_keys,
            [
                "root",
                "bridge",
                "later",
                "child-first",
                "child-second",
                "descendant"
            ]
            .map(str::to_string)
            .into_iter()
            .collect()
        );
        assert!(projection.expansions.values().all(|count| *count == 1));
        let index = projection.index().await.expect("index");
        let child_edges = index
            .lineage_edges
            .iter()
            .filter(|edge| edge.parent_graph_key == "later")
            .collect::<Vec<_>>();
        assert_eq!(
            child_edges
                .iter()
                .map(|edge| edge.child_graph_key.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("child-first"), Some("child-second")]
        );
        assert!(
            child_edges
                .iter()
                .all(|edge| edge.child_session_id.as_deref() == Some("child") && !edge.pending)
        );
        assert_projection_matches_reference(&observer, graphs).await;
    }
}
