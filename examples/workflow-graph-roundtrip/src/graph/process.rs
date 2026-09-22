use lash::ProcessId;
use lashlang::{
    ProcessParam, ProcessSignalDecl, WorkflowNode, WorkflowNodeKind, WorkflowNodeNameSource,
    WorkflowProcess, WorkflowSubgraph, WorkflowTerminalKind, format_type_expr,
};

use crate::{EditableProcessField, NodeData, RenderErrorResponse};

use super::{FragmentScope, parse_assignment_target_fragment, parse_fragment, workflow_node_id};

pub(super) fn process_from_data(
    id: &str,
    data: &NodeData,
    baseline: Option<WorkflowProcess>,
) -> Result<WorkflowProcess, RenderErrorResponse> {
    let mut process = baseline.unwrap_or_else(|| WorkflowProcess {
        id: workflow_node_id(id),
        name: data
            .process_name
            .clone()
            .unwrap_or_else(|| data.name.title().to_string()),
        display_name: data.name.title().to_string(),
        description: data.name.description().map(str::to_string),
        name_source: data.name.name_source(),
        params: Vec::new(),
        signals: Vec::new(),
        return_ty: None,
        body: WorkflowSubgraph::default(),
    });
    let process_id = ProcessId::from(process.id.to_string());
    // A lifted process's name, title and signals are derived, not authored
    // (FIG-2999, FIG-3118): the name digests the body, the displayed title is
    // that name, and the signal set is read back out of the body's
    // `waitSignal` calls. The authored name is the `const` binding, which the
    // host edits on the statement node that carries the arrow. So a client
    // that echoes these fields back is echoing a projection, and the echo is
    // dropped here the way a type facet's is — accepting it would leave the
    // module with a second declaration nothing binds.
    let derived = super::is_lifted_process(&process.name);
    if !derived {
        let name = data.process_name.as_deref().unwrap_or(data.name.title());
        process.name = editable_identifier(&process_id, "name", name)?;
        process.display_name = data.name.title().to_string();
        process.description = data.name.description().map(str::to_string);
        process.name_source = data.name.name_source();
        process.signals = data
            .signals
            .iter()
            .map(|field| process_signal_from_data(&process_id, field))
            .collect::<Result<_, _>>()?;
    }
    process.params = data
        .params
        .iter()
        .map(|field| process_param_from_data(&process_id, field))
        .collect::<Result<_, _>>()?;
    Ok(process)
}

pub(super) fn seeded_process_body(
    process_id: &ProcessId,
    params: &[ProcessParam],
) -> WorkflowSubgraph {
    WorkflowSubgraph {
        nodes: vec![WorkflowNode {
            id: workflow_node_id(&format!("{process_id}:seed:finish")),
            name: "finish".to_string(),
            description: None,
            name_source: WorkflowNodeNameSource::Derived,
            kind: WorkflowNodeKind::Terminal {
                terminal: WorkflowTerminalKind::Finish,
                expression: lashlang::Expr::Return(Box::new(lashlang::Expr::Number(0.0))),
            },
            available_variables: params.iter().map(|param| param.name.to_string()).collect(),
            type_facets: None,
            outputs: Vec::new(),
            execution_sites: Vec::new(),
            source_span: None,
        }],
        edges: Vec::new(),
    }
}

pub(super) fn editable_process_param(param: &ProcessParam) -> EditableProcessField {
    EditableProcessField {
        name: param.name.to_string(),
        field_type: format_type_expr(&param.ty),
    }
}

pub(super) fn editable_process_signal(signal: &ProcessSignalDecl) -> EditableProcessField {
    EditableProcessField {
        name: signal.name.to_string(),
        field_type: format_type_expr(&signal.ty),
    }
}

fn process_param_from_data(
    process_id: &ProcessId,
    field: &EditableProcessField,
) -> Result<ProcessParam, RenderErrorResponse> {
    Ok(ProcessParam {
        name: editable_identifier(process_id, "params.name", &field.name)?.into(),
        ty: editable_process_type(process_id, "params.type", &field.field_type)?,
    })
}

fn process_signal_from_data(
    process_id: &ProcessId,
    field: &EditableProcessField,
) -> Result<ProcessSignalDecl, RenderErrorResponse> {
    Ok(ProcessSignalDecl {
        name: editable_signal_name(process_id, &field.name)?.into(),
        ty: editable_process_type(process_id, "signals.type", &field.field_type)?,
    })
}

/// A signal name is an object key in `signals: { <name>: <type> }`, and a
/// TypeScript key may be a reserved word such as `continue`. So it is read
/// back as a key rather than as an identifier expression.
fn editable_signal_name(node_id: &str, value: &str) -> Result<String, RenderErrorResponse> {
    let reject = || {
        RenderErrorResponse::invalid_node_payload(
            node_id,
            "`data.signals.name` must be a single property name",
        )
    };
    let record = parse_fragment(&format!("({{ {value}: null }})"), &FragmentScope::default())
        .map_err(|message| {
            RenderErrorResponse::invalid_node_payload(
                node_id,
                format!("`data.signals.name` must be a property name: {message}"),
            )
        })?;
    let lashlang::Expr::Record(entries) = record else {
        return Err(reject());
    };
    let [(name, _)] = entries.as_slice() else {
        return Err(reject());
    };
    Ok(name.to_string())
}

fn editable_identifier(
    node_id: &str,
    field: &str,
    value: &str,
) -> Result<String, RenderErrorResponse> {
    let target =
        parse_assignment_target_fragment(value, &FragmentScope::default()).map_err(|message| {
            RenderErrorResponse::invalid_node_payload(
                node_id,
                format!("`data.{field}` must be an identifier: {message}"),
            )
        })?;
    target
        .is_simple()
        .then(|| target.root.to_string())
        .ok_or_else(|| {
            RenderErrorResponse::invalid_node_payload(
                node_id,
                format!("`data.{field}` must be an identifier without field or index access"),
            )
        })
}

/// ADR 0096 retired the Lashlang front-end, and with it the general
/// type-expression grammar this used to call. The vocabulary is not a loss:
/// the graph only ever renders a process parameter or signal schema, and the
/// TypeScript printer can spell exactly the scalar schemas below — anything
/// richer had no way back out to source. So the closed set is stated here.
fn editable_process_type(
    process_id: &ProcessId,
    field: &str,
    value: &str,
) -> Result<lashlang::TypeExpr, RenderErrorResponse> {
    match value.trim() {
        "any" => Ok(lashlang::TypeExpr::Any),
        "null" => Ok(lashlang::TypeExpr::Null),
        "str" | "string" => Ok(lashlang::TypeExpr::Str),
        "int" => Ok(lashlang::TypeExpr::Int),
        "float" => Ok(lashlang::TypeExpr::Float),
        "bool" | "boolean" => Ok(lashlang::TypeExpr::Bool),
        other => Err(RenderErrorResponse::invalid_node_payload(
            process_id,
            format!(
                "`data.{field}` is not a valid type expression: `{other}` is not one of \
                 any, null, str, int, float, bool"
            ),
        )),
    }
}
