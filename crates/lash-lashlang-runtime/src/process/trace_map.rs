use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use lash_trace::{
    TraceBranchMembership, TraceBranchSelection, TraceLabelMetadata, TraceLanguageExecutionMap,
    TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode, TraceLanguageExecutionPayload,
};

pub fn trace_lashlang_source_identity(artifact: &lashlang::ModuleArtifact) -> String {
    artifact.source_identity()
}

/// The admitted artifact's graph: identity, structure and sites, with no
/// dialect text (a trace map carries none).
fn artifact_graph(artifact: &lashlang::ModuleArtifact) -> lashlang::WorkflowGraph {
    lashlang::workflow_graph_from_artifact(artifact, &lashlang::NoStatementText)
}

#[derive(Debug, thiserror::Error)]
pub enum TraceLanguageExecutionMapError {
    #[error("failed to read Lashlang module artifact: {0}")]
    ArtifactStore(#[from] lashlang::ArtifactStoreError),
    #[error("Lashlang module artifact `{0}` is unavailable")]
    ArtifactMissing(String),
    #[error("process `{process_name}` is absent from Lashlang module `{module_ref}`")]
    ProcessMissing {
        module_ref: String,
        process_name: String,
    },
}

/// Loads the current process definition and returns its static execution map.
///
/// This is independent of trace delivery: a host can call it after attaching
/// to a resumed process whose initial `ExecutionStarted` event is unavailable.
pub async fn trace_lashlang_process_map_snapshot(
    store: &dyn lashlang::LashlangArtifactStore,
    input: &crate::LashlangProcessInput,
) -> Result<TraceLanguageExecutionMap, TraceLanguageExecutionMapError> {
    let artifact = store
        .get_module_artifact(&input.module_ref)
        .await?
        .ok_or_else(|| {
            TraceLanguageExecutionMapError::ArtifactMissing(input.module_ref.to_string())
        })?;
    trace_lashlang_process_map(&artifact, &input.process_name).ok_or_else(|| {
        TraceLanguageExecutionMapError::ProcessMissing {
            module_ref: input.module_ref.to_string(),
            process_name: input.process_name.clone(),
        }
    })
}

/// Returns the current static execution map for one process definition.
///
/// Hosts may request this independently of the live trace stream, so a
/// resumed segment does not need to repeat `ExecutionStarted` to make its
/// definition discoverable.
pub fn trace_lashlang_process_map(
    artifact: &lashlang::ModuleArtifact,
    process_name: &str,
) -> Option<TraceLanguageExecutionMap> {
    let graph = artifact_graph(artifact);
    let process = graph.process(process_name)?;
    Some(trace_workflow_subgraph(&process.body))
}

pub fn trace_lashlang_main_map(artifact: &lashlang::ModuleArtifact) -> TraceLanguageExecutionMap {
    trace_workflow_subgraph(&artifact_graph(artifact).main)
}

type TraceNodeKey = (String, lash_sansio::ExecutionNodeKind);

fn trace_workflow_subgraph(graph: &lashlang::WorkflowSubgraph) -> TraceLanguageExecutionMap {
    let mut nodes = BTreeMap::new();
    let mut edges = Vec::new();
    append_trace_workflow_subgraph(graph, &[], &mut nodes, &mut edges);
    let endpoint_ids = nodes
        .keys()
        .map(|(node_id, _)| node_id.clone())
        .collect::<BTreeSet<_>>();
    edges.retain(|edge| endpoint_ids.contains(&edge.from) && endpoint_ids.contains(&edge.to));
    TraceLanguageExecutionMap {
        nodes: nodes.into_values().collect(),
        edges,
    }
}

fn append_trace_workflow_subgraph(
    graph: &lashlang::WorkflowSubgraph,
    branch_memberships: &[TraceBranchMembership],
    nodes: &mut BTreeMap<TraceNodeKey, TraceLanguageExecutionMapNode>,
    edges: &mut Vec<TraceLanguageExecutionMapEdge>,
) {
    for node in &graph.nodes {
        let label_metadata =
            (node.name_source == lashlang::WorkflowNodeNameSource::Label).then(|| {
                TraceLabelMetadata {
                    title: node.name.clone(),
                    description: node.description.clone(),
                }
            });
        for site in &node.execution_sites {
            let candidate = TraceLanguageExecutionMapNode {
                id: node.id.to_string(),
                site: site.clone(),
                kind: site.kind,
                label: site.label.clone(),
                branch_memberships: branch_memberships.to_vec(),
                label_metadata: label_metadata.clone(),
            };
            let key = (candidate.id.clone(), candidate.kind);
            match nodes.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }
                // Several operations of one kind share one trace node. Retain
                // the smallest full site descriptor so metadata is independent
                // of projection traversal order.
                Entry::Occupied(mut entry) if candidate.site < entry.get().site => {
                    entry.insert(candidate);
                }
                Entry::Occupied(_) => {}
            }
        }
        if let lashlang::WorkflowNodeKind::Container(container) = &node.kind {
            for (slot, child) in container.child_subgraphs() {
                let mut child_memberships = branch_memberships.to_vec();
                if let lashlang::WorkflowContainer::If { .. } = container {
                    child_memberships.push(TraceBranchMembership {
                        branch_node_id: node.id.to_string(),
                        arm: if slot == "then" {
                            TraceBranchSelection::Then
                        } else {
                            TraceBranchSelection::Else
                        },
                    });
                }
                append_trace_workflow_subgraph(child, &child_memberships, nodes, edges);
            }
        }
    }
    for edge in &graph.edges {
        let label = match &edge.kind {
            lashlang::WorkflowEdgeKind::Sequence => "sequence".to_string(),
            lashlang::WorkflowEdgeKind::DataDependency { variable, version } => {
                format!("{variable}@{version}")
            }
        };
        edges.push(TraceLanguageExecutionMapEdge {
            id: edge.id.clone(),
            from: edge.from.to_string(),
            to: edge.to.to_string(),
            label,
        });
    }
}

