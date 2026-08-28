use std::collections::{BTreeMap, BTreeSet};

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
    pub(crate) process_id: String,
    pub(crate) status_label: String,
    pub(crate) lifecycle: lash::process::ProcessStatus,
    pub(crate) terminal: bool,
    pub(crate) label: String,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) input: Value,
    pub(crate) error: Option<String>,
    pub(crate) child_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LashlangGraphLineageEdge {
    pub(crate) parent_graph_key: String,
    pub(crate) parent_node_id: String,
    pub(crate) bridge_graph_key: String,
    pub(crate) bridge_process_id: Option<String>,
    pub(crate) bridge_status: String,
    pub(crate) bridge_title: String,
    pub(crate) child_graph_key: Option<String>,
    pub(crate) child_session_id: Option<String>,
    pub(crate) pending: bool,
    pub(crate) terminal: bool,
    pub(crate) error: Option<String>,
}

pub(crate) async fn index_for_session(
    process_observer: &lash::process::ProcessWorkObserver,
    current_session_id: &str,
    graphs: Vec<TraceLashlangGraph>,
) -> Result<LashlangGraphIndex, AppError> {
    let mut projection = GraphProjection::new(process_observer, current_session_id, graphs).await?;
    projection.compute_visibility().await;
    projection.index().await
}

pub(crate) async fn visible_graph_by_key(
    process_observer: &lash::process::ProcessWorkObserver,
    current_session_id: &str,
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
    effect_graphs_by_session: BTreeMap<String, Vec<usize>>,
    processes: BTreeMap<String, Option<lash::process::ObservedProcess>>,
    visible_keys: BTreeSet<String>,
}

impl<'a> GraphProjection<'a> {
    async fn new(
        process_observer: &'a lash::process::ProcessWorkObserver,
        current_session_id: &str,
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
            .visible_process_ids
            .into_iter()
            .collect::<BTreeSet<_>>();

        let graph_by_key = graphs
            .iter()
            .enumerate()
            .map(|(index, graph)| (graph.graph_key.clone(), index))
            .collect::<BTreeMap<_, _>>();
        let mut effect_graphs_by_session: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, graph) in graphs.iter().enumerate() {
            if matches!(
                &graph.subject,
                TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code"
            ) {
                effect_graphs_by_session
                    .entry(graph.scope.session_id.clone())
                    .or_default()
                    .push(index);
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
            if graph.scope.session_id == current_session_id {
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
            effect_graphs_by_session,
            processes,
            visible_keys,
        })
    }

