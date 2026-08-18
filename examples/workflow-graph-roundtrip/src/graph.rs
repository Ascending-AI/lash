use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    Expr, ProcessParam, VariableVersion, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowEdgeKind, WorkflowEffectKind, WorkflowGraph, WorkflowListComprehensionClause,
    WorkflowNode, WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowSubgraph,
    WorkflowTerminalKind, canonical_assign_target_source, canonical_expression_source,
    format_type_expr, parse_expression,
};
use serde_json::json;

use crate::{
    ChildGroup, EdgeData, EditableComprehensionClause, EditableValue, ExpectedArgumentType,
    FlowEdge, FlowNode, GraphRoots, NodeData, RenderErrorResponse, TypeDiagnostic, TypedVariable,
    ValidateRequest, ValidateResponse, ValidationKind, WorkflowDocument,
};

mod process;

use process::{
    editable_process_param, editable_process_signal, process_from_data, seeded_process_body,
};

pub(crate) fn validate_fragment(request: ValidateRequest) -> ValidateResponse {
    let result = match request.kind {
        ValidationKind::Expression => parse_expression(&request.text)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        ValidationKind::AssignmentTarget => parse_assignment_target_fragment(&request.text)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        ValidationKind::Identifier => {
            parse_assignment_target_fragment(&request.text).and_then(|target| {
                target.is_simple().then_some(()).ok_or_else(|| {
                    "expected an identifier without field or index access".to_string()
                })
            })
        }
    };
    match result {
        Ok(()) => ValidateResponse::valid(),
        Err(message) => ValidateResponse::invalid(request.kind, message),
    }
}

pub(crate) fn document_from_graph(
    version: u64,
    source: String,
    graph: WorkflowGraph,
) -> WorkflowDocument {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut roots = GraphRoots {
        main: node_ids(&graph.main),
        ..GraphRoots::default()
    };
    flatten_subgraph(&graph.main, "main", None, &mut nodes, &mut edges);
    for declaration in &graph.declarations {
        let WorkflowDeclaration::Process(process) = declaration else {
            continue;
        };
        let process_id = process.id.to_string();
        let scope = format!("process:{process_id}");
        let children = node_ids(&process.body);
        roots.processes.push(process_id.clone());
        nodes.push(FlowNode {
            id: process_id.clone(),
            node_type: "process".to_string(),
            parent_id: None,
            data: NodeData {
                kind: "process".to_string(),
                subkind: None,
                title: process.display_name.clone(),
                name: Some(process.name.clone()),
                params: process.params.iter().map(editable_process_param).collect(),
                signals: process
                    .signals
                    .iter()
                    .map(editable_process_signal)
                    .collect(),
                description: process.description.clone(),
                name_source: name_source(process.name_source),
                operation: None,
                effect: None,
                terminal_kind: None,
                fields: BTreeMap::new(),
                binding: None,
                target: None,
                expression: None,
                condition: None,
                iterable: None,
                clauses: Vec::new(),
                source: None,
                children: vec![ChildGroup {
                    slot: "body".to_string(),
                    scope: scope.clone(),
                    node_ids: children,
                }],
                available_vars: process
                    .params
                    .iter()
                    .map(|param| TypedVariable {
                        name: param.name.to_string(),
                        variable_type: format_type_expr(&param.ty),
                    })
                    .collect(),
                expected_arg_types: Vec::new(),
                diagnostics: Vec::new(),
            },
        });
        flatten_subgraph(
            &process.body,
            &scope,
            Some(process_id),
            &mut nodes,
            &mut edges,
        );
    }
    WorkflowDocument {
        schema_version: graph.schema_version,
        facet_schema_version: graph.facet_schema_version,
        version,
        source,
        nodes,
        edges,
        roots,
    }
}

pub(crate) fn graph_from_document(
    document: WorkflowDocument,
    baseline: &WorkflowGraph,
) -> Result<WorkflowGraph, RenderErrorResponse> {
    let mut seen = BTreeSet::new();
    for node in &document.nodes {
        if !seen.insert(node.id.as_str()) {
            return Err(RenderErrorResponse::document(
                format!("duplicate flow node id `{}`", node.id),
                json!({ "nodeId": node.id }),
            ));
        }
    }
    let nodes = document
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let edges = document.edges.iter().fold(
        BTreeMap::<&str, Vec<&FlowEdge>>::new(),
        |mut by_scope, edge| {
            by_scope
                .entry(edge.data.scope.as_str())
                .or_default()
                .push(edge);
            by_scope
        },
    );
    let baseline_nodes = baseline
        .nodes()
        .map(|node| (node.id.to_string(), node.clone()))
        .collect::<BTreeMap<_, _>>();
    let baseline_processes = baseline
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => {
                Some((process.id.to_string(), process.clone()))
            }
            WorkflowDeclaration::Type(_) | WorkflowDeclaration::Function(_) => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut graph = baseline.clone();
    graph.schema_version = document.schema_version;
    graph.facet_schema_version = None;
    graph.main = build_subgraph(
        "main",
        &document.roots.main,
        &nodes,
        &baseline_nodes,
        &edges,
    )?;
    let mut declarations = baseline
        .declarations
        .iter()
        .filter(|declaration| matches!(declaration, WorkflowDeclaration::Type(_)))
        .cloned()
        .collect::<Vec<_>>();
    for process_id in &document.roots.processes {
        let flow_process = nodes.get(process_id.as_str()).copied().ok_or_else(|| {
            RenderErrorResponse::document(
                format!("missing process container `{process_id}`"),
                json!({ "processId": process_id }),
            )
        })?;
        if flow_process.data.kind != "process" {
            return Err(RenderErrorResponse::invalid_node_payload(
                process_id,
                "a process root needs `data.kind` set to `process`",
            ));
        }
        let is_new = !baseline_processes.contains_key(process_id);
        let mut process = process_from_data(
            process_id,
            &flow_process.data,
            baseline_processes.get(process_id).cloned(),
        )?;
        process.body = rebuild_process_body(
            process_id,
            &flow_process.data,
            &nodes,
            &baseline_nodes,
            &edges,
            is_new,
            &process.params,
        )?;
        declarations.push(WorkflowDeclaration::Process(process));
    }
    graph.declarations = declarations;
    Ok(graph)
}