pub(super) fn language_event_node_id(payload: &TraceLanguageExecutionPayload) -> Option<&str> {
    match payload {
        TraceLanguageExecutionPayload::NodeStarted { node_id, .. }
        | TraceLanguageExecutionPayload::NodeWaiting { node_id, .. }
        | TraceLanguageExecutionPayload::NodeResumed { node_id, .. }
        | TraceLanguageExecutionPayload::NodeCancelled { node_id, .. }
        | TraceLanguageExecutionPayload::NodeCompleted { node_id, .. }
        | TraceLanguageExecutionPayload::NodeFailed { node_id, .. }
        | TraceLanguageExecutionPayload::BranchSelected { node_id, .. } => Some(node_id),
        TraceLanguageExecutionPayload::ChildStarted { parent_node_id, .. } => Some(parent_node_id),
        TraceLanguageExecutionPayload::ExecutionStarted { .. }
        | TraceLanguageExecutionPayload::ExecutionFinished { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lashlang::testing::ast_builders as b;
    use lashlang::testing::harness::link_labeled;

    fn call(operation: &str) -> lashlang::Expr {
        b::module_call(
            &["tools"],
            operation,
            vec![b::record(vec![("value", b::var("n"))])],
        )
    }

    fn body() -> lashlang::Expr {
        b::block(vec![
            b::assign("n", b::num(1.0)),
            b::finish(b::list(vec![call("echo"), call("err")])),
        ])
    }

    fn assert_map_contract(map: &TraceLanguageExecutionMap, graph: &lashlang::WorkflowSubgraph) {
        let keys = map
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.kind.as_str()))
            .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), map.nodes.len(), "map keys are (node id, kind)");

        let terminal = map
            .nodes
            .iter()
            .find(|node| node.kind == lash_sansio::ExecutionNodeKind::Terminal)
            .expect("terminal site");
        let operation = map
            .nodes
            .iter()
            .find(|node| {
                node.id == terminal.id
                    && node.kind == lash_sansio::ExecutionNodeKind::ResourceOperation
            })
            .expect("the same structural node retains its resource-operation kind");
        assert_eq!(
            operation.label, "echo",
            "the smallest site label is canonical"
        );
        assert_eq!(operation.site.label, "echo");

        let producer = graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Data { .. }))
            .expect("pure producer");
        assert!(
            graph.edges.iter().any(|edge| edge.from == producer.id),
            "the source graph contains the producer dependency"
        );
        assert!(
            map.nodes.iter().all(|node| node.id != producer.id.as_str()),
            "pure producers have no execution site"
        );
        let endpoint_ids = map
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(map.edges.iter().all(|edge| {
            endpoint_ids.contains(edge.from.as_str()) && endpoint_ids.contains(edge.to.as_str())
        }));
    }

    #[test]
    fn main_map_is_keyed_by_node_and_kind_and_closed_over_edges() {
        let linked = link_labeled(b::program(match body() {
            lashlang::Expr::Block(expressions) => expressions,
            _ => unreachable!(),
        }));
        let graph = artifact_graph(&linked.artifact);
        let map = trace_lashlang_main_map(&linked.artifact);
        assert_map_contract(&map, &graph.main);
    }

    #[test]
    fn process_map_is_keyed_by_node_and_kind_and_closed_over_edges() {
        let linked = link_labeled(b::module(
            vec![b::process("worker", Vec::new(), body())],
            Vec::new(),
        ));
        let graph = artifact_graph(&linked.artifact);
        let process = graph.process("worker").expect("worker graph");
        let map = trace_lashlang_process_map(&linked.artifact, "worker").expect("worker map");
        assert_map_contract(&map, &process.body);
    }
}