    async fn compute_visibility(&mut self) {
        loop {
            let mut changed = false;
            let roots = self.visible_keys.iter().cloned().collect::<Vec<_>>();
            for graph_key in roots {
                let Some(graph_index) = self.graph_by_key.get(&graph_key).copied() else {
                    continue;
                };
                let children = self.graphs[graph_index].children.clone();
                for child in children {
                    if self.graph_by_key.contains_key(&child.child_graph_key) {
                        changed |= self.visible_keys.insert(child.child_graph_key.clone());
                    }
                    let Some(process_id) = process_id_from_graph_key(&child.child_graph_key) else {
                        continue;
                    };
                    let Some(process) = self.observed_process(process_id).await else {
                        continue;
                    };
                    let Some(child_session_id) = process.child_session_id.as_deref() else {
                        continue;
                    };
                    let child_graph_keys = self
                        .child_session_effect_graphs(child_session_id)
                        .into_iter()
                        .map(|graph| graph.graph_key.clone())
                        .collect::<Vec<_>>();
                    for child_graph_key in child_graph_keys {
                        changed |= self.visible_keys.insert(child_graph_key);
                    }
                }
            }
            if !changed {
                break;
            }
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
        let Some(process_id) = process_id_from_graph_key(&child.child_graph_key) else {
            out.push(LashlangGraphLineageEdge {
                parent_graph_key: child.parent_graph_key.clone(),
                parent_node_id: child.parent_node_id.clone(),
                bridge_graph_key: child.child_graph_key.clone(),
                bridge_process_id: None,
                bridge_status: self.graph_presence_status(&child.child_graph_key),
                bridge_title: child
                    .child_entry_name
                    .clone()
                    .unwrap_or_else(|| short_graph_title(&child.child_graph_key)),
                child_graph_key: Some(child.child_graph_key.clone()),
                child_session_id: None,
                pending: !self.graph_by_key.contains_key(&child.child_graph_key),
                terminal: false,
                error: None,
            });
            return;
        };

        let process = self.observed_process(process_id).await;
        if let Some(process) = process.as_ref()
            && let Some(child_session_id) = process.child_session_id.clone()
        {
            let child_graphs = self.child_session_effect_graphs(&child_session_id);
            if child_graphs.is_empty() {
                out.push(LashlangGraphLineageEdge {
                    parent_graph_key: child.parent_graph_key.clone(),
                    parent_node_id: child.parent_node_id.clone(),
                    bridge_graph_key: child.child_graph_key.clone(),
                    bridge_process_id: Some(process_id.to_string()),
                    bridge_status: process.status_label.clone(),
                    bridge_title: lineage_bridge_title(child, Some(process), process_id),
                    child_graph_key: None,
                    child_session_id: Some(child_session_id),
                    pending: !process.terminal,
                    terminal: process.terminal,
                    error: process.error.clone(),
                });
            } else {
                for graph in child_graphs {
                    out.push(LashlangGraphLineageEdge {
                        parent_graph_key: child.parent_graph_key.clone(),
                        parent_node_id: child.parent_node_id.clone(),
                        bridge_graph_key: child.child_graph_key.clone(),
                        bridge_process_id: Some(process_id.to_string()),
                        bridge_status: process.status_label.clone(),
                        bridge_title: lineage_bridge_title(child, Some(process), process_id),
                        child_graph_key: Some(graph.graph_key.clone()),
                        child_session_id: Some(child_session_id.clone()),
                        pending: false,
                        terminal: process.terminal,
                        error: process.error.clone(),
                    });
                }
            }
            return;
        }

        let child_graph_observed = self.graph_by_key.contains_key(&child.child_graph_key);
        let terminal = process
            .as_ref()
            .map(|process| process.terminal)
            .unwrap_or(false);
        out.push(LashlangGraphLineageEdge {
            parent_graph_key: child.parent_graph_key.clone(),
            parent_node_id: child.parent_node_id.clone(),
            bridge_graph_key: child.child_graph_key.clone(),
            bridge_process_id: Some(process_id.to_string()),
            bridge_status: process
                .as_ref()
                .map(|process| process.status_label.clone())
                .unwrap_or_else(|| self.graph_presence_status(&child.child_graph_key)),
            bridge_title: lineage_bridge_title(child, process.as_ref(), process_id),
            child_graph_key: Some(child.child_graph_key.clone()),
            child_session_id: None,
            pending: !child_graph_observed && !terminal,
            terminal,
            error: process.as_ref().and_then(|process| process.error.clone()),
        });
    }

    fn child_session_effect_graphs(&self, session_id: &str) -> Vec<&TraceLashlangGraph> {
        self.effect_graphs_by_session
            .get(session_id)
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
        process_id: &str,
    ) -> Option<lash::process::ObservedProcess> {
        if !self.processes.contains_key(process_id) {
            let process = self
                .process_observer
                .process(process_id)
                .await
                .ok()
                .flatten();
            self.processes.insert(process_id.to_string(), process);
        }
        self.processes.get(process_id).cloned().flatten()
    }
}

fn process_summary_from_observed(
    process: &lash::process::ObservedProcess,
) -> LashlangGraphProcessSummary {
    LashlangGraphProcessSummary {
        process_id: process.process_id.clone(),
        status_label: process.status_label.clone(),
        lifecycle: process.lifecycle,
        terminal: process.terminal,
        label: process.label.clone(),
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
        TraceRuntimeSubject::Effect { kind, .. } if kind == "exec_code" => "foreground".to_string(),
        TraceRuntimeSubject::Effect { kind, .. } => kind.clone(),
        TraceRuntimeSubject::Process { .. } => "process".to_string(),
    }
}

fn graph_title(graph: &TraceLashlangGraph) -> String {
    match &graph.subject {
        TraceRuntimeSubject::Effect { .. } if graph.entry_name == "main" => {
            "foreground Lashlang".to_string()
        }
        TraceRuntimeSubject::Effect { .. } => graph.entry_name.clone(),
        TraceRuntimeSubject::Process { process_id } => {
            if graph.entry_name.trim().is_empty() {
                process_id.clone()
            } else {
                graph.entry_name.clone()
            }
        }
    }
}

fn process_id_from_graph_key(graph_key: &str) -> Option<&str> {
    graph_key
        .strip_prefix("process:")
        .filter(|id| !id.is_empty())
}

fn lineage_bridge_title(
    child: &lash::tracing::TraceLashlangGraphChildLink,
    process: Option<&lash::process::ObservedProcess>,
    process_id: &str,
) -> String {
    process
        .map(|process| process.label.clone())
        .or_else(|| child.child_entry_name.clone())
        .unwrap_or_else(|| process_id.to_string())
}

fn short_graph_title(graph_key: &str) -> String {
    if let Some(process_id) = process_id_from_graph_key(graph_key) {
        return process_id.to_string();
    }
    graph_key.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::process::ProcessInput as RuntimeInput;
    use lash::tracing::{TraceLanguageExecutionStatus, TraceLashlangGraphChildLink};
    use serde_json::json;
    use std::sync::Arc;

    fn test_process_observer(
        registry: Arc<dyn lash::process::ProcessRegistry>,
    ) -> lash::process::ProcessWorkObserver {
        let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
            .with_native_queued_work()
            .model(
                lash::ModelSpec::builder("test-model")
                    .context_window_tokens(4096)
                    .build()
                    .expect("model spec"),
            )
            .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .store_factory(Arc::new(
                lash::persistence::InMemorySessionStoreFactory::new(),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .process_registry(registry)
            .build(crate::test_core_owner())
            .expect("build core");
        core.processes()
            .observer()
            .expect("process observer configured")
    }

    fn test_graph(
        graph_key: &str,
        session_id: &str,
        subject: TraceRuntimeSubject,
        children: Vec<TraceLashlangGraphChildLink>,
    ) -> TraceLashlangGraph {
        TraceLashlangGraph {
            graph_key: graph_key.to_string(),
            scope: TraceRuntimeScope::new(session_id),
            subject,
            module_ref: format!("{graph_key}:module"),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Running,
            nodes: Vec::new(),
            edges: Vec::new(),
            children,
        }
    }

    #[tokio::test]
    async fn graph_index_resolves_subagent_bridge_to_child_session_effect_graph() {
        let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default())
            as Arc<dyn lash::process::ProcessRegistry>;
        let observer = test_process_observer(Arc::clone(&registry));
        let child_session_id = "child-session";
        let create_request = lash::SessionCreateRequest::child_session(
            "root",
            lash::SessionStartPoint::Empty,
            lash::plugins::PluginOptions::default(),
        )
        .with_session_id(child_session_id);
        registry
            .register_process(lash::process::ProcessRegistration::new(
                "subagent-process",
                RuntimeInput::SessionTurn {
                    definition_key: "agent-workbench-subagent:v1".to_string(),
                    create_request: Box::new(create_request),
                    turn_input: Box::new(lash::TurnInput::text("run child")),
                    output_contract: lash::tools::ToolOutputContract::Static,
                },
                lash::process::RecoveryContract::Rerunnable,
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new("root")),
            ))
            .await
            .expect("register subagent process");

        let parent_graph = TraceLashlangGraph {
            graph_key: "effect:root:turn-1:exec-1".to_string(),
            scope: TraceRuntimeScope {
                session_id: "root".to_string(),
                turn_id: Some("turn-1".to_string()),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                effect_id: "exec-1".to_string(),
                kind: "exec_code".to_string(),
            },
            module_ref: "parent-module".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Running,
            nodes: Vec::new(),
            edges: Vec::new(),
            children: vec![TraceLashlangGraphChildLink {
                parent_graph_key: "effect:root:turn-1:exec-1".to_string(),
                parent_node_id: "spawn".to_string(),
                child_graph_key: "process:subagent-process".to_string(),
                child_module_ref: None,
                child_entry_ref: None,
                child_entry_name: Some("subagent".to_string()),
            }],
        };
        let child_graph = TraceLashlangGraph {
            graph_key: "effect:child-session:turn-1:exec-1".to_string(),
            scope: TraceRuntimeScope {
                session_id: child_session_id.to_string(),
                turn_id: Some("turn-1".to_string()),
                turn_index: Some(0),
                protocol_iteration: Some(0),
            },
            subject: TraceRuntimeSubject::Effect {
                effect_id: "exec-1".to_string(),
                kind: "exec_code".to_string(),
            },
            module_ref: "child-module".to_string(),
            entry_kind: "main".to_string(),
            entry_ref: None,
            entry_name: "main".to_string(),
            status: TraceLanguageExecutionStatus::Completed,
            nodes: Vec::new(),
            edges: Vec::new(),
            children: Vec::new(),
        };
        let mut projection = GraphProjection::new(
            &observer,
            "root",
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
        assert_eq!(edge.bridge_process_id.as_deref(), Some("subagent-process"));
        assert_eq!(edge.bridge_graph_key, "process:subagent-process");
        assert_eq!(edge.child_session_id.as_deref(), Some(child_session_id));
        assert_eq!(
            edge.child_graph_key.as_deref(),
            Some(child_graph.graph_key.as_str())
        );
        assert!(!edge.pending);
    }

    #[tokio::test]
    async fn graph_index_filters_to_current_session_and_reachable_children() {
        let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default())
            as Arc<dyn lash::process::ProcessRegistry>;
        let observer = test_process_observer(Arc::clone(&registry));
        let current_session_id = "current-session";
        let child_session_id = "child-session";
        let old_session_id = "old-session";
        let create_request = lash::SessionCreateRequest::child_session(
            current_session_id,
            lash::SessionStartPoint::Empty,
            lash::plugins::PluginOptions::default(),
        )
        .with_session_id(child_session_id);
        registry
            .register_process(lash::process::ProcessRegistration::new(
                "subagent-process",
                RuntimeInput::SessionTurn {
                    definition_key: "agent-workbench-subagent:v1".to_string(),
                    create_request: Box::new(create_request),
                    turn_input: Box::new(lash::TurnInput::text("run child")),
                    output_contract: lash::tools::ToolOutputContract::Static,
                },
                lash::process::RecoveryContract::Rerunnable,
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    current_session_id,
                )),
            ))
            .await
            .expect("register subagent process");
        registry
            .add_observer(
                current_session_id,
                "subagent-process",
                lash::process::ProcessObserverBy::host("workbench-current"),
            )
            .await
            .expect("observe current process");
        registry
            .register_process(lash::process::ProcessRegistration::new(
                "old-process",
                RuntimeInput::External {
                    metadata: json!({ "old": true }),
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            ))
            .await
            .expect("register old process");
        registry
            .add_observer(
                old_session_id,
                "old-process",
                lash::process::ProcessObserverBy::host("workbench-old"),
            )
            .await
            .expect("observe old process");

        let parent_graph = test_graph(
            "effect:current-session:turn-1:exec-1",
            current_session_id,
            TraceRuntimeSubject::Effect {
                effect_id: "exec-1".to_string(),
                kind: "exec_code".to_string(),
            },
            vec![TraceLashlangGraphChildLink {
                parent_graph_key: "effect:current-session:turn-1:exec-1".to_string(),
                parent_node_id: "spawn".to_string(),
                child_graph_key: "process:subagent-process".to_string(),
                child_module_ref: None,
                child_entry_ref: None,
                child_entry_name: Some("subagent".to_string()),
            }],
        );
        let process_graph = test_graph(
            "process:subagent-process",
            old_session_id,
            TraceRuntimeSubject::Process {
                process_id: "subagent-process".to_string(),
            },
            Vec::new(),
        );
        let child_graph = test_graph(
            "effect:child-session:turn-1:exec-1",
            child_session_id,
            TraceRuntimeSubject::Effect {
                effect_id: "exec-1".to_string(),
                kind: "exec_code".to_string(),
            },
            Vec::new(),
        );
        let old_graph = test_graph(
            "process:old-process",
            old_session_id,
            TraceRuntimeSubject::Process {
                process_id: "old-process".to_string(),
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
        assert!(keys.contains("process:subagent-process"));
        assert!(keys.contains("effect:child-session:turn-1:exec-1"));
        assert!(!keys.contains("process:old-process"));
    }
}
