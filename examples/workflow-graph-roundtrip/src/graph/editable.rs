//! The lens's editable fields, as this example's HTTP surface sees them.
//!
//! Everything here turns one flow node's editable text into IR and back
//! through `lash_typescript`'s workflow-graph doors, so the example carries no
//! grammar of its own.

use std::collections::{BTreeMap, BTreeSet};

use lash::rlm::lang::{
    Expr, WorkflowDeclaration, WorkflowEffectKind, WorkflowGraph, WorkflowNode, WorkflowNodeId,
    WorkflowNodeKind, WorkflowTerminalKind, workflow_call_to_ir, workflow_effect_to_ir,
};
use lash::typescript::workflow_graph::{
    TypeScriptFragmentError, parse_typescript_assign_target, parse_typescript_expression,
    typescript_expression_source,
};

use super::{effect_name, required_text};
use crate::{EditableValue, NodeData, RenderErrorResponse, WorkflowDocument};

pub(super) fn editable_expression(
    id: &str,
    data: &NodeData,
    graph_scope: &GraphScope,
) -> Result<Expr, RenderErrorResponse> {
    editable_parsed_expression(id, data, graph_scope).map(|(_, expression)| expression)
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
    let mut has_authored_expression = data.expression.is_some();
    let mut expression = match &data.expression {
        Some(source) => parse_fragment(source, &scope).map_err(|error| {
            RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
        })?,
        None => {
            let operation = required_text(id, data.operation.as_ref(), "operation")?;
            synthesize_receiver_call(id, data, &operation, &scope)?
        }
    };
    // Switching a node to an operation of another receiver is not a method
    // rename: the authored text still names the receiver the node came from,
    // so renaming the method alone leaves `display.list_recent`. When the node
    // names a receiver its expression does not, the call is re-synthesized
    // from the node's own receiver and operation (FIG-3179).
    // A node whose expression is not a receiver call at all is left alone, so
    // the authored-expression guard below still refuses it rather than having
    // it quietly replaced by a synthesized call (FIG-3177).
    if let Some(receiver) = data.receiver.as_deref()
        && receiver_call_receiver(&expression).is_some_and(|current| current != receiver)
    {
        let operation = required_text(id, data.operation.as_ref(), "operation")?;
        expression = synthesize_receiver_call(id, data, &operation, &scope)?;
        has_authored_expression = false;
    }
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
    let scope = FragmentScope::of_data(data, graph_scope);
    let requested_effect = data.effect.as_deref();
    let mut expression = match &data.expression {
        Some(source) => parse_fragment(source, &scope).map_err(|error| {
            RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
        })?,
        None => synthesize_effect_expression(
            id,
            data,
            &required_text(id, data.effect.as_ref(), "effect")?,
            &scope,
        )?,
    };
    if let Some(requested_effect) = requested_effect {
        let current_effect = lash::rlm::lang::workflow_effect_from_ir(&expression)
            .map(|(effect, _, _)| effect)
            .ok_or_else(|| {
                RenderErrorResponse::invalid_node_payload(
                    id,
                    "an effect node needs a recognized effect expression",
                )
            })?;
        if effect_name(&current_effect) != requested_effect {
            expression = synthesize_effect_expression(id, data, requested_effect, &scope)?;
        } else {
            apply_fields(id, &mut expression, &data.fields, &scope)?;
        }
    } else {
        apply_fields(id, &mut expression, &data.fields, &scope)?;
    }
    let source = typescript_expression_source(&expression).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    Ok((source, expression))
}

