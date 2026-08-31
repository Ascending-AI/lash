use crate::ast::{AssignTarget, Expr, ListComprehensionClause};
use crate::parse_expression;
use crate::source::{canonical_assign_target_source, canonical_expression_source};

use super::{GraphRenderError, WorkflowListComprehensionClause, WorkflowNode};

pub(super) fn expression_text(expression: &Expr, allow_non_sourceable: bool) -> String {
    match canonical_expression_source(expression) {
        Ok(text) => text,
        Err(error) if allow_non_sourceable => format!("<non-sourceable expression: {error}>"),
        Err(error) => {
            panic!("an expression parsed from canonical source must remain sourceable: {error}")
        }
    }
}

pub(super) fn assign_target_text(target: &AssignTarget) -> String {
    canonical_assign_target_source(target)
        .expect("an assignment target parsed from canonical source must remain sourceable")
}

pub(super) fn workflow_clause(
    clause: &ListComprehensionClause,
    allow_non_sourceable: bool,
) -> WorkflowListComprehensionClause {
    match clause {
        ListComprehensionClause::For { binding, iterable } => {
            WorkflowListComprehensionClause::For {
                binding: binding.to_string(),
                iterable: expression_text(iterable, allow_non_sourceable),
            }
        }
        ListComprehensionClause::If { condition } => WorkflowListComprehensionClause::If {
            condition: expression_text(condition, allow_non_sourceable),
        },
    }
}

pub(super) fn parse_expression_field(
    node: &WorkflowNode,
    field: &'static str,
    text: &str,
) -> Result<Expr, GraphRenderError> {
    parse_expression(text).map_err(|error| GraphRenderError::InvalidExpression {
        node_id: node.id.to_string(),
        field,
        message: error.to_string(),
    })
}

pub(super) fn parse_assignment_target_field(
    node: &WorkflowNode,
    field: &'static str,
    text: &str,
) -> Result<AssignTarget, GraphRenderError> {
    let expression = parse_expression(&format!("{text} = null")).map_err(|error| {
        GraphRenderError::InvalidAssignmentTarget {
            node_id: node.id.to_string(),
            field,
            message: error.to_string(),
        }
    })?;
    match expression {
        Expr::Assign { target, .. } => Ok(target),
        _ => Err(GraphRenderError::InvalidAssignmentTarget {
            node_id: node.id.to_string(),
            field,
            message: "expected an assignment target".to_string(),
        }),
    }
}

pub(super) fn parse_simple_binding_field(
    node: &WorkflowNode,
    field: &'static str,
    text: &str,
) -> Result<AssignTarget, GraphRenderError> {
    let target = parse_assignment_target_field(node, field, text)?;
    if !target.is_simple() {
        return invalid_payload(node, "this field requires a simple binding target");
    }
    Ok(target)
}

pub(super) fn parse_comprehension_clauses(
    node: &WorkflowNode,
    clauses: &[WorkflowListComprehensionClause],
) -> Result<Vec<ListComprehensionClause>, GraphRenderError> {
    clauses
        .iter()
        .map(|clause| match clause {
            WorkflowListComprehensionClause::For { binding, iterable } => {
                Ok(ListComprehensionClause::For {
                    binding: parse_simple_binding_field(node, "clause binding", binding)?.root,
                    iterable: parse_expression_field(node, "clause iterable", iterable)?,
                })
            }
            WorkflowListComprehensionClause::If { condition } => Ok(ListComprehensionClause::If {
                condition: parse_expression_field(node, "clause condition", condition)?,
            }),
        })
        .collect()
}

pub(super) fn with_assignment(
    node: &WorkflowNode,
    binding: &Option<String>,
    expression: Expr,
    must_be_simple: bool,
) -> Result<Expr, GraphRenderError> {
    match binding {
        Some(text) => {
            let target = parse_assignment_target_field(node, "binding", text)?;
            if must_be_simple && !target.is_simple() {
                return invalid_payload(node, "this node kind requires a simple binding target");
            }
            Ok(Expr::Assign {
                target,
                expr: Box::new(expression),
            })
        }
        None => Ok(expression),
    }
}

fn invalid_payload<T>(node: &WorkflowNode, message: &str) -> Result<T, GraphRenderError> {
    Err(GraphRenderError::InvalidNodePayload {
        node_id: node.id.to_string(),
        message: message.to_string(),
    })
}
