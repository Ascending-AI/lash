use std::collections::{BTreeMap, BTreeSet};

use lash::typescript::workflow_graph::{
    typescript_assign_target_source, typescript_expression_source, typescript_for_of_source,
    typescript_process_wrapper,
};
use lash::vm::ir::{
    Expr, ProcessParam, VariableVersion, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowNode, WorkflowNodeId, WorkflowNodeKind, WorkflowSubgraph, format_type_expr,
    workflow_call_from_ir, workflow_call_to_ir, workflow_effect_from_ir, workflow_effect_to_ir,
};
use lash::workflow::{WorkflowBodyForm, WorkflowCatch, WorkflowGraph};
use serde_json::json;

use crate::{
    ChildGroup, EdgeData, ExpectedArgumentType, FlowEdge, FlowNode, GraphRoots, NodeBody,
    NodeContainer, NodeData, NodeName, RenderErrorResponse, TypeDiagnostic, TypedVariable,
    ValidateRequest, ValidateResponse, ValidationKind, WorkflowDocument,
};

mod editable;
mod process;

use editable::*;

use process::{editable_process_param, process_from_data, seeded_process_body};

pub(crate) fn validate_fragment(request: ValidateRequest) -> ValidateResponse {
    // A fragment is validated in the scope it will be edited in: the host sends
    // the node's available variables, and the dialect resolves against exactly
    // those names.
    let scope = FragmentScope::new(
        request.available_vars.iter().cloned().collect(),
        &GraphScope::default(),
    );
    let result = match request.kind {
        ValidationKind::Expression => parse_fragment(&request.text, &scope)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        ValidationKind::AssignmentTarget => parse_assignment_target_fragment(&request.text, &scope)
            .map(|_| ())
            .map_err(|error| error.to_string()),
        ValidationKind::Identifier => parse_assignment_target_fragment(&request.text, &scope)
            .and_then(|target| {
                target.is_simple().then_some(()).ok_or_else(|| {
                    "expected an identifier without field or index access".to_string()
                })
            }),
    };
    match result {
        Ok(()) => ValidateResponse::valid(),
        Err(message) => ValidateResponse::invalid(request.kind, message),
    }
}

/// The wire document of `graph`. `source` is the TypeScript lens's view of
/// it, or the reason the lens has none; the document is complete either way.
pub(crate) fn document_from_graph(
    version: u64,
    source: Result<String, String>,
    graph: WorkflowGraph,
) -> WorkflowDocument {
    let (source, source_unavailable) = match source {
        Ok(source) => (source, None),
        Err(reason) => (String::new(), Some(reason)),
    };
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut roots = GraphRoots {
        main: node_ids(&graph.main),
        ..GraphRoots::default()
    };
    let graph_scope = GraphScope::main(process_bindings(&graph));
    flatten_subgraph(
        &graph.main,
        "main",
        None,
        &mut nodes,
        &mut edges,
        &graph_scope,
    );
    for declaration in &graph.declarations {
        let WorkflowDeclaration::Process(process) = declaration else {
            continue;
        };
        let process_id = process.id.to_string();
        let scope = format!("process:{process_id}");
        let children = node_ids(&process.body);
        roots.processes.push(process_id.to_string());
        nodes.push(FlowNode {
            id: process_id.to_string(),
            node_type: "process".to_string(),
            parent_id: None,
            data: NodeData {
                name: NodeName::projected(
                    process.name_source,
                    process.display_name.clone(),
                    process.description.clone(),
                ),
                body: NodeBody::Process {
                    process_name: Some(process.name.clone()),
                    params: process.params.iter().map(editable_process_param).collect(),
                    children: vec![ChildGroup {
                        slot: "body".to_string(),
                        scope: scope.clone(),
                        node_ids: children,
                    }],
                },
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
            Some(process_id.to_string()),
            &mut nodes,
            &mut edges,
            &graph_scope.in_process(),
        );
    }
    WorkflowDocument {
        schema_version: graph.schema_version,
        definition: graph.source_identity.clone(),
        facet_schema_version: graph.facet_schema_version,
        version,
        source,
        source_unavailable,
        not_admitted: None,
        nodes,
        edges,
        roots,
    }
}

/// The document `document` asks for, as a graph over `baseline`: a node the
/// document still names keeps its id, and keeps its typed IR exactly unless
/// its form was edited. The result states what to reach, not a program to
/// trust: [`crate::edits::apply_document`] turns it into typed edits of the
/// baseline's draft, and lash derives everything else again.
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
            WorkflowDeclaration::Function(_) => None,
        })
        .collect::<BTreeMap<_, _>>();
    let graph_scope = GraphScope::main(document_process_bindings(&document, baseline));
    let mut graph = baseline.clone();
    graph.schema_version = document.schema_version;
    graph.facet_schema_version = None;
    graph.main = build_subgraph(
        "main",
        &document.roots.main,
        &nodes,
        &baseline_nodes,
        &edges,
        &graph_scope,
    )?;
    let mut declarations = baseline
        .declarations
        .iter()
        .filter(|declaration| matches!(declaration, WorkflowDeclaration::Function(_)))
        .cloned()
        .collect::<Vec<_>>();
    for process_id in &document.roots.processes {
        let flow_process = nodes.get(process_id.as_str()).copied().ok_or_else(|| {
            RenderErrorResponse::document(
                format!("missing process container `{process_id}`"),
                json!({ "processId": process_id }),
            )
        })?;
        if flow_process.data.kind() != "process" {
            return Err(RenderErrorResponse::invalid_node_payload(
                process_id,
                "a process root needs `data.kind` set to `process`",
            ));
        }
        let is_new = !baseline_processes.contains_key(process_id);
        let baseline_process = baseline_processes.get(process_id);
        let mut process =
            process_from_data(process_id, &flow_process.data, baseline_process.cloned())?;
        // The wrapper passes the authored parameters to the run body, so a
        // process that is new or whose parameters changed takes the wrapper
        // the dialect lowers for the parameters it has now.
        if baseline_process.is_none_or(|baseline| baseline.params != process.params) {
            let hidden = match &process.origin {
                lash::vm::ir::ProcessOrigin::Lifted { hidden_params, .. } => {
                    *hidden_params as usize
                }
                lash::vm::ir::ProcessOrigin::Declared => 0,
            };
            let authored = process.params.len().saturating_sub(hidden);
            process.wrapper = Some(Box::new(typescript_process_wrapper(
                &process.params[..authored],
            )));
        }
        let rebuilt = rebuild_process_body(
            &RebuiltProcess {
                id: process_id,
                params: &process.params,
                is_new,
            },
            &flow_process.data,
            &nodes,
            &baseline_nodes,
            &edges,
            &graph_scope.in_process(),
        )?;
        process.body = rebuilt;
        declarations.push(WorkflowDeclaration::Process(process));
    }
    graph.declarations = declarations;
    bind_declared_processes(&mut graph);
    Ok(graph)
}

