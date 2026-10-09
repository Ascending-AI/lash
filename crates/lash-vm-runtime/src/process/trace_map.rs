use std::collections::{BTreeMap, BTreeSet};

use lash_trace::{
    TraceBranchMembership, TraceBranchSelection, TraceLabelMetadata, TraceLanguageExecutionMap,
    TraceLanguageExecutionMapEdge, TraceLanguageExecutionMapNode,
};

/// Returns the current static execution map for one process definition.
///
/// Hosts may request this independently of the live trace stream, so a
/// resumed segment does not need to repeat `ExecutionStarted` to make its
/// definition discoverable.
pub fn trace_lashlang_process_map(
    graph: &lash_vm::WorkflowGraph,
    process_name: &str,
) -> Option<TraceLanguageExecutionMap> {
    let process = graph.process(process_name)?;
    Some(trace_workflow_subgraph(&process.body))
}

pub fn trace_lashlang_main_map(graph: &lash_vm::WorkflowGraph) -> TraceLanguageExecutionMap {
    trace_workflow_subgraph(&graph.main)
}

/// One map entry per execution site: a node with two calls has two.
type TraceNodeKey = (String, lash_sansio::WorkflowExecutionSite);

fn trace_workflow_subgraph(graph: &lash_vm::WorkflowSubgraph) -> TraceLanguageExecutionMap {
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
    graph: &lash_vm::WorkflowSubgraph,
    branch_memberships: &[TraceBranchMembership],
    nodes: &mut BTreeMap<TraceNodeKey, TraceLanguageExecutionMapNode>,
    edges: &mut Vec<TraceLanguageExecutionMapEdge>,
) {
    for node in &graph.nodes {
        let label_metadata =
            (node.name_source == lash_vm::WorkflowNodeNameSource::Label).then(|| {
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
            nodes.insert((candidate.id.clone(), candidate.site.clone()), candidate);
        }
        if let lash_vm::WorkflowNodeKind::Container(container) = &node.kind {
            for (slot, child) in container.child_subgraphs() {
                let mut child_memberships = branch_memberships.to_vec();
                if let lash_vm::WorkflowContainer::If { .. } = container {
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
            lash_vm::WorkflowEdgeKind::Sequence => "sequence".to_string(),
            lash_vm::WorkflowEdgeKind::DataDependency { variable, version } => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_vm::testing::ast_builders as b;
    use lash_vm::testing::harness::link_labeled;

    fn call(operation: &str) -> lash_vm::Expr {
        b::module_call(
            &["tools"],
            operation,
            vec![b::record(vec![("value", b::var("n"))])],
        )
    }

    fn body() -> lash_vm::Expr {
        b::block(vec![
            b::assign("n", b::num(1.0)),
            b::finish(b::list(vec![call("echo"), call("err")])),
        ])
    }

    fn assert_map_contract(map: &TraceLanguageExecutionMap, graph: &lash_vm::WorkflowSubgraph) {
        let keys = map
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), &node.site))
            .collect::<BTreeSet<_>>();
        assert_eq!(keys.len(), map.nodes.len(), "map keys are sites");
        let graph_sites = graph
            .nodes
            .iter()
            .flat_map(|node| node.execution_sites.iter())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            map.nodes
                .iter()
                .map(|node| &node.site)
                .collect::<BTreeSet<_>>(),
            graph_sites,
            "the map lists the document's sites, one entry each"
        );

        let terminal = map
            .nodes
            .iter()
            .find(|node| node.kind == lash_sansio::ExecutionNodeKind::Terminal)
            .expect("terminal site");
        // `finish([echo(), err()])`: the two calls are two sites of the
        // statement's node, never one entry standing for both (FIG-5575).
        let operations = map
            .nodes
            .iter()
            .filter(|node| {
                node.id == terminal.id
                    && node.kind == lash_sansio::ExecutionNodeKind::ResourceOperation
            })
            .collect::<Vec<_>>();
        assert_eq!(
            operations
                .iter()
                .map(|node| node.label.as_str())
                .collect::<Vec<_>>(),
            ["echo", "err"],
            "each call keeps its own entry, in evaluation order"
        );
        assert_ne!(operations[0].site.site_path, operations[1].site.site_path);

        let producer = graph
            .nodes
            .iter()
            .find(|node| matches!(node.kind, lash_vm::WorkflowNodeKind::Data { .. }))
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
    fn main_map_is_keyed_by_site_and_closed_over_edges() {
        let linked = link_labeled(b::program(match body() {
            lash_vm::Expr::Block(expressions) => expressions,
            _ => unreachable!(),
        }));
        let graph = lash_vm::workflow_graph_from_artifact(&linked.artifact);
        let map = trace_lashlang_main_map(&lash_vm::workflow_graph_from_artifact(&linked.artifact));
        assert_map_contract(&map, &graph.main);
    }

    #[test]
    fn process_map_is_keyed_by_site_and_closed_over_edges() {
        let linked = link_labeled(b::module(
            vec![b::process("worker", Vec::new(), body())],
            Vec::new(),
        ));
        let graph = lash_vm::workflow_graph_from_artifact(&linked.artifact);
        let process = graph.process("worker").expect("worker graph");
        let map = trace_lashlang_process_map(
            &lash_vm::workflow_graph_from_artifact(&linked.artifact),
            "worker",
        )
        .expect("worker map");
        assert_map_contract(&map, &process.body);
    }
}
