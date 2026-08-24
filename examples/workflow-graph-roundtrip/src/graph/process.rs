use lashlang::{
    ProcessParam, ProcessSignalDecl, WorkflowNode, WorkflowNodeKind, WorkflowNodeNameSource,
    WorkflowProcess, WorkflowSubgraph, WorkflowTerminalKind, format_type_expr,
    parse_type_expression,
};

use crate::{EditableProcessField, NodeData, RenderErrorResponse};

use super::{parse_assignment_target_fragment, workflow_node_id};

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
    let process_id = process.id.to_string();
    let name = data.process_name.as_deref().unwrap_or(data.name.title());
    process.name = editable_identifier(&process_id, "name", name)?;
    process.display_name = data.name.title().to_string();
    process.description = data.name.description().map(str::to_string);
    process.name_source = data.name.name_source();
    process.params = data
        .params
        .iter()
        .map(|field| process_param_from_data(&process_id, field))
        .collect::<Result<_, _>>()?;
    process.signals = data
        .signals
        .iter()
        .map(|field| process_signal_from_data(&process_id, field))
        .collect::<Result<_, _>>()?;
    Ok(process)
}

pub(super) fn seeded_process_body(process_id: &str, params: &[ProcessParam]) -> WorkflowSubgraph {
    WorkflowSubgraph {
        nodes: vec![WorkflowNode {
            id: workflow_node_id(&format!("{process_id}:seed:finish")),
            name: "finish".to_string(),
            description: None,
            name_source: WorkflowNodeNameSource::Derived,
            kind: WorkflowNodeKind::Terminal {
                terminal: WorkflowTerminalKind::Finish,
                expression: "finish 0".to_string(),
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
    process_id: &str,
    field: &EditableProcessField,
) -> Result<ProcessParam, RenderErrorResponse> {
    Ok(ProcessParam {
        name: editable_identifier(process_id, "params.name", &field.name)?.into(),
        ty: editable_process_type(process_id, "params.type", &field.field_type)?,
    })
}

fn process_signal_from_data(
    process_id: &str,
    field: &EditableProcessField,
) -> Result<ProcessSignalDecl, RenderErrorResponse> {
    Ok(ProcessSignalDecl {
        name: editable_identifier(process_id, "signals.name", &field.name)?.into(),
        ty: editable_process_type(process_id, "signals.type", &field.field_type)?,
    })
}

fn editable_identifier(
    node_id: &str,
    field: &str,
    value: &str,
) -> Result<String, RenderErrorResponse> {
    let target = parse_assignment_target_fragment(value).map_err(|message| {
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

fn editable_process_type(
    process_id: &str,
    field: &str,
    value: &str,
) -> Result<lashlang::TypeExpr, RenderErrorResponse> {
    parse_type_expression(value).map_err(|error| {
        RenderErrorResponse::invalid_node_payload(
            process_id,
            format!("`data.{field}` is not a valid type expression: {error}"),
        )
    })
}
