use lash::vm::ir::{ProcessParam, WorkflowProcess, WorkflowSubgraph, format_type_expr};

use crate::{EditableProcessField, NodeData, RenderErrorResponse};

use super::{FragmentScope, parse_assignment_target_fragment, workflow_node_id};

pub(super) fn process_from_data(
    id: &str,
    data: &NodeData,
    baseline: Option<WorkflowProcess>,
) -> Result<WorkflowProcess, RenderErrorResponse> {
    let mut process = baseline.unwrap_or_else(|| WorkflowProcess {
        id: workflow_node_id(id),
        name: data
            .process_name()
            .clone()
            .unwrap_or_else(|| data.name.title().to_string()),
        display_name: data.name.title().to_string(),
        description: data.name.description().map(str::to_string),
        name_source: data.name.name_source(),
        params: Vec::new(),
        return_ty: None,
        origin: Default::default(),
        wrapper: None,
        body: WorkflowSubgraph::default(),
    });
    let process_id = process.id.to_string();
    // A lifted process's identity is derived from its body. The authored
    // name is the const binding; echoing its projected identity cannot rename it.
    let derived = process.origin.is_lifted();
    if !derived {
        let name = data.process_name().as_deref().unwrap_or(data.name.title());
        process.name = editable_identifier(&process_id, "name", name)?;
        process.display_name = data.name.title().to_string();
        process.description = data.name.description().map(str::to_string);
        process.name_source = data.name.name_source();
    }
    process.params = data
        .params()
        .iter()
        .map(|field| process_param_from_data(&process_id, field))
        .collect::<Result<_, _>>()?;
    Ok(process)
}

pub(super) fn editable_process_param(param: &ProcessParam) -> EditableProcessField {
    EditableProcessField {
        name: param.name.to_string(),
        field_type: format_type_expr(&param.ty),
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

/// ADR 0096 retired the Lash VM front-end, and with it the general
/// type-expression grammar this used to call. The vocabulary is not a loss:
/// the graph only ever renders a process parameter schema, and the
/// TypeScript printer can spell exactly the scalar schemas below — anything
/// richer had no way back out to source. So the closed set is stated here.
fn editable_process_type(
    process_id: &str,
    field: &str,
    value: &str,
) -> Result<lash::vm::ir::TypeExpr, RenderErrorResponse> {
    match value.trim() {
        "any" => Ok(lash::vm::ir::TypeExpr::Any),
        "null" => Ok(lash::vm::ir::TypeExpr::Null),
        "str" | "string" => Ok(lash::vm::ir::TypeExpr::Str),
        // TypeScript has one number type, which lowers to `float`; an `int`
        // parameter has no annotation the canonical source could carry.
        "float" | "number" => Ok(lash::vm::ir::TypeExpr::Float),
        "bool" | "boolean" => Ok(lash::vm::ir::TypeExpr::Bool),
        other => Err(RenderErrorResponse::invalid_node_payload(
            process_id,
            format!(
                "`data.{field}` is not a valid type expression: `{other}` is not one of \
                 any, null, str, float, bool"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TypeScript has one number type: a parameter typed `int` has no
    /// annotation the canonical source could carry, so it is refused where
    /// the host reads it rather than widened when the source prints.
    #[test]
    fn process_parameter_types_are_the_ones_typescript_can_annotate() {
        let process = "process";
        for (text, expected) in [
            ("number", lash::vm::ir::TypeExpr::Float),
            ("float", lash::vm::ir::TypeExpr::Float),
            ("string", lash::vm::ir::TypeExpr::Str),
            ("bool", lash::vm::ir::TypeExpr::Bool),
            ("any", lash::vm::ir::TypeExpr::Any),
        ] {
            assert_eq!(
                editable_process_type(process, "params.type", text)
                    .unwrap_or_else(|_| panic!("{text} is a parameter type")),
                expected
            );
        }
        assert!(editable_process_type(process, "params.type", "int").is_err());
    }
}