fn rebuild_process_body(
    process_id: &str,
    data: &NodeData,
    flow_nodes: &BTreeMap<&str, &FlowNode>,
    baseline_nodes: &BTreeMap<String, WorkflowNode>,
    flow_edges: &BTreeMap<&str, Vec<&FlowEdge>>,
    is_new: bool,
    params: &[ProcessParam],
) -> Result<WorkflowSubgraph, RenderErrorResponse> {
    let body = data.children.iter().find(|child| child.slot == "body");
    match body {
        Some(body) => {
            if is_new && body.node_ids.is_empty() {
                Ok(seeded_process_body(process_id, params))
            } else {
                build_subgraph(
                    &body.scope,
                    &body.node_ids,
                    flow_nodes,
                    baseline_nodes,
                    flow_edges,
                )
            }
        }
        None if is_new => Ok(seeded_process_body(process_id, params)),
        None => Err(RenderErrorResponse::document(
            format!("process container `{process_id}` is missing its body"),
            json!({ "processId": process_id, "child": "body" }),
        )),
    }
}

pub(crate) fn reconcile_node_ids(
    posted: &WorkflowDocument,
    reprojected: &WorkflowDocument,
) -> BTreeMap<String, String> {
    let reprojected_by_location = nodes_by_structural_location(reprojected);
    nodes_by_structural_location(posted)
        .into_iter()
        .filter_map(|(location, posted_id)| {
            reprojected_by_location
                .get(&location)
                .cloned()
                .map(|reprojected_id| (posted_id, reprojected_id))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum StructuralRoot {
    Main,
    Process(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StructuralLocation {
    root: StructuralRoot,
    path: Vec<(String, usize)>,
}

fn nodes_by_structural_location(
    document: &WorkflowDocument,
) -> BTreeMap<StructuralLocation, String> {
    let nodes = document
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut locations = BTreeMap::new();
    collect_group_locations(
        &nodes,
        StructuralRoot::Main,
        &[],
        "main",
        &document.roots.main,
        &mut locations,
    );
    for (process_index, process_id) in document.roots.processes.iter().enumerate() {
        let root = StructuralRoot::Process(process_index);
        locations.insert(
            StructuralLocation {
                root: root.clone(),
                path: Vec::new(),
            },
            process_id.clone(),
        );
        if let Some(process) = nodes.get(process_id.as_str()) {
            for child in &process.data.children {
                collect_group_locations(
                    &nodes,
                    root.clone(),
                    &[],
                    &child.slot,
                    &child.node_ids,
                    &mut locations,
                );
            }
        }
    }
    locations
}

fn collect_group_locations(
    nodes: &BTreeMap<&str, &FlowNode>,
    root: StructuralRoot,
    parent_path: &[(String, usize)],
    slot: &str,
    node_ids: &[String],
    locations: &mut BTreeMap<StructuralLocation, String>,
) {
    for (index, node_id) in node_ids.iter().enumerate() {
        let mut path = parent_path.to_vec();
        path.push((slot.to_string(), index));
        locations.insert(
            StructuralLocation {
                root: root.clone(),
                path: path.clone(),
            },
            node_id.clone(),
        );
        if let Some(node) = nodes.get(node_id.as_str()) {
            for child in &node.data.children {
                collect_group_locations(
                    nodes,
                    root.clone(),
                    &path,
                    &child.slot,
                    &child.node_ids,
                    locations,
                );
            }
        }
    }
}

fn flatten_subgraph(
    graph: &WorkflowSubgraph,
    scope: &str,
    parent_id: Option<String>,
    nodes: &mut Vec<FlowNode>,
    edges: &mut Vec<FlowEdge>,
) {
    for edge in &graph.edges {
        edges.push(flow_edge(edge, scope));
    }
    for node in &graph.nodes {
        let id = node.id.to_string();
        let child_groups = child_groups(node);
        nodes.push(FlowNode {
            id: id.clone(),
            node_type: node_kind(node).to_string(),
            parent_id: parent_id.clone(),
            data: node_data(node, child_groups.clone()),
        });
        for child in child_groups {
            if let Some(subgraph) = child_subgraph(node, &child.slot) {
                flatten_subgraph(subgraph, &child.scope, Some(id.clone()), nodes, edges);
            }
        }
    }
}

fn node_data(node: &WorkflowNode, children: Vec<ChildGroup>) -> NodeData {
    let (operation, effect) = match &node.kind {
        WorkflowNodeKind::Call { operation, .. } => (Some(operation.clone()), None),
        WorkflowNodeKind::Effect { effect, .. } => (None, Some(effect_name(effect).to_string())),
        _ => (None, None),
    };
    let source = match &node.kind {
        WorkflowNodeKind::Opaque { source } => Some(source.clone()),
        _ => None,
    };
    let binding = match &node.kind {
        WorkflowNodeKind::Data { binding, .. }
        | WorkflowNodeKind::Call { binding, .. }
        | WorkflowNodeKind::Effect { binding, .. }
        | WorkflowNodeKind::Computation { binding, .. }
        | WorkflowNodeKind::Container(WorkflowContainer::If { binding, .. })
        | WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { binding, .. }) => {
            binding.clone()
        }
        WorkflowNodeKind::Container(WorkflowContainer::For { binding, .. }) => {
            Some(binding.clone())
        }
        _ => None,
    };
    let target = match &node.kind {
        WorkflowNodeKind::StateUpdate { target, .. } => Some(target.clone()),
        _ => None,
    };
    let expression = match &node.kind {
        WorkflowNodeKind::Data { expression, .. }
        | WorkflowNodeKind::Computation { expression, .. }
        | WorkflowNodeKind::StateUpdate { expression, .. } => Some(expression.clone()),
        WorkflowNodeKind::Terminal { expression, .. } => terminal_value(expression),
        _ => None,
    };
    let condition = match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If { condition, .. })
        | WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) => {
            Some(condition.clone())
        }
        _ => None,
    };
    let iterable = match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::For { iterable, .. }) => {
            Some(iterable.clone())
        }
        _ => None,
    };
    let clauses = match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { clauses, .. }) => {
            clauses.iter().map(editable_clause).collect()
        }
        _ => Vec::new(),
    };
    NodeData {
        kind: node_kind(node).to_string(),
        subkind: node_subkind(node).map(str::to_string),
        title: node.name.clone(),
        name: None,
        params: Vec::new(),
        signals: Vec::new(),
        description: node.description.clone(),
        name_source: name_source(node.name_source),
        operation,
        effect,
        terminal_kind: terminal_kind(node).map(str::to_string),
        fields: editable_fields(node),
        binding,
        target,
        expression,
        condition,
        iterable,
        clauses,
        source,
        children,
        available_vars: node
            .type_facets
            .as_ref()
            .map(|facets| {
                facets
                    .available_variables
                    .iter()
                    .map(|variable| TypedVariable {
                        name: variable.name.clone(),
                        variable_type: format_type_expr(&variable.ty),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        expected_arg_types: node
            .type_facets
            .as_ref()
            .map(|facets| {
                facets
                    .expected_arguments
                    .iter()
                    .map(|argument| ExpectedArgumentType {
                        slot: argument.slot.clone(),
                        expected_type: format_type_expr(&argument.ty),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        diagnostics: node
            .type_facets
            .as_ref()
            .map(|facets| {
                facets
                    .diagnostics
                    .iter()
                    .map(|diagnostic| TypeDiagnostic {
                        node_id: diagnostic.node_id.to_string(),
                        kind: diagnostic.kind.clone(),
                        message: diagnostic.message.clone(),
                        span: diagnostic.span,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn terminal_value(expression: &str) -> Option<String> {
    let expression = parse_expression(expression).ok()?;
    let value = match expression {
        Expr::Finish(value) | Expr::Fail(value) => value,
        _ => return None,
    };
    canonical_expression_source(&value).ok()
}

fn editable_clause(clause: &WorkflowListComprehensionClause) -> EditableComprehensionClause {
    match clause {
        WorkflowListComprehensionClause::For { binding, iterable } => {
            EditableComprehensionClause::For {
                binding: binding.clone(),
                iterable: iterable.clone(),
            }
        }
        WorkflowListComprehensionClause::If { condition } => EditableComprehensionClause::If {
            condition: condition.clone(),
        },
    }
}

fn flow_edge(edge: &WorkflowEdge, scope: &str) -> FlowEdge {
    let (kind, variable, version) = match &edge.kind {
        WorkflowEdgeKind::Sequence => ("sequence".to_string(), None, None),
        WorkflowEdgeKind::DataDependency { variable, version } => {
            ("data".to_string(), Some(variable.clone()), Some(*version))
        }
    };
    FlowEdge {
        id: edge.id.clone(),
        source: edge.from.to_string(),
        target: edge.to.to_string(),
        data: EdgeData {
            kind,
            scope: scope.to_string(),
            variable,
            version,
        },
    }
}

fn child_groups(node: &WorkflowNode) -> Vec<ChildGroup> {
    let group = |slot: &str, graph: &WorkflowSubgraph| ChildGroup {
        slot: slot.to_string(),
        scope: format!("container:{}:{slot}", node.id),
        node_ids: node_ids(graph),
    };
    match &node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If {
            then_graph,
            else_graph,
            ..
        }) => [
            then_graph.as_deref().map(|graph| group("then", graph)),
            else_graph.as_deref().map(|graph| group("else", graph)),
        ]
        .into_iter()
        .flatten()
        .collect(),
        WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) => body
            .as_deref()
            .map(|graph| vec![group("body", graph)])
            .unwrap_or_default(),
        WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) => body
            .as_deref()
            .map(|graph| vec![group("body", graph)])
            .unwrap_or_default(),
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { element, .. }) => {
            element
                .as_deref()
                .map(|graph| vec![group("element", graph)])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn child_subgraph<'a>(node: &'a WorkflowNode, slot: &str) -> Option<&'a WorkflowSubgraph> {
    match (&node.kind, slot) {
        (WorkflowNodeKind::Container(WorkflowContainer::If { then_graph, .. }), "then") => {
            then_graph.as_deref()
        }
        (WorkflowNodeKind::Container(WorkflowContainer::If { else_graph, .. }), "else") => {
            else_graph.as_deref()
        }
        (WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }), "body") => {
            body.as_deref()
        }
        (WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }), "body") => {
            body.as_deref()
        }
        (
            WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { element, .. }),
            "element",
        ) => element.as_deref(),
        _ => None,
    }
}

fn build_subgraph(
    scope: &str,
    node_ids: &[String],
    flow_nodes: &BTreeMap<&str, &FlowNode>,
    baseline_nodes: &BTreeMap<String, WorkflowNode>,
    flow_edges: &BTreeMap<&str, Vec<&FlowEdge>>,
) -> Result<WorkflowSubgraph, RenderErrorResponse> {
    let mut nodes = Vec::with_capacity(node_ids.len());
    for id in node_ids {
        let flow = flow_nodes.get(id.as_str()).copied().ok_or_else(|| {
            RenderErrorResponse::document(
                format!("scope `{scope}` references missing flow node `{id}`"),
                json!({ "scope": scope, "nodeId": id }),
            )
        })?;
        let (mut node, is_new) = match baseline_nodes.get(id).cloned() {
            Some(mut node) => {
                apply_editable_data(&mut node, &flow.data)?;
                (node, false)
            }
            None => (node_from_flow_data(id, &flow.data)?, true),
        };
        rebuild_children(
            &mut node,
            &flow.data.children,
            flow_nodes,
            baseline_nodes,
            flow_edges,
            is_new,
        )?;
        nodes.push(node);
    }
    let edges = flow_edges
        .get(scope)
        .into_iter()
        .flatten()
        .map(|edge| workflow_edge(edge))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkflowSubgraph { nodes, edges })
}

fn workflow_edge(edge: &FlowEdge) -> Result<WorkflowEdge, RenderErrorResponse> {
    let parse_id = |id: &str| {
        serde_json::from_value::<WorkflowNodeId>(serde_json::Value::String(id.to_string())).map_err(
            |error| {
                RenderErrorResponse::document(
                    format!("invalid workflow node id `{id}`: {error}"),
                    json!({ "nodeId": id }),
                )
            },
        )
    };
    let kind = match edge.data.kind.as_str() {
        "sequence" => WorkflowEdgeKind::Sequence,
        "data" => WorkflowEdgeKind::DataDependency {
            variable: edge.data.variable.clone().ok_or_else(|| {
                RenderErrorResponse::document(
                    format!("data edge `{}` is missing `data.variable`", edge.id),
                    json!({ "edgeId": edge.id }),
                )
            })?,
            version: edge.data.version.ok_or_else(|| {
                RenderErrorResponse::document(
                    format!("data edge `{}` is missing `data.version`", edge.id),
                    json!({ "edgeId": edge.id }),
                )
            })?,
        },
        kind => {
            return Err(RenderErrorResponse::document(
                format!("edge `{}` has unknown kind `{kind}`", edge.id),
                json!({ "edgeId": edge.id, "kind": kind }),
            ));
        }
    };
    Ok(WorkflowEdge {
        id: edge.id.clone(),
        from: parse_id(&edge.source)?,
        to: parse_id(&edge.target)?,
        kind,
    })
}

fn rebuild_children(
    node: &mut WorkflowNode,
    children: &[ChildGroup],
    flow_nodes: &BTreeMap<&str, &FlowNode>,
    baseline_nodes: &BTreeMap<String, WorkflowNode>,
    flow_edges: &BTreeMap<&str, Vec<&FlowEdge>>,
    allow_empty_default: bool,
) -> Result<(), RenderErrorResponse> {
    let build = |slot: &str| -> Result<Option<Box<WorkflowSubgraph>>, RenderErrorResponse> {
        children
            .iter()
            .find(|child| child.slot == slot)
            .map(|child| {
                build_subgraph(
                    &child.scope,
                    &child.node_ids,
                    flow_nodes,
                    baseline_nodes,
                    flow_edges,
                )
                .map(Box::new)
            })
            .transpose()
            .map(|graph| {
                graph.or_else(|| allow_empty_default.then(|| Box::new(WorkflowSubgraph::default())))
            })
    };
    match &mut node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If {
            then_graph,
            else_graph,
            ..
        }) => {
            *then_graph = build("then")?;
            *else_graph = build("else")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::For { body, .. }) => {
            *body = build("body")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::While { body, .. }) => {
            *body = build("body")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { element, .. }) => {
            *element = build("element")?;
            if allow_empty_default
                && element
                    .as_deref()
                    .is_some_and(|element| element.nodes.is_empty())
            {
                return Err(RenderErrorResponse::invalid_node_payload(
                    &node.id.to_string(),
                    "container body cannot be empty; a list comprehension needs one element node",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn node_from_flow_data(id: &str, data: &NodeData) -> Result<WorkflowNode, RenderErrorResponse> {
    let mut outputs = Vec::new();
    let kind = match data.kind.as_str() {
        "data" => WorkflowNodeKind::Data {
            binding: data.binding.clone(),
            expression: editable_expression(id, data)?,
        },
        "call" => {
            let (expression, parsed) = editable_call_expression(id, data)?;
            let operation = first_receiver_operation(&parsed)
                .expect("editable_call_expression guarantees a receiver call");
            WorkflowNodeKind::Call {
                binding: data.binding.clone(),
                operation: operation.to_string(),
                expression,
            }
        }
        "effect" => {
            let (expression, parsed) = editable_effect_expression(id, data)?;
            let effect = match data.effect.as_deref() {
                Some(effect) => parse_effect_kind(id, effect)?,
                None => effect_kind(&parsed).ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(
                        id,
                        "an effect node needs `data.effect` or a recognized effect expression",
                    )
                })?,
            };
            WorkflowNodeKind::Effect {
                binding: data.binding.clone(),
                effect,
                expression,
            }
        }
        "state_update" => {
            let target = required_text(id, data.target.as_ref(), "target")?;
            let target = parse_assignment_target(id, &target)?;
            outputs.push(VariableVersion {
                variable: target.root.to_string(),
                version: 0,
            });
            WorkflowNodeKind::StateUpdate {
                target: canonical_assign_target_source(&target).map_err(|error| {
                    RenderErrorResponse::invalid_assignment_target(id, "target", error.to_string())
                })?,
                expression: editable_expression(id, data)?,
            }
        }
        "computation" => WorkflowNodeKind::Computation {
            binding: data.binding.clone(),
            expression: editable_expression(id, data)?,
        },
        "terminal" => {
            let terminal = parse_terminal_kind(id, data.terminal_kind.as_deref())?;
            let expression = terminal_expression(id, &terminal, data.expression.as_ref())?;
            WorkflowNodeKind::Terminal {
                terminal,
                expression,
            }
        }
        "opaque" => WorkflowNodeKind::Opaque {
            source: required_text(id, data.source.as_ref(), "source")?,
        },
        "container" => WorkflowNodeKind::Container(match data.subkind.as_deref() {
            Some("if") => WorkflowContainer::If {
                binding: data.binding.clone(),
                condition: required_text(id, data.condition.as_ref(), "condition")?,
                then_is_block: true,
                else_is_block: true,
                then_graph: Some(Box::new(WorkflowSubgraph::default())),
                else_graph: Some(Box::new(WorkflowSubgraph::default())),
            },
            Some("while") => WorkflowContainer::While {
                condition: required_text(id, data.condition.as_ref(), "condition")?,
                body: Some(Box::new(WorkflowSubgraph::default())),
            },
            Some("for") => WorkflowContainer::For {
                binding: required_text(id, data.binding.as_ref(), "binding")?,
                iterable: required_text(id, data.iterable.as_ref(), "iterable")?,
                body: Some(Box::new(WorkflowSubgraph::default())),
            },
            Some("comprehension") => WorkflowContainer::ListComprehension {
                binding: data.binding.clone(),
                clauses: data.clauses.iter().map(workflow_clause).collect(),
                element: Some(Box::new(WorkflowSubgraph::default())),
            },
            subkind => {
                return Err(RenderErrorResponse::unknown_node_kind(
                    id,
                    "container",
                    subkind,
                ));
            }
        }),
        kind => {
            return Err(RenderErrorResponse::unknown_node_kind(
                id,
                kind,
                data.subkind.as_deref(),
            ));
        }
    };
    Ok(WorkflowNode {
        id: workflow_node_id(id),
        name: data.title.clone(),
        description: data.description.clone(),
        name_source: parse_name_source(&data.name_source),
        kind,
        available_variables: Vec::new(),
        type_facets: None,
        outputs,
        execution_sites: Vec::new(),
        source_span: None,
    })
}

fn editable_expression(id: &str, data: &NodeData) -> Result<String, RenderErrorResponse> {
    editable_parsed_expression(id, data).map(|(source, _)| source)
}

fn editable_parsed_expression(
    id: &str,
    data: &NodeData,
) -> Result<(String, Expr), RenderErrorResponse> {
    let source = required_text(id, data.expression.as_ref(), "expression")?;
    let mut expression = parse_expression(&source).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    apply_fields(id, &mut expression, &data.fields)?;
    let source = canonical_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

fn editable_call_expression(
    id: &str,
    data: &NodeData,
) -> Result<(String, Expr), RenderErrorResponse> {
    let has_authored_expression = data.expression.is_some();
    let mut expression = match &data.expression {
        Some(source) => parse_expression(source).map_err(|error| {
            RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
        })?,
        None => {
            let operation = required_text(id, data.operation.as_ref(), "operation")?;
            parse_expression(&format!("await display.{operation}({{}})?")).map_err(|error| {
                RenderErrorResponse::invalid_expression(id, "operation", error.to_string())
            })?
        }
    };
    if let Some(operation) = &data.operation {
        *receiver_operation_mut(&mut expression).ok_or_else(|| {
            RenderErrorResponse::invalid_expression(
                id,
                "expression",
                "a call node needs a receiver call expression",
            )
        })? = operation.clone().into();
    }
    if first_receiver_operation(&expression).is_none() {
        return Err(RenderErrorResponse::invalid_expression(
            id,
            "expression",
            "a call node needs a receiver call expression",
        ));
    }
    if !has_authored_expression || !data.fields.is_empty() {
        apply_fields(id, &mut expression, &data.fields)?;
    }
    let source = canonical_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

fn editable_effect_expression(
    id: &str,
    data: &NodeData,
) -> Result<(String, Expr), RenderErrorResponse> {
    if data.expression.is_some() {
        return editable_parsed_expression(id, data);
    }
    let effect = required_text(id, data.effect.as_ref(), "effect")?;
    let expression = match effect.as_str() {
        "sleep" => Expr::SleepFor(Box::new(
            data.fields
                .get("duration")
                .ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(id, "sleep needs a `duration` field")
                })?
                .to_expr(id, "fields.duration")?,
        )),
        "wait_signal" => {
            let Some(EditableValue::String(signal)) = data.fields.get("signal") else {
                return Err(RenderErrorResponse::invalid_node_payload(
                    id,
                    "wait_signal needs a string `signal` field",
                ));
            };
            Expr::WaitSignal {
                name: signal.clone().into(),
            }
        }
        _ => {
            return Err(RenderErrorResponse::invalid_node_payload(
                id,
                format!("new effect `{effect}` needs an expression"),
            ));
        }
    };
    let source = canonical_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

fn parse_terminal_kind(
    id: &str,
    terminal_kind: Option<&str>,
) -> Result<WorkflowTerminalKind, RenderErrorResponse> {
    match terminal_kind {
        Some("finish") => Ok(WorkflowTerminalKind::Finish),
        Some("fail") => Ok(WorkflowTerminalKind::Fail),
        Some(kind) => Err(RenderErrorResponse::invalid_node_payload(
            id,
            format!("unknown terminal kind `{kind}`"),
        )),
        None => Err(RenderErrorResponse::invalid_node_payload(
            id,
            "a terminal node needs `data.terminalKind`",
        )),
    }
}

fn terminal_expression(
    id: &str,
    terminal: &WorkflowTerminalKind,
    value: Option<&String>,
) -> Result<String, RenderErrorResponse> {
    let value = required_text(id, value, "expression")?;
    let value = parse_expression(&value).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    let expression = match terminal {
        WorkflowTerminalKind::Finish => Expr::Finish(Box::new(value)),
        WorkflowTerminalKind::Fail => Expr::Fail(Box::new(value)),
    };
    canonical_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })
}

fn parse_assignment_target(
    id: &str,
    source: &str,
) -> Result<lashlang::AssignTarget, RenderErrorResponse> {
    parse_assignment_target_fragment(source)
        .map_err(|message| RenderErrorResponse::invalid_assignment_target(id, "target", message))
}

fn parse_assignment_target_fragment(source: &str) -> Result<lashlang::AssignTarget, String> {
    let expression =
        parse_expression(&format!("{source} = null")).map_err(|error| error.to_string())?;
    match expression {
        Expr::Assign { target, .. } => Ok(target),
        _ => Err("expected an assignment target".to_string()),
    }
}

fn workflow_node_id(id: &str) -> WorkflowNodeId {
    serde_json::from_value(serde_json::Value::String(id.to_string())).unwrap_or_else(|_| {
        let placeholder = format!(
            "new-node-{}",
            id.as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        serde_json::from_value(serde_json::Value::String(placeholder))
            .expect("generated workflow node ids are valid")
    })
}

fn first_receiver_operation(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
        Expr::Await(inner) => match inner.as_ref() {
            Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
            Expr::ResultUnwrap(inner) => match inner.as_ref() {
                Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
                _ => None,
            },
            _ => None,
        },
        Expr::ResultUnwrap(inner) => match inner.as_ref() {
            Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
            Expr::Await(inner) => match inner.as_ref() {
                Expr::ReceiverCall { operation, .. } => Some(operation.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn receiver_operation_mut(expression: &mut Expr) -> Option<&mut compact_str::CompactString> {
    match expression {
        Expr::ReceiverCall { operation, .. } => Some(operation),
        Expr::Await(inner) | Expr::ResultUnwrap(inner) => receiver_operation_mut(inner),
        _ => None,
    }
}

fn effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
    if let Expr::ResultUnwrap(inner) = expression {
        return direct_effect_kind(inner);
    }
    direct_effect_kind(expression)
}

fn direct_effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
    match expression {
        Expr::StartProcess(_) => Some(WorkflowEffectKind::StartProcess),
        Expr::Await(_) => Some(WorkflowEffectKind::AwaitJoin),
        Expr::SignalRun { .. } => Some(WorkflowEffectKind::SignalRun),
        Expr::WaitSignal { .. } => Some(WorkflowEffectKind::WaitSignal),
        Expr::SleepFor(_) | Expr::SleepUntil(_) => Some(WorkflowEffectKind::Sleep),
        Expr::Cancel(_) => Some(WorkflowEffectKind::Cancel),
        Expr::Print(_) => Some(WorkflowEffectKind::Print),
        Expr::Yield(_) => Some(WorkflowEffectKind::Yield),
        Expr::Wake(_) => Some(WorkflowEffectKind::Wake),
        Expr::Break => Some(WorkflowEffectKind::Break),
        Expr::Continue => Some(WorkflowEffectKind::Continue),
        _ => None,
    }
}

fn parse_effect_kind(id: &str, effect: &str) -> Result<WorkflowEffectKind, RenderErrorResponse> {
    match effect {
        "start_process" => Ok(WorkflowEffectKind::StartProcess),
        "await_join" => Ok(WorkflowEffectKind::AwaitJoin),
        "signal_run" => Ok(WorkflowEffectKind::SignalRun),
        "wait_signal" => Ok(WorkflowEffectKind::WaitSignal),
        "sleep" => Ok(WorkflowEffectKind::Sleep),
        "cancel" => Ok(WorkflowEffectKind::Cancel),
        "print" => Ok(WorkflowEffectKind::Print),
        "yield" => Ok(WorkflowEffectKind::Yield),
        "wake" => Ok(WorkflowEffectKind::Wake),
        "break" => Ok(WorkflowEffectKind::Break),
        "continue" => Ok(WorkflowEffectKind::Continue),
        _ => Err(RenderErrorResponse::invalid_node_payload(
            id,
            format!("unknown effect kind `{effect}`"),
        )),
    }
}

fn apply_editable_data(
    node: &mut WorkflowNode,
    data: &NodeData,
) -> Result<(), RenderErrorResponse> {
    let node_id = node.id.to_string();
    node.name = data.title.clone();
    node.description = data.description.clone();
    node.name_source = parse_name_source(&data.name_source);
    if let WorkflowNodeKind::Opaque { source } = &mut node.kind
        && let Some(updated) = &data.source
    {
        *source = updated.clone();
    }
    match &mut node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        } => {
            *binding = data.binding.clone();
            *expression = canonical_editable_expression(&node_id, data.expression.as_ref())?;
        }
        WorkflowNodeKind::Call {
            binding,
            operation,
            expression,
        } => {
            *binding = data.binding.clone();
            let mut parsed = parse_stored_expression(&node_id, expression)?;
            let edited_operation = required_text(&node_id, data.operation.as_ref(), "operation")?;
            *receiver_operation_mut(&mut parsed).ok_or_else(|| {
                RenderErrorResponse::invalid_node_payload(
                    &node_id,
                    "stored call expression has no receiver operation",
                )
            })? = edited_operation.into();
            apply_fields(&node_id, &mut parsed, &data.fields)?;
            *operation = first_receiver_operation(&parsed)
                .expect("receiver operation was updated")
                .to_string();
            *expression = canonical_expression_source(&parsed).map_err(|error| {
                RenderErrorResponse::invalid_expression(&node_id, "expression", error.to_string())
            })?;
        }
        WorkflowNodeKind::Effect {
            binding,
            effect,
            expression,
        } => {
            *binding = data.binding.clone();
            let (edited_expression, parsed) = editable_effect_expression(&node_id, data)?;
            *effect = effect_kind(&parsed).ok_or_else(|| {
                RenderErrorResponse::invalid_node_payload(
                    &node_id,
                    "edited expression is not a recognized effect",
                )
            })?;
            *expression = edited_expression;
        }
        WorkflowNodeKind::Computation {
            binding,
            expression,
        } => {
            *binding = data.binding.clone();
            *expression = required_text(&node_id, data.expression.as_ref(), "expression")?;
        }
        WorkflowNodeKind::StateUpdate { target, expression } => {
            *target = required_text(&node_id, data.target.as_ref(), "target")?;
            *expression = required_text(&node_id, data.expression.as_ref(), "expression")?;
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            *terminal = parse_terminal_kind(&node_id, data.terminal_kind.as_deref())?;
            *expression = terminal_expression(&node_id, terminal, data.expression.as_ref())?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::If {
            binding, condition, ..
        }) => {
            *binding = data.binding.clone();
            *condition = required_text(&node_id, data.condition.as_ref(), "condition")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::For {
            binding, iterable, ..
        }) => {
            *binding = required_text(&node_id, data.binding.as_ref(), "binding")?;
            *iterable = required_text(&node_id, data.iterable.as_ref(), "iterable")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) => {
            *condition = required_text(&node_id, data.condition.as_ref(), "condition")?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension {
            binding,
            clauses,
            ..
        }) => {
            *binding = data.binding.clone();
            *clauses = data.clauses.iter().map(workflow_clause).collect();
        }
        _ => {}
    }
    Ok(())
}

fn canonical_editable_expression(
    node_id: &str,
    expression: Option<&String>,
) -> Result<String, RenderErrorResponse> {
    let expression = required_text(node_id, expression, "expression")?;
    let expression = parse_expression(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(node_id, "expression", error.to_string())
    })?;
    canonical_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(node_id, "expression", error.to_string())
    })
}

fn parse_stored_expression(node_id: &str, source: &str) -> Result<Expr, RenderErrorResponse> {
    parse_expression(source).map_err(|error| {
        RenderErrorResponse::document(
            format!("stored expression for node `{node_id}` did not parse: {error}"),
            json!({ "nodeId": node_id, "reason": error.to_string() }),
        )
    })
}

fn required_text(
    node_id: &str,
    value: Option<&String>,
    field: &'static str,
) -> Result<String, RenderErrorResponse> {
    value.cloned().ok_or_else(|| {
        RenderErrorResponse::document(
            format!("flow node `{node_id}` is missing `data.{field}`"),
            json!({ "nodeId": node_id, "field": field }),
        )
    })
}

fn workflow_clause(clause: &EditableComprehensionClause) -> WorkflowListComprehensionClause {
    match clause {
        EditableComprehensionClause::For { binding, iterable } => {
            WorkflowListComprehensionClause::For {
                binding: binding.clone(),
                iterable: iterable.clone(),
            }
        }
        EditableComprehensionClause::If { condition } => WorkflowListComprehensionClause::If {
            condition: condition.clone(),
        },
    }
}

fn editable_fields(node: &WorkflowNode) -> BTreeMap<String, EditableValue> {
    let expression = match &node.kind {
        WorkflowNodeKind::Data { expression, .. }
        | WorkflowNodeKind::Call { expression, .. }
        | WorkflowNodeKind::Effect { expression, .. }
        | WorkflowNodeKind::Terminal { expression, .. } => expression,
        _ => return BTreeMap::new(),
    };
    let Ok(expression) = parse_expression(expression) else {
        return BTreeMap::new();
    };
    if let Some(fields) = receiver_fields(&expression) {
        return fields
            .iter()
            .map(|(name, value)| (name.to_string(), EditableValue::from_expr(value)))
            .collect();
    }
    match &expression {
        Expr::SleepFor(value) | Expr::SleepUntil(value) => {
            BTreeMap::from([("duration".to_string(), EditableValue::from_expr(value))])
        }
        Expr::WaitSignal { name } => BTreeMap::from([(
            "signal".to_string(),
            EditableValue::String(name.to_string()),
        )]),
        _ => BTreeMap::new(),
    }
}

fn apply_fields(
    node_id: &str,
    expression: &mut Expr,
    fields: &BTreeMap<String, EditableValue>,
) -> Result<(), RenderErrorResponse> {
    if let Some(entries) = receiver_fields_mut(expression) {
        entries.clear();
        for (name, value) in fields {
            let value = value.to_expr(node_id, &format!("fields.{name}"))?;
            entries.push((name.clone().into(), value));
        }
        return Ok(());
    }
    match expression {
        Expr::SleepFor(value) | Expr::SleepUntil(value) => {
            if let Some(duration) = fields.get("duration") {
                **value = duration.to_expr(node_id, "fields.duration")?;
            }
        }
        Expr::WaitSignal { name } => {
            if let Some(EditableValue::String(signal)) = fields.get("signal") {
                *name = signal.clone().into();
            }
        }
        _ => {}
    }
    Ok(())
}

fn receiver_fields(expression: &Expr) -> Option<&Vec<(compact_str::CompactString, Expr)>> {
    match expression {
        Expr::ReceiverCall { args, .. } => args.first().and_then(|arg| match arg {
            Expr::Record(fields) => Some(fields),
            _ => None,
        }),
        Expr::Await(inner) | Expr::ResultUnwrap(inner) => receiver_fields(inner),
        _ => None,
    }
}

fn receiver_fields_mut(
    expression: &mut Expr,
) -> Option<&mut Vec<(compact_str::CompactString, Expr)>> {
    match expression {
        Expr::ReceiverCall { args, .. } => args.first_mut().and_then(|arg| match arg {
            Expr::Record(fields) => Some(fields),
            _ => None,
        }),
        Expr::Await(inner) | Expr::ResultUnwrap(inner) => receiver_fields_mut(inner),
        _ => None,
    }
}

impl EditableValue {
    fn from_expr(expression: &Expr) -> Self {
        Self::literal_from_expr(expression).unwrap_or_else(|| {
            Self::Expr(
                canonical_expression_source(expression)
                    .expect("a parsed editable expression must remain sourceable"),
            )
        })
    }

    fn literal_from_expr(expression: &Expr) -> Option<Self> {
        match expression {
            Expr::Null => Some(Self::Null),
            Expr::Bool(value) => Some(Self::Bool(*value)),
            Expr::Number(value) => Some(Self::Number(*value)),
            Expr::String(value) => Some(Self::String(value.to_string())),
            Expr::List(values) => values
                .iter()
                .map(Self::literal_from_expr)
                .collect::<Option<Vec<_>>>()
                .map(Self::List),
            Expr::Record(entries) => entries
                .iter()
                .map(|(key, value)| Some((key.to_string(), Self::literal_from_expr(value)?)))
                .collect::<Option<BTreeMap<_, _>>>()
                .map(Self::Object),
            _ => None,
        }
    }

    fn to_expr(&self, node_id: &str, field: &str) -> Result<Expr, RenderErrorResponse> {
        Ok(match self {
            Self::Null => Expr::Null,
            Self::Bool(value) => Expr::Bool(*value),
            Self::Number(value) => Expr::Number(*value),
            Self::String(value) => Expr::String(value.clone().into()),
            Self::List(values) => Expr::List(
                values
                    .iter()
                    .map(|value| value.to_expr(node_id, field))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Expr(source) => parse_expression(source).map_err(|error| {
                RenderErrorResponse::invalid_expression(node_id, field, error.to_string())
            })?,
            Self::Object(entries) => Expr::Record(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone().into(), value.to_expr(node_id, field)?)))
                    .collect::<Result<_, RenderErrorResponse>>()?,
            ),
        })
    }
}

