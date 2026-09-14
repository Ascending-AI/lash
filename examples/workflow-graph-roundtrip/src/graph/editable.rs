//! The lens's editable fields, as this example's HTTP surface sees them.
//!
//! Everything here turns one flow node's editable text into IR and back
//! through `lash_typescript`'s workflow-graph doors, so the example carries no
//! grammar of its own.

use std::collections::BTreeSet;

use lash_typescript::workflow_graph::{
    TypeScriptFragmentError, parse_typescript_assign_target, parse_typescript_expression,
    typescript_expression_source, typescript_statement_source,
};
use lashlang::{
    Expr, WorkflowDeclaration, WorkflowEffectKind, WorkflowGraph, WorkflowNode, WorkflowNodeId,
    WorkflowTerminalKind,
};

use super::{apply_fields, required_text};
use crate::{EditableValue, NodeData, RenderErrorResponse, WorkflowDocument};

pub(super) fn editable_expression(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<String, RenderErrorResponse> {
    editable_parsed_expression(id, data, graph_scope).map(|(source, _)| source)
}

pub(super) fn editable_parsed_expression(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<(String, Expr), RenderErrorResponse> {
    let scope = FragmentScope::of_data(data, graph_scope);
    let source = required_text(id, data.expression.as_ref(), "expression")?;
    let mut expression = parse_fragment(&source, &scope).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    apply_fields(id, &mut expression, &data.fields, &scope)?;
    let source = typescript_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

pub(super) fn editable_call_expression(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<(String, Expr), RenderErrorResponse> {
    let scope = FragmentScope::of_data(data, graph_scope);
    let has_authored_expression = data.expression.is_some();
    let mut expression = match &data.expression {
        Some(source) => parse_fragment(source, &scope).map_err(|error| {
            RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
        })?,
        None => {
            let operation = required_text(id, data.operation.as_ref(), "operation")?;
            // `display` stays undeclared so the fragment lowers it as the host
            // receiver it is; declaring it would make TypeScript treat the call
            // as a runtime method and reject the host operation.
            parse_fragment(&format!("await display.{operation}({{}})"), &scope).map_err(
                |error| RenderErrorResponse::invalid_expression(id, "operation", error.to_string()),
            )?
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
        apply_fields(id, &mut expression, &data.fields, &scope)?;
    }
    let source = typescript_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

pub(super) fn editable_effect_expression(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<(String, Expr), RenderErrorResponse> {
    if data.expression.is_some() {
        return editable_parsed_expression(id, data, graph_scope);
    }
    let effect = required_text(id, data.effect.as_ref(), "effect")?;
    let expression = match effect.as_str() {
        "sleep" => Expr::SleepFor(Box::new(
            data.fields
                .get("duration")
                .ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(id, "sleep needs a `duration` field")
                })?
                .to_expr(
                    id,
                    "fields.duration",
                    &FragmentScope::of_data(data, graph_scope),
                )?,
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
    let source = typescript_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

pub(super) fn parse_terminal_kind(
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

pub(super) fn terminal_expression(
    id: &str,
    terminal: &WorkflowTerminalKind,
    value: Option<&String>,
    scope: &FragmentScope,
) -> Result<String, RenderErrorResponse> {
    let value = required_text(id, value, "expression")?;
    let value = parse_fragment(&value, scope).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    // Inside a process the terminal is the `return` that ends the run body;
    // `finish` is cell-only. Both render through the lens's own printer.
    if scope.in_process && matches!(terminal, WorkflowTerminalKind::Finish) {
        let bound = scope.globals.iter().cloned().collect::<Vec<_>>();
        return typescript_statement_source(&Expr::Return(Box::new(value)), &bound).map_err(
            |error| RenderErrorResponse::invalid_expression(id, "expression", error.to_string()),
        );
    }
    let expression = match terminal {
        WorkflowTerminalKind::Finish => Expr::Finish(Box::new(value)),
        WorkflowTerminalKind::Fail => Expr::Fail(Box::new(value)),
    };
    typescript_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })
}

pub(super) fn parse_assignment_target(
    id: &str,
    source: &str,
    scope: &FragmentScope,
) -> Result<lashlang::AssignTarget, RenderErrorResponse> {
    parse_assignment_target_fragment(source, scope)
        .map_err(|message| RenderErrorResponse::invalid_assignment_target(id, "target", message))
}

pub(super) fn parse_assignment_target_fragment(
    source: &str,
    scope: &FragmentScope,
) -> Result<lashlang::AssignTarget, String> {
    parse_typescript_assign_target(source, &scope.globals, &scope.processes)
        .map_err(|error| error.to_string())
}

/// What a fragment cut from one node is allowed to name.
///
/// A TypeScript fragment is parsed by the dialect's own front-end, which
/// rejects an unknown binding. The names live where the fragment sits are
/// exactly the node's available variables, and the module's `defineProcess`
/// bindings stay process handles rather than ordinary ambient values, so a
/// fragment that starts one still resolves a static process target.
#[derive(Clone, Debug, Default)]
pub(crate) struct FragmentScope {
    pub(super) globals: BTreeSet<String>,
    pub(super) processes: BTreeSet<String>,
    pub(super) in_process: bool,
}

impl FragmentScope {
    pub(super) fn new(globals: BTreeSet<String>, graph: &GraphScope) -> Self {
        Self {
            globals,
            processes: graph.processes.clone(),
            in_process: graph.in_process,
        }
    }

    /// The scope around a projected graph node.
    pub(super) fn of_node(node: &WorkflowNode, graph: &GraphScope) -> Self {
        Self::new(node.available_variables.iter().cloned().collect(), graph)
    }

    /// The same scope, taken from an inbound flow-node payload.
    pub(super) fn of_data(data: &NodeData, graph: &GraphScope) -> Self {
        Self::new(
            data.available_vars
                .iter()
                .map(|variable| variable.name.clone())
                .collect(),
            graph,
        )
    }
}

/// Where in a graph a node sits: the module's process bindings, and whether
/// the node is inside a process body. TypeScript ends a process with `return`
/// and a cell with `finish(...)`, so a terminal's text depends on both.
#[derive(Clone, Debug, Default)]
pub(crate) struct GraphScope {
    pub(super) processes: BTreeSet<String>,
    pub(super) in_process: bool,
}

impl GraphScope {
    pub(super) fn main(processes: BTreeSet<String>) -> Self {
        Self {
            processes,
            in_process: false,
        }
    }

    pub(super) fn in_process(&self) -> Self {
        Self {
            processes: self.processes.clone(),
            in_process: true,
        }
    }
}

/// The `defineProcess` bindings a projected graph declares.
pub(super) fn process_bindings(graph: &WorkflowGraph) -> BTreeSet<String> {
    graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process.name.clone()),
            _ => None,
        })
        .collect()
}

/// The `defineProcess` bindings an edited document declares, which is the
/// baseline's set plus any process the host renamed or added in this edit.
pub(super) fn document_process_bindings(
    document: &WorkflowDocument,
    baseline: &WorkflowGraph,
) -> BTreeSet<String> {
    let mut names = process_bindings(baseline);
    names.extend(
        document
            .nodes
            .iter()
            .filter(|node| node.data.kind == "process")
            .filter_map(|node| node.data.process_name.clone()),
    );
    names
}

pub(super) fn parse_fragment(
    text: &str,
    scope: &FragmentScope,
) -> Result<Expr, TypeScriptFragmentError> {
    parse_typescript_expression(text, &scope.globals, &scope.processes)
}

pub(super) fn workflow_node_id(id: &str) -> WorkflowNodeId {
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

pub(super) fn first_receiver_operation(expression: &Expr) -> Option<&str> {
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

pub(super) fn receiver_operation_mut(
    expression: &mut Expr,
) -> Option<&mut compact_str::CompactString> {
    match expression {
        Expr::ReceiverCall { operation, .. } => Some(operation),
        Expr::Await(inner) | Expr::ResultUnwrap(inner) => receiver_operation_mut(inner),
        _ => None,
    }
}

pub(super) fn effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
    if let Expr::ResultUnwrap(inner) = expression {
        return direct_effect_kind(inner);
    }
    direct_effect_kind(expression)
}

pub(super) fn direct_effect_kind(expression: &Expr) -> Option<WorkflowEffectKind> {
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

pub(super) fn parse_effect_kind(
    id: &str,
    effect: &str,
) -> Result<WorkflowEffectKind, RenderErrorResponse> {
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