/// Give every declared process its module binding.
///
/// A TypeScript process is `const name = async (..) => {..}`: the module body
/// holds the binding and the declaration hangs off it, so a graph whose main
/// subgraph never binds a declared process cannot be rendered. A host that adds
/// a process container gets that binding here rather than having to know the
/// module shape.
///
/// A *lifted* process is the exception (ADR 0095): its authored arrow already
/// travels inline in the statement that binds or passes it, and the lens
/// deliberately renders no module declaration for it. Synthesising a
/// `name = name` binding for one emits a read of a name nothing declares, so
/// lifted processes are left to the statement that already carries them.
fn bind_declared_processes(graph: &mut WorkflowGraph) {
    // A renamed process leaves its old module binding pointing at a name no
    // declaration answers to, and that binding cannot be rendered. The binding
    // is derived from the process, not authored, so a stale one is dropped and
    // the new name is bound below.
    let declared = graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process.name.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    graph.main.nodes.retain(|node| match &node.kind {
        WorkflowNodeKind::Data {
            binding: Some(binding),
            expression: Expr::ProcessRef { process },
        } if binding.is_simple() && binding.root == *process => declared.contains(process.as_str()),
        _ => true,
    });
    let bound = graph
        .main
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            WorkflowNodeKind::Data {
                binding: Some(binding),
                expression: Expr::ProcessRef { process },
            } if binding.is_simple() && binding.root == *process => Some(process.to_string()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let missing = graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process)
                if !bound.contains(&process.name) && !process.origin.is_lifted() =>
            {
                Some(process.name.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for (offset, name) in missing.into_iter().enumerate() {
        let node = WorkflowNode {
            id: workflow_node_id(&format!("process-binding:{name}")),
            name: "data".to_string(),
            description: None,
            name_source: lash::vm::ir::WorkflowNodeNameSource::Derived,
            kind: WorkflowNodeKind::Data {
                binding: Some(lash::vm::ir::AssignTarget::variable(name.clone().into())),
                expression: Expr::ProcessRef {
                    process: name.into(),
                },
            },
            available_variables: Vec::new(),
            type_facets: None,
            outputs: Vec::new(),
            execution_sites: Vec::new(),
            source_span: None,
        };
        graph.main.nodes.insert(offset, node);
    }
}

/// The freshly declared process a body is being rebuilt for.
struct RebuiltProcess<'a> {
    id: &'a str,
    params: &'a [ProcessParam],
    is_new: bool,
}

fn rebuild_process_body(
    process: &RebuiltProcess<'_>,
    data: &NodeData,
    flow_nodes: &BTreeMap<&str, &FlowNode>,
    baseline_nodes: &BTreeMap<String, WorkflowNode>,
    flow_edges: &BTreeMap<&str, Vec<&FlowEdge>>,
    graph_scope: &GraphScope,
) -> Result<WorkflowSubgraph, RenderErrorResponse> {
    let RebuiltProcess {
        id: process_id,
        params,
        is_new,
    } = *process;
    let body = data.children().iter().find(|child| child.slot == "body");
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
                    graph_scope,
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

fn flatten_subgraph(
    graph: &WorkflowSubgraph,
    scope: &str,
    parent_id: Option<String>,
    nodes: &mut Vec<FlowNode>,
    edges: &mut Vec<FlowEdge>,
    graph_scope: &GraphScope,
) {
    for edge in &graph.edges {
        edges.push(flow_edge(edge, scope));
    }
    for node in &graph.nodes {
        let id = node.id.to_string();
        let child_groups = child_groups(node);
        let data = node_data(node, child_groups.clone(), graph_scope);
        nodes.push(FlowNode {
            id: id.clone(),
            node_type: data.kind().to_string(),
            parent_id: parent_id.clone(),
            data,
        });
        if let WorkflowNodeKind::Container(container) = &node.kind {
            for (slot, subgraph) in container.child_subgraphs() {
                let scope = format!("container:{}:{slot}", node.id);
                flatten_subgraph(
                    subgraph,
                    &scope,
                    Some(id.clone()),
                    nodes,
                    edges,
                    graph_scope,
                );
            }
        }
    }
}

fn node_data(node: &WorkflowNode, children: Vec<ChildGroup>, graph_scope: &GraphScope) -> NodeData {
    let expression = match &node.kind {
        WorkflowNodeKind::Data { expression, .. }
        | WorkflowNodeKind::Computation { expression, .. }
        | WorkflowNodeKind::StateUpdate { expression, .. } => {
            typescript_expression_source(expression).ok()
        }
        WorkflowNodeKind::Call {
            receiver,
            operation,
            arguments,
            result_steps,
            ..
        } => typescript_expression_source(&workflow_call_to_ir(
            receiver,
            operation,
            arguments,
            result_steps,
        ))
        .ok(),
        WorkflowNodeKind::Effect {
            effect,
            arguments,
            result_steps,
            ..
        } => workflow_effect_to_ir(*effect, arguments, result_steps)
            .and_then(|expression| typescript_expression_source(&expression).ok()),
        WorkflowNodeKind::Terminal { expression, .. } => {
            terminal_value(expression, &FragmentScope::of_node(node, graph_scope))
        }
        WorkflowNodeKind::Throw { value } => typescript_expression_source(value).ok(),
        WorkflowNodeKind::Container(_) => None,
    };
    let binding = |target: &Option<lash::vm::ir::AssignTarget>| {
        target
            .as_ref()
            .and_then(|target| typescript_assign_target_source(target).ok())
    };
    let fields = editable_fields(node, graph_scope);
    let body = match &node.kind {
        WorkflowNodeKind::Data {
            binding: target, ..
        } => NodeBody::Data {
            binding: binding(target),
            expression,
            fields,
        },
        WorkflowNodeKind::Call {
            binding: target,
            operation,
            ..
        } => NodeBody::Call {
            binding: binding(target),
            operation: Some(operation.clone()),
            receiver: None,
            expression,
            fields,
        },
        WorkflowNodeKind::Effect {
            binding: target,
            effect,
            ..
        } => NodeBody::Effect {
            binding: binding(target),
            effect: *effect,
            expression,
            fields,
        },
        WorkflowNodeKind::Computation {
            binding: target, ..
        } => NodeBody::Computation {
            binding: binding(target),
            expression,
            fields,
        },
        WorkflowNodeKind::StateUpdate { target, .. } => NodeBody::StateUpdate {
            target: typescript_assign_target_source(target).ok(),
            expression,
            fields,
        },
        WorkflowNodeKind::Terminal { terminal, .. } => NodeBody::Terminal {
            terminal_kind: terminal.clone(),
            expression,
        },
        WorkflowNodeKind::Throw { .. } => NodeBody::Throw { expression },
        WorkflowNodeKind::Container(container) => NodeBody::Container(match container {
            WorkflowContainer::If {
                binding: target,
                condition,
                ..
            } => NodeContainer::If {
                binding: binding(target),
                condition: typescript_expression_source(condition).ok(),
                children,
            },
            WorkflowContainer::While { condition, .. } => NodeContainer::While {
                condition: typescript_expression_source(condition).ok(),
                children,
            },
            WorkflowContainer::For { iterable, .. } => NodeContainer::For {
                binding: container.loop_element_name().map(str::to_string),
                iterable: typescript_expression_source(typescript_for_of_source(iterable)).ok(),
                children,
            },
            WorkflowContainer::Try {
                binding: target,
                catch,
                finally,
                ..
            } => NodeContainer::Try {
                binding: binding(target),
                catch_binding: catch.as_ref().map(|catch| catch.binding.clone()),
                finally: finally.is_some(),
                children,
            },
            WorkflowContainer::Scope {
                binding: target, ..
            } => NodeContainer::Scope {
                binding: binding(target),
                children,
            },
        }),
    };
    NodeData {
        name: NodeName::projected(
            node.name_source,
            node.name.clone(),
            node.description.clone(),
        ),
        body,
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
                        slot: argument.slot.to_string(),
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
                        kind: diagnostic_kind_text(diagnostic.kind),
                        classification: diagnostic.classification,
                        slot: diagnostic.slot.as_ref().map(ToString::to_string),
                        message: diagnostic.message.clone(),
                        span: diagnostic.span,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The value text a terminal node exposes for editing.
///
/// A cell terminal is `finish(value)` or `fail(value)`; a process terminal is
/// the `return value;` that ends the run body, which the language will not
/// parse in expression position, so it is read through the process door.
fn terminal_value(expression: &Expr, _scope: &FragmentScope) -> Option<String> {
    let value = match expression {
        Expr::Finish(value) | Expr::Fail(value) | Expr::FunctionReturn(value) => value,
        _ => return None,
    };
    typescript_expression_source(value).ok()
}

fn flow_edge(edge: &WorkflowEdge, scope: &str) -> FlowEdge {
    FlowEdge {
        id: edge.id.clone(),
        source: edge.from.to_string(),
        target: edge.to.to_string(),
        data: EdgeData {
            body: edge.kind.clone(),
            scope: scope.to_string(),
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
        WorkflowNodeKind::Container(container) => container
            .child_subgraphs()
            .map(|(slot, graph)| group(slot, graph))
            .collect(),
        _ => Vec::new(),
    }
}

fn build_subgraph(
    scope: &str,
    node_ids: &[String],
    flow_nodes: &BTreeMap<&str, &FlowNode>,
    baseline_nodes: &BTreeMap<String, WorkflowNode>,
    flow_edges: &BTreeMap<&str, Vec<&FlowEdge>>,
    graph_scope: &GraphScope,
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
                // A node whose form the host did not touch keeps its typed
                // IR exactly: nothing is read back through a text field.
                if !same_form(&flow.data, &node_data(&node, Vec::new(), graph_scope)) {
                    apply_editable_data(&mut node, &flow.data, graph_scope)?;
                }
                (node, false)
            }
            None => (node_from_flow_data(id, &flow.data, graph_scope)?, true),
        };
        rebuild_children(
            &mut node,
            flow.data.children(),
            flow_nodes,
            baseline_nodes,
            flow_edges,
            is_new,
            graph_scope,
        )?;
        nodes.push(node);
    }
    let edges = flow_edges
        .get(scope)
        .into_iter()
        .flatten()
        .map(|edge| workflow_edge(edge))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkflowSubgraph {
        form: WorkflowBodyForm::default(),
        nodes,
        edges,
    })
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
    let kind = edge.data.body.clone();
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
    graph_scope: &GraphScope,
) -> Result<(), RenderErrorResponse> {
    let node_id = node.id.to_string();
    let build = |slot: &str| -> Result<WorkflowSubgraph, RenderErrorResponse> {
        let Some(child) = children.iter().find(|child| child.slot == slot) else {
            // A clause the form just switched on starts empty.
            if allow_empty_default || matches!(slot, "catch" | "finally") {
                return Ok(WorkflowSubgraph::default());
            }
            return Err(RenderErrorResponse::document(
                format!("node `{node_id}` is missing required child `{slot}`"),
                json!({ "nodeId": node_id, "child": slot }),
            ));
        };
        build_subgraph(
            &child.scope,
            &child.node_ids,
            flow_nodes,
            baseline_nodes,
            flow_edges,
            graph_scope,
        )
    };
    if let WorkflowNodeKind::Container(container) = &mut node.kind {
        for (slot, graph) in container.child_subgraphs_mut() {
            let rebuilt = build(slot)?;
            graph.nodes = rebuilt.nodes;
            graph.edges = rebuilt.edges;
        }
    }
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "editable_call_expression / the updated call expression above each guarantee a \
              receiver operation on the parsed call they just produced"
)]
fn node_from_flow_data(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<WorkflowNode, RenderErrorResponse> {
    let mut outputs = Vec::new();
    let kind = match &data.body {
        NodeBody::Data { .. } => WorkflowNodeKind::Data {
            binding: editable_binding(
                id,
                data.binding().as_ref(),
                &FragmentScope::of_data(data, graph_scope),
            )?,
            expression: editable_expression(id, data, graph_scope)?,
        },
        NodeBody::Call { .. } => {
            let (_, parsed) = editable_call_expression(id, data, graph_scope)?;
            let (receiver, operation, arguments, result_steps) = workflow_call_from_ir(&parsed)
                .expect("editable_call_expression guarantees a receiver call");
            WorkflowNodeKind::Call {
                binding: editable_binding(
                    id,
                    data.binding().as_ref(),
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                receiver,
                operation,
                arguments,
                result_steps,
            }
        }
        NodeBody::Effect { effect, .. } => {
            let (_, parsed) = editable_effect_expression(id, data, graph_scope)?;
            let (parsed_effect, arguments, result_steps) = workflow_effect_from_ir(&parsed)
                .ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(
                        id,
                        "an effect node needs a recognized effect expression",
                    )
                })?;
            let effect = *effect;
            if effect != parsed_effect {
                return Err(RenderErrorResponse::invalid_node_payload(
                    id,
                    "effect kind does not match its expression",
                ));
            }
            WorkflowNodeKind::Effect {
                binding: editable_binding(
                    id,
                    data.binding().as_ref(),
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                effect,
                arguments,
                result_steps,
            }
        }
        NodeBody::StateUpdate { .. } => {
            let target = required_text(id, data.target().as_ref(), "target")?;
            let target =
                parse_assignment_target(id, &target, &FragmentScope::of_data(data, graph_scope))?;
            outputs.push(VariableVersion {
                variable: target.root.to_string(),
                version: 0,
            });
            WorkflowNodeKind::StateUpdate {
                target,
                expression: editable_expression(id, data, graph_scope)?,
                update: None,
                pinned: None,
            }
        }
        NodeBody::Computation { .. } => WorkflowNodeKind::Computation {
            binding: editable_binding(
                id,
                data.binding().as_ref(),
                &FragmentScope::of_data(data, graph_scope),
            )?,
            expression: editable_expression(id, data, graph_scope)?,
        },
        NodeBody::Terminal { .. } => {
            let terminal = required_terminal_kind(id, data.terminal_kind())?;
            let expression = terminal_expression(
                id,
                &terminal,
                data.expression().as_ref(),
                &FragmentScope::of_data(data, graph_scope),
            )?;
            WorkflowNodeKind::Terminal {
                terminal,
                expression,
            }
        }
        NodeBody::Throw { .. } => WorkflowNodeKind::Throw {
            value: editable_expression(id, data, graph_scope)?,
        },
        NodeBody::Container(container) => WorkflowNodeKind::Container(match container {
            NodeContainer::If { .. } => WorkflowContainer::If {
                binding: editable_binding(
                    id,
                    data.binding().as_ref(),
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                condition: required_expression(
                    id,
                    data.condition().as_ref(),
                    "condition",
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                then_graph: Box::new(WorkflowSubgraph::default()),
                else_graph: Box::new(WorkflowSubgraph::default()),
            },
            NodeContainer::While { .. } => WorkflowContainer::While {
                binding: None,
                condition: required_expression(
                    id,
                    data.condition().as_ref(),
                    "condition",
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                body: Box::new(WorkflowSubgraph::default()),
            },
            NodeContainer::For { .. } => WorkflowContainer::For {
                binding: None,
                authored_element: None,
                element: required_text(id, data.binding().as_ref(), "binding")?,
                iterable: required_expression(
                    id,
                    data.iterable().as_ref(),
                    "iterable",
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                bind: None,
                body: Box::new(WorkflowSubgraph::default()),
            },
            NodeContainer::Try {
                catch_binding,
                finally,
                ..
            } => WorkflowContainer::Try {
                binding: editable_binding(
                    id,
                    data.binding().as_ref(),
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                body: Box::new(WorkflowSubgraph::default()),
                catch: catch_binding.as_ref().map(|binding| WorkflowCatch {
                    binding: binding.clone(),
                    body: Box::new(WorkflowSubgraph::default()),
                }),
                finally: finally.then(|| Box::new(WorkflowSubgraph::default())),
            },
            NodeContainer::Scope { .. } => WorkflowContainer::Scope {
                binding: editable_binding(
                    id,
                    data.binding().as_ref(),
                    &FragmentScope::of_data(data, graph_scope),
                )?,
                body: Box::new(WorkflowSubgraph::default()),
            },
        }),
        NodeBody::Process { .. } => {
            return Err(RenderErrorResponse::invalid_node_payload(
                id,
                "a process belongs in the process roots",
            ));
        }
    };
    Ok(WorkflowNode {
        id: workflow_node_id(id),
        name: data.name.title().to_string(),
        description: data.name.description().map(str::to_string),
        name_source: data.name.name_source(),
        kind,
        // The lens re-parses a node's editable text inside a wrapper that
        // declares the names the node can read, so a rebuilt node carries the
        // scope the host projected onto it. Only the names are used; the
        // echoed facet types are recomputed from the saved source.
        available_variables: data
            .available_vars
            .iter()
            .map(|variable| variable.name.clone())
            .collect(),
        type_facets: None,
        outputs,
        execution_sites: Vec::new(),
        source_span: None,
    })
}

#[expect(
    clippy::expect_used,
    reason = "the edited call expression was re-parsed from a receiver call, so its receiver \
              operation is present"
)]
fn apply_editable_data(
    node: &mut WorkflowNode,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<(), RenderErrorResponse> {
    let node_id = node.id.to_string();
    // The lens re-parses a node's editable text inside a wrapper declaring the
    // names the node may read, so an edited node keeps the scope the host was
    // shown. Only the names are taken; the echoed facet types are recomputed
    // from the saved source.
    for variable in &data.available_vars {
        if !node.available_variables.contains(&variable.name) {
            node.available_variables.push(variable.name.clone());
        }
    }
    let scope = FragmentScope::of_node(node, graph_scope);
    // Each field is read back only when the host changed it: a field left
    // as it was shown keeps the typed IR it shows.
    let shown = node_data(node, Vec::new(), graph_scope);
    let rebound = data.binding() != shown.binding();
    let reworded = data.expression() != shown.expression();
    node.name = data.name.title().to_string();
    node.description = data.name.description().map(str::to_string);
    node.name_source = data.name.name_source();
    match &mut node.kind {
        WorkflowNodeKind::Data {
            binding,
            expression,
        } => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if reworded {
                *expression =
                    canonical_editable_expression(&node_id, data.expression().as_ref(), &scope)?;
            }
        }
        WorkflowNodeKind::Call {
            binding,
            receiver,
            operation,
            arguments,
            result_steps,
        } => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if !reworded && data.operation() == shown.operation() && data.fields() == shown.fields()
            {
                return Ok(());
            }
            let mut parsed = workflow_call_to_ir(receiver, operation, arguments, result_steps);
            let edited_operation = required_text(&node_id, data.operation().as_ref(), "operation")?;
            // Switching an existing call to an operation of another receiver
            // rewrites more than the method name: the stored expression still
            // calls the receiver the node came from. Re-synthesize the call
            // from the node's own receiver when the two disagree, so the saved
            // source names the receiver the editor switched to (FIG-3179).
            if let Some(receiver) = data.receiver().as_deref()
                && receiver_call_receiver(&parsed).is_some_and(|current| current != receiver)
            {
                parsed = synthesize_receiver_call(&node_id, data, &edited_operation, &scope)?;
            }
            *receiver_operation_mut(&mut parsed).ok_or_else(|| {
                RenderErrorResponse::invalid_node_payload(
                    &node_id,
                    "stored call expression has no receiver operation",
                )
            })? = edited_operation.into();
            apply_fields(&node_id, &mut parsed, data.fields(), &scope)?;
            let (new_receiver, new_operation, new_arguments, new_result_steps) =
                workflow_call_from_ir(&parsed).expect("receiver operation was updated");
            *receiver = new_receiver;
            *operation = new_operation;
            *arguments = new_arguments;
            *result_steps = new_result_steps;
        }
        WorkflowNodeKind::Effect {
            binding,
            effect,
            arguments,
            result_steps,
        } => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if !reworded && data.effect() == shown.effect() && data.fields() == shown.fields() {
                return Ok(());
            }
            let (_, parsed) = editable_effect_expression(&node_id, data, graph_scope)?;
            let (new_effect, new_arguments, new_result_steps) = workflow_effect_from_ir(&parsed)
                .ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(
                        &node_id,
                        "edited expression is not a recognized effect",
                    )
                })?;
            *effect = new_effect;
            *arguments = new_arguments;
            *result_steps = new_result_steps;
        }
        WorkflowNodeKind::Computation {
            binding,
            expression,
        } => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if reworded {
                *expression = required_expression(
                    &node_id,
                    data.expression().as_ref(),
                    "expression",
                    &scope,
                )?;
            }
        }
        WorkflowNodeKind::StateUpdate {
            target, expression, ..
        } => {
            if data.target() != shown.target() {
                *target = parse_assignment_target(
                    &node_id,
                    &required_text(&node_id, data.target().as_ref(), "target")?,
                    &scope,
                )?;
            }
            if reworded {
                *expression = required_expression(
                    &node_id,
                    data.expression().as_ref(),
                    "expression",
                    &scope,
                )?;
            }
        }
        WorkflowNodeKind::Terminal {
            terminal,
            expression,
        } => {
            *terminal = required_terminal_kind(&node_id, data.terminal_kind())?;
            *expression =
                terminal_expression(&node_id, terminal, data.expression().as_ref(), &scope)?;
        }
        WorkflowNodeKind::Container(WorkflowContainer::If {
            binding, condition, ..
        }) => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if data.condition() != shown.condition() {
                *condition =
                    required_expression(&node_id, data.condition().as_ref(), "condition", &scope)?;
            }
        }
        WorkflowNodeKind::Container(container @ WorkflowContainer::For { .. }) => {
            // The document shows the loop as its TypeScript header: the name
            // the body reads and the source it iterates. Each is replaced
            // only when its text was edited, so an untouched loop keeps the
            // element binding and the snapshot the lowerer gave it.
            let shown_element = container.loop_element_name().map(str::to_string);
            let edited_element = required_text(&node_id, data.binding().as_ref(), "binding")?;
            let WorkflowContainer::For {
                element,
                iterable,
                bind,
                ..
            } = container
            else {
                unreachable!("the arm matched a loop")
            };
            if shown_element.as_deref() != Some(edited_element.as_str()) {
                *element = edited_element;
                *bind = None;
            }
            let shown_iterable =
                typescript_expression_source(typescript_for_of_source(iterable)).ok();
            if shown_iterable.as_ref() != data.iterable().as_ref() {
                *iterable =
                    required_expression(&node_id, data.iterable().as_ref(), "iterable", &scope)?;
            }
        }
        WorkflowNodeKind::Container(WorkflowContainer::While { condition, .. }) => {
            if data.condition() != shown.condition() {
                *condition =
                    required_expression(&node_id, data.condition().as_ref(), "condition", &scope)?;
            }
        }
        WorkflowNodeKind::Throw { value } => {
            if reworded {
                *value = required_expression(
                    &node_id,
                    data.expression().as_ref(),
                    "expression",
                    &scope,
                )?;
            }
        }
        WorkflowNodeKind::Container(WorkflowContainer::Try {
            binding,
            catch,
            finally,
            ..
        }) => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
            if let NodeBody::Container(NodeContainer::Try {
                catch_binding,
                finally: has_finally,
                ..
            }) = &data.body
            {
                let caught = catch.take().map(|catch| catch.body).unwrap_or_default();
                *catch = catch_binding.as_ref().map(|binding| WorkflowCatch {
                    binding: binding.clone(),
                    body: caught,
                });
                let last = finally.take().unwrap_or_default();
                *finally = has_finally.then_some(last);
            }
        }
        WorkflowNodeKind::Container(WorkflowContainer::Scope { binding, .. }) => {
            if rebound {
                *binding = editable_binding(&node_id, data.binding().as_ref(), &scope)?;
            }
        }
    }
    Ok(())
}

