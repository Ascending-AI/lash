use lash::typescript::workflow_graph::{
    typescript_assign_target_source, typescript_expression_source, typescript_for_of_source,
};
use lash::vm::ir::{
    Expr, VariableVersion, WorkflowContainer, WorkflowDeclaration, WorkflowEdge, WorkflowNode,
    WorkflowNodeKind, WorkflowSubgraph, format_type_expr, workflow_call_from_ir,
    workflow_call_to_ir, workflow_effect_from_ir, workflow_effect_to_ir,
};
use lash::workflow::{WorkflowCatch, WorkflowGraph};
use serde_json::json;

use crate::{
    ChildGroup, EdgeData, ExpectedArgumentType, FlowEdge, FlowNode, GraphRoots, NodeBody,
    NodeContainer, NodeData, NodeName, RenderErrorResponse, TypeDiagnostic, TypedVariable,
    ValidateRequest, ValidateResponse, ValidationKind, WorkflowDocument,
};

mod editable;
mod process;

use editable::*;

use process::editable_process_param;

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
            if reworded || data.terminal_kind() != shown.terminal_kind() {
                let returns = matches!(expression, Expr::FunctionReturn(_));
                *terminal = required_terminal_kind(&node_id, data.terminal_kind())?;
                let mut terminal_scope = scope.clone();
                terminal_scope.in_process = returns;
                *expression = terminal_expression(
                    &node_id,
                    terminal,
                    data.expression().as_ref(),
                    &terminal_scope,
                )?;
            }
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

/// Lower just the form being edited. Child bodies and process wrappers stay in
/// the draft and are changed only by their explicit operations.
pub(crate) fn form_node(
    id: &str,
    data: &NodeData,
    baseline: Option<&WorkflowNode>,
    graph: &WorkflowGraph,
    in_process: bool,
) -> Result<WorkflowNode, RenderErrorResponse> {
    let mut scope = GraphScope::main(process_bindings(graph));
    scope.in_process = in_process;
    if let Some(baseline) = baseline {
        let mut node = baseline.clone();
        apply_editable_data(&mut node, data, &scope)?;
        Ok(node)
    } else {
        node_from_flow_data(id, data, &scope)
    }
}

pub(crate) fn form_process(
    id: &str,
    data: &NodeData,
    baseline: Option<lash::vm::ir::WorkflowProcess>,
) -> Result<lash::vm::ir::WorkflowProcess, RenderErrorResponse> {
    process::process_from_data(id, data, baseline)
}