fn synthesize_effect_expression(
    id: &str,
    data: &NodeData,
    effect: &str,
    scope: &FragmentScope,
) -> Result<Expr, RenderErrorResponse> {
    let expression = match effect {
        "sleep" => Expr::SleepFor(Box::new(
            data.fields
                .get("duration")
                .ok_or_else(|| {
                    RenderErrorResponse::invalid_node_payload(id, "sleep needs a `duration` field")
                })?
                .to_expr(id, "fields.duration", scope)?,
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
    Ok(expression)
}

pub(super) fn editable_fields(
    node: &WorkflowNode,
    _graph_scope: &GraphScope,
) -> BTreeMap<String, EditableValue> {
    let expression = match &node.kind {
        WorkflowNodeKind::Data { expression, .. }
        | WorkflowNodeKind::Terminal { expression, .. } => expression.clone(),
        WorkflowNodeKind::Call {
            receiver,
            operation,
            arguments,
            result_steps,
            ..
        } => workflow_call_to_ir(receiver, operation, arguments, result_steps),
        WorkflowNodeKind::Effect {
            effect,
            arguments,
            result_steps,
            ..
        } => {
            let Some(expression) = workflow_effect_to_ir(*effect, arguments, result_steps) else {
                return BTreeMap::new();
            };
            expression
        }
        _ => return BTreeMap::new(),
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

pub(super) fn apply_fields(
    node_id: &str,
    expression: &mut Expr,
    fields: &BTreeMap<String, EditableValue>,
    scope: &FragmentScope,
) -> Result<(), RenderErrorResponse> {
    if let Some(entries) = receiver_fields_mut(expression) {
        entries.clear();
        for (name, value) in fields {
            let value = value.to_expr(node_id, &format!("fields.{name}"), scope)?;
            entries.push((name.clone().into(), value));
        }
        return Ok(());
    }
    match expression {
        Expr::SleepFor(value) | Expr::SleepUntil(value) => {
            if let Some(duration) = fields.get("duration") {
                **value = duration.to_expr(node_id, "fields.duration", scope)?;
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
    #[expect(
        clippy::expect_used,
        reason = "the expression was parsed from authored source, so re-sourcing it round-trips"
    )]
    fn from_expr(expression: &Expr) -> Self {
        Self::literal_from_expr(expression).unwrap_or_else(|| {
            Self::Expr(
                typescript_expression_source(expression)
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

    fn to_expr(
        &self,
        node_id: &str,
        field: &str,
        scope: &FragmentScope,
    ) -> Result<Expr, RenderErrorResponse> {
        Ok(match self {
            Self::Null => Expr::Null,
            Self::Bool(value) => Expr::Bool(*value),
            Self::Number(value) => Expr::Number(*value),
            Self::String(value) => Expr::String(value.clone().into()),
            Self::List(values) => Expr::List(
                values
                    .iter()
                    .map(|value| value.to_expr(node_id, field, scope))
                    .collect::<Result<_, _>>()?,
            ),
            Self::Expr(source) => parse_fragment(source, scope).map_err(|error| {
                RenderErrorResponse::invalid_expression(node_id, field, error.to_string())
            })?,
            Self::Object(entries) => Expr::Record(
                entries
                    .iter()
                    .map(|(key, value)| {
                        Ok((key.clone().into(), value.to_expr(node_id, field, scope)?))
                    })
                    .collect::<Result<_, RenderErrorResponse>>()?,
            ),
        })
    }
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
) -> Result<Expr, RenderErrorResponse> {
    let value = required_text(id, value, "expression")?;
    let value = parse_fragment(&value, scope).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "expression", error.to_string())
    })?;
    // Inside a process the terminal is the `return` that ends the run body;
    // `finish` is cell-only. Both render through the lens's own printer.
    if scope.in_process && matches!(terminal, WorkflowTerminalKind::Finish) {
        return Ok(Expr::Return(Box::new(value)));
    }
    let expression = match terminal {
        WorkflowTerminalKind::Finish => Expr::Finish(Box::new(value)),
        WorkflowTerminalKind::Fail => Expr::Fail(Box::new(value)),
    };
    Ok(expression)
}

pub(super) fn parse_assignment_target(
    id: &str,
    source: &str,
    scope: &FragmentScope,
) -> Result<lash::rlm::lang::AssignTarget, RenderErrorResponse> {
    parse_assignment_target_fragment(source, scope)
        .map_err(|message| RenderErrorResponse::invalid_assignment_target(id, "target", message))
}

pub(super) fn parse_assignment_target_fragment(
    source: &str,
    scope: &FragmentScope,
) -> Result<lash::rlm::lang::AssignTarget, String> {
    parse_typescript_assign_target(source, &scope.globals, &scope.processes)
        .map_err(|error| error.to_string())
}

/// What a fragment cut from one node is allowed to name.
///
/// A TypeScript fragment is parsed by the dialect's own front-end, which
/// rejects an unknown binding. The names live where the fragment sits are
/// exactly the node's available variables, and the module's process
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

/// The process bindings a projected graph declares.
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

/// The process bindings an edited document declares, which is the
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

#[expect(
    clippy::expect_used,
    reason = "the placeholder hex id is generated by this module's id grammar, which \
              WorkflowNodeId accepts"
)]
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

/// The awaited receiver call the editor seeds for a catalog entry, parsed in
/// the node's own scope.
///
/// The receiver stays undeclared so the fragment lowers it as the host receiver
/// it is; declaring it would make TypeScript treat the call as a runtime method
/// and reject the host operation. The receiver rides on the node because the
/// operation name alone does not name it: `list_recent` belongs to `gmail`, not
/// to `display`. A node from a client that predates the catalog field still
/// means the display catalog it could reach (FIG-3178).
pub(super) fn synthesize_receiver_call(
    id: &str,
    data: &NodeData,
    operation: &str,
    scope: &FragmentScope,
) -> Result<Expr, RenderErrorResponse> {
    let receiver = data.receiver.as_deref().unwrap_or(crate::display::RECEIVER);
    parse_fragment(&format!("await {receiver}.{operation}({{}})"), scope).map_err(|error| {
        RenderErrorResponse::invalid_expression(id, "operation", error.to_string())
    })
}

/// The receiver a call expression names, as source (`display`, `gmail`, ...),
/// so a node's declared receiver can be compared against the one its expression
/// actually calls.
pub(super) fn receiver_call_receiver(expression: &Expr) -> Option<String> {
    match expression {
        Expr::ReceiverCall { receiver, .. } => typescript_expression_source(receiver).ok(),
        Expr::Await(inner) | Expr::ResultUnwrap(inner) => receiver_call_receiver(inner),
        _ => None,
    }
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

pub(super) fn parse_effect_kind(
    id: &str,
    effect: &str,
) -> Result<WorkflowEffectKind, RenderErrorResponse> {
    match effect {
        "await_join" => Ok(WorkflowEffectKind::AwaitJoin),
        "wait_signal" => Ok(WorkflowEffectKind::WaitSignal),
        "sleep" => Ok(WorkflowEffectKind::SleepFor),
        "print" => Ok(WorkflowEffectKind::Print),
        "yield" => Ok(WorkflowEffectKind::Yield),
        "break" => Ok(WorkflowEffectKind::Break),
        "continue" => Ok(WorkflowEffectKind::Continue),
        _ => Err(RenderErrorResponse::invalid_node_payload(
            id,
            format!("unknown effect kind `{effect}`"),
        )),
    }
}