/// Whether two forms of one node say the same thing. Child membership and
/// the facets a client echoes back are not part of a node's own form.
fn same_form(submitted: &NodeData, projected: &NodeData) -> bool {
    let own = |data: &NodeData| {
        let mut value = serde_json::to_value(data).unwrap_or_default();
        if let Some(fields) = value.as_object_mut() {
            for derived in [
                "children",
                "availableVars",
                "expectedArgTypes",
                "diagnostics",
            ] {
                fields.remove(derived);
            }
        }
        value
    };
    own(submitted) == own(projected)
}

fn canonical_editable_expression(
    node_id: &str,
    expression: Option<&String>,
    scope: &FragmentScope,
) -> Result<Expr, RenderErrorResponse> {
    let expression = required_text(node_id, expression, "expression")?;
    let expression = parse_fragment(&expression, scope).map_err(|error| {
        RenderErrorResponse::invalid_expression(node_id, "expression", error.to_string())
    })?;
    Ok(expression)
}

fn required_expression(
    node_id: &str,
    source: Option<&String>,
    field: &'static str,
    scope: &FragmentScope,
) -> Result<Expr, RenderErrorResponse> {
    let source = required_text(node_id, source, field)?;
    parse_fragment(&source, scope)
        .map_err(|error| RenderErrorResponse::invalid_expression(node_id, field, error.to_string()))
}