fn node_ids(graph: &WorkflowSubgraph) -> Vec<String> {
    graph.nodes.iter().map(|node| node.id.to_string()).collect()
}

fn node_kind(node: &WorkflowNode) -> &'static str {
    match node.kind {
        WorkflowNodeKind::Data { .. } => "data",
        WorkflowNodeKind::Call { .. } => "call",
        WorkflowNodeKind::Effect { .. } => "effect",
        WorkflowNodeKind::Computation { .. } => "computation",
        WorkflowNodeKind::StateUpdate { .. } => "state_update",
        WorkflowNodeKind::Terminal { .. } => "terminal",
        WorkflowNodeKind::Container(_) => "container",
        WorkflowNodeKind::Opaque { .. } => "opaque",
    }
}

fn node_subkind(node: &WorkflowNode) -> Option<&'static str> {
    match node.kind {
        WorkflowNodeKind::Container(WorkflowContainer::If { .. }) => Some("if"),
        WorkflowNodeKind::Container(WorkflowContainer::While { .. }) => Some("while"),
        WorkflowNodeKind::Container(WorkflowContainer::For { .. }) => Some("for"),
        WorkflowNodeKind::Container(WorkflowContainer::ListComprehension { .. }) => {
            Some("comprehension")
        }
        _ => None,
    }
}

