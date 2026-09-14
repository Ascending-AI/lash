//! The lens's editable text fields: printing one node's TypeScript and parsing
//! a host's edit of it back into IR.
//!
//! Every field here is TypeScript, and every parse goes through the dialect's
//! own front-end rather than a second grammar. A node carries the identifiers
//! visible before it runs, so a fragment that references them is parsed with
//! exactly those names in scope: the dialect rejects an unknown binding, which
//! is the behaviour the lens wants, but only for names that really are not
//! there.

use std::collections::BTreeSet;

use lashlang::{AssignPathStep, AssignTarget, Expr, ListComprehensionClause, Program};

use super::printer::{typescript_assign_target_source, typescript_expression_source};
use super::{
    GraphRenderError, RenderContext, WorkflowListComprehensionClause, WorkflowNode,
    process_run_body_of,
};

pub(super) fn expression_text(expression: &Expr, allow_non_sourceable: bool) -> String {
    match typescript_expression_source(expression) {
        Ok(text) => text,
        Err(error) if allow_non_sourceable => format!("<non-sourceable expression: {error}>"),
        Err(error) => {
            panic!("an expression parsed from canonical source must remain sourceable: {error}")
        }
    }
}

pub(super) fn assign_target_text(target: &AssignTarget) -> String {
    typescript_assign_target_source(target)
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

/// Parse one fragment of TypeScript with the node's visible bindings in scope.
pub(super) fn parse_typescript_fragment(
    node: &WorkflowNode,
    text: &str,
    context: RenderContext,
) -> Result<Program, GraphRenderError> {
    let globals = node
        .available_variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let source = match context {
        RenderContext::Main => text.to_string(),
        RenderContext::Process => format!(
            "const {OPAQUE_WRAPPER} = defineProcess({{ name: \"{OPAQUE_WRAPPER}\", signals: {{}}, run: async () => {{\n{text}\n}} }});\n"
        ),
    };
    crate::parse_with_globals(&source, &globals).map_err(|error| {
        GraphRenderError::InvalidOpaqueSource {
            node_id: node.id.to_string(),
            message: error.to_string(),
        }
    })
}

/// The name the opaque-statement wrapper binds. It never reaches a graph.
const OPAQUE_WRAPPER: &str = "workflowGraphOpaque";

/// The authored `run` body of the opaque-statement wrapper process.
pub(super) fn opaque_process_run_body(process: &lashlang::ProcessDecl) -> Option<&Expr> {
    process_run_body_of(process)
}

pub(super) fn parse_expression_field(
    node: &WorkflowNode,
    field: &'static str,
    text: &str,
) -> Result<Expr, GraphRenderError> {
    let globals = node
        .available_variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let program = crate::parse_with_globals(text, &globals).map_err(|error| {
        GraphRenderError::InvalidExpression {
            node_id: node.id.to_string(),
            field,
            message: error.to_string(),
        }
    })?;
    if !program.declarations.is_empty() {
        return Err(GraphRenderError::InvalidExpression {
            node_id: node.id.to_string(),
            field,
            message: "expected one expression, found a declaration".to_string(),
        });
    }
    single_expression(program.main).ok_or(GraphRenderError::InvalidExpression {
        node_id: node.id.to_string(),
        field,
        message: "expected exactly one expression".to_string(),
    })
}

fn single_expression(main: Expr) -> Option<Expr> {
    match main {
        Expr::Block(mut expressions) => match expressions.len() {
            1 => Some(expressions.remove(0)),
            _ => None,
        },
        expression => Some(expression),
    }
}

/// Parse an assignment target such as `total`, `state.count` or `rows[0]`.
///
/// The dialect has no production for a bare target, so the text is parsed as
/// the member expression it is and converted; that keeps one grammar in play
/// rather than a second hand-written path parser.
pub(super) fn parse_assignment_target_field(
    node: &WorkflowNode,
    field: &'static str,
    text: &str,
) -> Result<AssignTarget, GraphRenderError> {
    let root = text
        .split(['.', '['])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut globals = node
        .available_variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    globals.insert(root);
    let program = crate::parse_with_globals(text, &globals).map_err(|error| {
        GraphRenderError::InvalidAssignmentTarget {
            node_id: node.id.to_string(),
            field,
            message: error.to_string(),
        }
    })?;
    let invalid = || GraphRenderError::InvalidAssignmentTarget {
        node_id: node.id.to_string(),
        field,
        message: "expected an assignment target".to_string(),
    };
    let expression = single_expression(program.main).ok_or_else(invalid)?;
    assign_target_from_expression(&expression).ok_or_else(invalid)
}

fn assign_target_from_expression(expression: &Expr) -> Option<AssignTarget> {
    match expression {
        Expr::Variable(name) => Some(AssignTarget {
            root: name.clone(),
            steps: Vec::new(),
        }),
        Expr::Field { target, field } => {
            let mut target = assign_target_from_expression(target)?;
            target.steps.push(AssignPathStep::Field(field.clone()));
            Some(target)
        }
        Expr::Index { target, index } => {
            let mut target = assign_target_from_expression(target)?;
            target
                .steps
                .push(AssignPathStep::Index(index.as_ref().clone()));
            Some(target)
        }
        _ => None,
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