fn editable_binding(
    node_id: &str,
    source: Option<&String>,
    scope: &FragmentScope,
) -> Result<Option<lash::vm::ir::AssignTarget>, RenderErrorResponse> {
    source
        .map(|source| parse_assignment_target(node_id, source, scope))
        .transpose()
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

fn diagnostic_kind_text(kind: lash::vm::ir::WorkflowDiagnosticKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "invalid_diagnostic_kind".to_string())
}

fn node_ids(graph: &WorkflowSubgraph) -> Vec<String> {
    graph.nodes.iter().map(|node| node.id.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document a save of `document` over `baseline` leaves: the typed
    /// edits the document means, applied to the baseline's draft.
    fn saved(document: WorkflowDocument, baseline: &WorkflowGraph) -> WorkflowGraph {
        let target = graph_from_document(document, baseline).expect("the document reads");
        let draft = lash::workflow::WorkflowDraft::open(baseline).expect("the baseline opens");
        let named = draft
            .opened()
            .map(|(handle, id)| (id.clone(), handle))
            .collect();
        crate::edits::apply_document(draft, &named, baseline, &target)
            .unwrap_or_else(|error| panic!("the document's edits apply: {error}"))
            .draft
            .document()
            .clone()
    }

    /// FIG-3630: the editor saves an admitted workflow whose process is a
    /// lifted literal after the author adds a statement in front of it and
    /// edits the process body. The document carries the literal's reference
    /// as text; the save must read it back as that reference and find the
    /// lifted body wherever the statement now sits.
    #[test]
    fn a_statement_added_before_a_lifted_process_saves_its_body_edit() {
        let source = "const blank = async () => {\n  return 0;\n};\n";
        let environment = crate::runtime::host_environment();
        let graph = lash::typescript::workflow_graph::workflow_graph_from_source_with_facets(
            source,
            Some(&environment),
        )
        .expect("the blank workflow admits");
        let mut document = document_from_graph(1, Ok(source.to_string()), graph.clone());

        let binding = document
            .nodes
            .iter()
            .find(|node| node.node_type == "data")
            .expect("the process binding")
            .clone();
        let mut inserted = binding;
        inserted.id = "new:before-process".to_string();
        *inserted.data.binding_mut().expect("binding node") = Some("greeting".to_string());
        *inserted.data.expression_mut().expect("expression node") = Some("\"hello\"".to_string());
        document.roots.main.insert(0, inserted.id.clone());
        document.nodes.push(inserted);
        document
            .nodes
            .iter_mut()
            .find(|node| node.node_type == "terminal")
            .expect("the process terminal")
            .data
            .expression_mut()
            .expect("expression node")
            .replace("7".to_string());

        let rebuilt = saved(document, &graph);
        let rendered = lash::typescript::workflow_graph::workflow_graph_to_source(&rebuilt)
            .unwrap_or_else(|error| panic!("the edited workflow renders: {error}"));
        assert_eq!(
            rendered,
            "let greeting = \"hello\";\nconst blank = async () => {\n  return 7;\n};\n"
        );
    }

    #[test]
    fn promoted_constructs_flatten_and_rebuild_as_typed_nodes() {
        let input = r#"const worker = async () => {
  return 1;
};
const runs = [await processes.start({ definition: worker }), await processes.start({ definition: worker })];
const state = { count: 0 };
state.count = 1;
while (state.count < 2) {
  state.count = state.count + 1;
}
let introduced = 0;
for (const item of [1, 2]) {
  state.count = item;
  introduced = item;
}
finish([state, introduced]);
"#;
        let graph = lash::typescript::workflow_graph::workflow_graph_from_source(input)
            .expect("project promoted graph");
        let source = lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("render promoted graph");
        let document = document_from_graph(1, Ok(source.clone()), graph.clone());

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
            node.data.name.title() == "while"
                && node.data.condition().as_deref() == Some("(state.count < 2)")
                && node
                    .data
                    .children()
                    .iter()
                    .any(|child| child.slot == "body")
        }));
        assert!(document.nodes.iter().any(|node| {
            node.node_type == "state_update"
                && node.data.target().as_deref() == Some("state.count")
                && node.data.expression().is_some()
        }));
        assert!(document.nodes.iter().any(|node| {
            node.node_type == "computation"
                && node.data.binding().as_deref() == Some("runs")
                && node.data.expression().as_deref()
                    == Some(
                        "[await processes.start({ definition: worker }), \
                         await processes.start({ definition: worker })]",
                    )
        }));

        let rebuilt = saved(document, &graph);
        assert_eq!(
            lash::typescript::workflow_graph::workflow_graph_to_source(&rebuilt)
                .expect("render rebuilt graph"),
            source
        );
        assert_eq!(
            lash::typescript::workflow_graph::workflow_graph_from_source(&source)
                .expect("reproject rebuilt source"),
            graph
        );
    }

    #[test]
    fn api_document_transport_preserves_every_nested_container_kind() {
        let input = r#"const items = [1, 2].map((value) => value * 2);
if (true) {
  for (const item of items) {
    while (false) {
    }
  }
} else {
}
finish(items);
"#;
        let graph = lash::typescript::workflow_graph::workflow_graph_from_source(input)
            .expect("project container graph");
        let source = lash::typescript::workflow_graph::workflow_graph_to_source(&graph)
            .expect("render container graph");
        let document = document_from_graph(1, Ok(source.clone()), graph.clone());

        let transported_json =
            serde_json::to_string(&document).expect("serialize public workflow document");
        let transported: WorkflowDocument =
            serde_json::from_str(&transported_json).expect("deserialize public workflow document");

        let container_kinds = transported
            .nodes
            .iter()
            .filter(|node| node.data.kind() == "container")
            .filter_map(|node| node.data.subkind())
            .collect::<BTreeSet<_>>();
        assert_eq!(container_kinds, BTreeSet::from(["for", "if", "while"]));
        assert!(transported.nodes.iter().any(|node| {
            node.data.subkind() == Some("if")
                && node
                    .data
                    .children()
                    .iter()
                    .any(|child| child.slot == "else" && child.node_ids.is_empty())
        }));
        assert!(transported.nodes.iter().any(|node| {
            node.data.subkind() == Some("while")
                && node
                    .data
                    .children()
                    .iter()
                    .any(|child| child.slot == "body" && child.node_ids.is_empty())
        }));

        let rebuilt = saved(transported, &graph);
        let rendered = lash::typescript::workflow_graph::workflow_graph_to_source(&rebuilt)
            .expect("render transported workflow graph");
        assert_eq!(rendered, source);
        assert_eq!(
            lash::typescript::workflow_graph::workflow_graph_from_source(&rendered)
                .expect("reproject transported source"),
            graph
        );
    }
}