fn effect_name(effect: &lashlang::WorkflowEffectKind) -> &'static str {
    use lashlang::WorkflowEffectKind;
    match effect {
        WorkflowEffectKind::StartProcess => "start_process",
        WorkflowEffectKind::AwaitJoin => "await_join",
        WorkflowEffectKind::SignalRun => "signal_run",
        WorkflowEffectKind::WaitSignal => "wait_signal",
        WorkflowEffectKind::Sleep => "sleep",
        WorkflowEffectKind::Cancel => "cancel",
        WorkflowEffectKind::Print => "print",
        WorkflowEffectKind::Yield => "yield",
        WorkflowEffectKind::Wake => "wake",
        WorkflowEffectKind::Break => "break",
        WorkflowEffectKind::Continue => "continue",
    }
}

fn terminal_kind(node: &WorkflowNode) -> Option<&'static str> {
    match &node.kind {
        WorkflowNodeKind::Terminal {
            terminal: WorkflowTerminalKind::Finish,
            ..
        } => Some("finish"),
        WorkflowNodeKind::Terminal {
            terminal: WorkflowTerminalKind::Fail,
            ..
        } => Some("fail"),
        _ => None,
    }
}

fn name_source(source: WorkflowNodeNameSource) -> String {
    match source {
        WorkflowNodeNameSource::Label => "label",
        WorkflowNodeNameSource::Derived => "derived",
    }
    .to_string()
}

