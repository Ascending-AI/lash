use lash_trace::{
    TraceLabelMetadata, TraceLanguageExecutionMap, TraceLanguageExecutionMapEdge,
    TraceLanguageExecutionMapNode, TraceLanguageExecutionPayload,
};

pub fn trace_lashlang_source_identity(artifact: &lashlang::ModuleArtifact) -> String {
    lash_typescript::workflow_graph::workflow_graph_from_program(&artifact.canonical_ir)
        .source_identity
}

pub(super) fn trace_lashlang_process_map(
    artifact: &lashlang::ModuleArtifact,
    process_name: &str,
) -> TraceLanguageExecutionMap {
    let graph =
        lash_typescript::workflow_graph::workflow_graph_from_program(&artifact.canonical_ir);
    let Some(process) = graph.process(process_name) else {
        return TraceLanguageExecutionMap::default();
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    append_trace_workflow_subgraph(&process.body, &mut nodes, &mut edges);
    TraceLanguageExecutionMap { nodes, edges }
}

pub fn trace_lashlang_main_map(artifact: &lashlang::ModuleArtifact) -> TraceLanguageExecutionMap {
    let graph =
        lash_typescript::workflow_graph::workflow_graph_from_program(&artifact.canonical_ir);
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    append_trace_workflow_subgraph(&graph.main, &mut nodes, &mut edges);
    TraceLanguageExecutionMap { nodes, edges }
}

fn append_trace_workflow_subgraph(
    graph: &lashlang::WorkflowSubgraph,
    nodes: &mut Vec<TraceLanguageExecutionMapNode>,
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
            if nodes
                .iter()
                .any(|candidate| candidate.id == node.id.as_str() && candidate.site == *site)
            {
                continue;
            }
            nodes.push(TraceLanguageExecutionMapNode {
                id: node.id.to_string(),
                site: site.clone(),
                kind: site.kind.clone(),
                label: site.label.clone(),
                label_metadata: label_metadata.clone(),
            });
        }
        if let lashlang::WorkflowNodeKind::Container(container) = &node.kind {
            for (_, child) in container.child_subgraphs() {
                append_trace_workflow_subgraph(child, nodes, edges);
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
        | TraceLanguageExecutionPayload::NodeCompleted { node_id, .. }
        | TraceLanguageExecutionPayload::NodeFailed { node_id, .. }
        | TraceLanguageExecutionPayload::BranchSelected { node_id, .. } => Some(node_id),
        TraceLanguageExecutionPayload::ChildStarted { parent_node_id, .. } => Some(parent_node_id),
        TraceLanguageExecutionPayload::ExecutionStarted { .. }
        | TraceLanguageExecutionPayload::ExecutionFinished { .. } => None,
    }
}
