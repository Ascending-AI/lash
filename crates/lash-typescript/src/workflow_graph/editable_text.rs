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

use super::printer::{
    typescript_assign_target_source, typescript_expression_source, typescript_statement_source,
};
use super::{
    GraphRenderError, RenderContext, RenderScope, WorkflowListComprehensionClause, WorkflowNode,
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

pub(super) fn statement_text(
    expression: &Expr,
    bound: &[String],
    allow_non_sourceable: bool,
) -> String {
    match typescript_statement_source(expression, bound) {
        Ok(text) => text,
        Err(error) if allow_non_sourceable => format!("<non-sourceable statement: {error}>"),
        Err(error) => {
            panic!("a statement parsed from canonical source must remain sourceable: {error}")
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
///
/// Returns the parsed program and the number of leading statements the wrapper
/// itself contributed, which the caller skips.
pub(super) fn parse_typescript_fragment(
    node: &WorkflowNode,
    text: &str,
    context: RenderContext<'_>,
) -> Result<(Program, usize), GraphRenderError> {
    let names = fragment_bindings(node);
    let (source, globals, prelude) = match context.scope {
        RenderScope::Main => (text.to_string(), names.iter().cloned().collect(), 0),
        // A process fragment is reparsed inside a process, and a function body
        // cannot close over a mutable outer binding. So the visible names are
        // re-declared inside the wrapper rather than left sitting above it,
        // which is what keeps an edited reassignment parseable.
        RenderScope::Process => {
            let prelude = names
                .iter()
                .map(|name| format!("  let {name};\n"))
                .collect::<String>();
            (
                format!(
                    "const {OPAQUE_WRAPPER} = defineProcess({{ name: \"{OPAQUE_WRAPPER}\", signals: {{}}, run: async () => {{\n{prelude}{text}\n}} }});\n"
                ),
                BTreeSet::new(),
                names.len(),
            )
        }
    };
    let program = crate::parse_workflow_fragment(&source, &globals, &context.process_bindings())
        .map_err(|error| GraphRenderError::InvalidOpaqueSource {
            node_id: node.id.to_string(),
            message: error.to_string(),
        })?;
    Ok((program, prelude))
}

/// The names a fragment may read, in the order the wrapper declares them.
///
/// Generated bindings are not authored names and never reach a fragment.
fn fragment_bindings(node: &WorkflowNode) -> Vec<String> {
    node.available_variables
        .iter()
        .filter(|name| !name.starts_with(crate::GENERATED_BINDING_PREFIX))
        .cloned()
        .collect()
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
    context: RenderContext<'_>,
) -> Result<Expr, GraphRenderError> {
    let globals = node
        .available_variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    // An expression field is read in expression position, so it parses inside
    // parentheses: `{ count: 0 }` is an object literal here, and a bare
    // statement parse would read it as a labelled block.
    let parenthesized = format!("(\n{text}\n)");
    let program =
        crate::parse_workflow_fragment(&parenthesized, &globals, &context.process_bindings())
            .map_err(|error| GraphRenderError::InvalidExpression {
                node_id: node.id.to_string(),
                field,
                message: error.to_string(),
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
    let program =
        crate::parse_workflow_fragment(text, &globals, &BTreeSet::new()).map_err(|error| {
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
    context: RenderContext<'_>,
) -> Result<Vec<ListComprehensionClause>, GraphRenderError> {
    clauses
        .iter()
        .map(|clause| match clause {
            WorkflowListComprehensionClause::For { binding, iterable } => {
                Ok(ListComprehensionClause::For {
                    binding: parse_simple_binding_field(node, "clause binding", binding)?.root,
                    iterable: parse_expression_field(node, "clause iterable", iterable, context)?,
                })
            }
            WorkflowListComprehensionClause::If { condition } => Ok(ListComprehensionClause::If {
                condition: parse_expression_field(node, "clause condition", condition, context)?,
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