fn parse_name_source(source: &str) -> WorkflowNodeNameSource {
    if source == "label" {
        WorkflowNodeNameSource::Label
    } else {
        WorkflowNodeNameSource::Derived
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promoted_constructs_flatten_and_rebuild_as_typed_nodes() {
        let input = r#"type State = { count: int }

process worker() {
  finish 1
}

schema = Type { state: State }
runs = [start worker(), start worker()]
state = { count: 0 }
state.count = 1
while state.count < 2 {
  state.count = state.count + 1
}
for item in [1, 2] {
  state.count = item
  introduced = item
}
finish [state, introduced]
"#;
        let graph = lashlang::workflow_graph_from_source(input).expect("project promoted graph");
        let source = lashlang::workflow_graph_to_source(&graph).expect("render promoted graph");
        let document = document_from_graph(1, source.clone(), graph.clone());

        for kind in [
            "data",
            "computation",
            "state_update",
            "container",
            "terminal",
        ] {
            assert!(
                document.nodes.iter().any(|node| node.node_type == kind),
                "missing flattened {kind} node"
            );
        }
        assert!(!document.nodes.iter().any(|node| node.node_type == "opaque"));
        assert!(document.nodes.iter().any(|node| {
            node.data.title == "while"
                && node.data.condition.as_deref() == Some("(state.count < 2)")
                && node.data.children.iter().any(|child| child.slot == "body")
        }));
        assert!(document.nodes.iter().any(|node| {
            node.node_type == "state_update"
                && node.data.target.as_deref() == Some("state.count")
                && node.data.expression.is_some()
        }));
        assert!(document.nodes.iter().any(|node| {
            node.node_type == "computation"
                && node.data.binding.as_deref() == Some("runs")
                && node.data.expression.as_deref() == Some("[start worker(), start worker()]")
        }));

        let rebuilt = graph_from_document(document, &graph).expect("rebuild promoted graph");
        assert_eq!(
            lashlang::workflow_graph_to_source(&rebuilt).expect("render rebuilt graph"),
            source
        );
        assert_eq!(
            lashlang::workflow_graph_from_source(&source).expect("reproject rebuilt source"),
            graph
        );
    }
}
