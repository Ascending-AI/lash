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

pub(super) fn assign_target_text(target: &AssignTarget, allow_non_sourceable: bool) -> String {
    match typescript_assign_target_source(target) {
        Ok(text) => text,
        // A lowered program projected for a trace may bind a generated
        // destructuring temporary, which has no authored spelling.
        Err(error) if allow_non_sourceable => format!("<non-sourceable target: {error}>"),
        Err(_) => {
            panic!("an assignment target parsed from canonical source must remain sourceable")
        }
    }
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
                format!("const {OPAQUE_WRAPPER} = async () => {{\n{prelude}{text}\n}};\n"),
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

/// The authored statements of the opaque-statement wrapper.
///
/// The wrapper is a top-level `const`-bound `async` arrow, which FIG-2999 made
/// a process *literal* assigned in `main` rather than a `Declaration::Process`
/// (ADR 0095). So the wrapper is read back out of its own binding, and the
/// authored body out of the run wrapper the literal carries.
pub(super) fn opaque_wrapper_run_body(program: &Program) -> Option<&Expr> {
    let [Expr::Assign { target, expr }] = super::printer::statement_block_contents(&program.main)
    else {
        return None;
    };
    if target.root.as_str() != OPAQUE_WRAPPER || !target.steps.is_empty() {
        return None;
    }
    let Expr::ProcessLiteral(literal) = expr.as_ref() else {
        return None;
    };
    super::printer::process_literal_run_body(literal)
}

/// Parse one editable TypeScript expression fragment with `globals` in scope.
///
/// This is the lens's public fragment door: hosts that let a person retype a
/// node's expression parse the result through the dialect's own front-end, in
/// expression position, rather than carrying a second grammar. The text is
/// read inside parentheses so `{ count: 0 }` is an object literal and not a
/// labelled block.
pub fn parse_typescript_expression(
    text: &str,
    globals: &BTreeSet<String>,
    processes: &BTreeSet<String>,
) -> Result<Expr, TypeScriptFragmentError> {
    expression_fragment(text, globals, processes).map_err(TypeScriptFragmentError)
}

/// Parse one editable expression fragment, reading an `async` arrow as the
/// process literal it is.
///
/// Both expression doors — the public fragment door and the node-field door —
/// go through here, so a host that re-parses and re-prints a node's text gets
/// the same expression the renderer would. Parenthesising an `async` arrow
/// reads it as an ordinary function expression, which drops the `async` on the
/// way back out and makes the next parse refuse its `await`s; the arrow is
/// therefore parsed as the one statement that promotes it (FIG-3118).
fn expression_fragment(
    text: &str,
    globals: &BTreeSet<String>,
    processes: &BTreeSet<String>,
) -> Result<Expr, String> {
    // An `async` arrow is a process literal, and only a top-level `const`
    // binding promotes it to one. Every other expression field is read in
    // expression position, so it parses inside parentheses: `{ count: 0 }` is
    // an object literal there, and a bare statement parse would read it as a
    // labelled block.
    let (fragment, unwrap_binding) = if is_async_arrow_text(text) {
        (
            format!("const {PROCESS_LITERAL_BINDING} =\n{text}\n;"),
            true,
        )
    } else {
        (format!("(\n{text}\n)"), false)
    };
    let program = crate::parse_workflow_fragment(&fragment, globals, processes)
        .map_err(|error| error.to_string())?;
    if !program.declarations.is_empty() {
        return Err("expected one expression, found a declaration".to_string());
    }
    let expression =
        single_expression(program.main).ok_or("expected exactly one expression".to_string())?;
    if !unwrap_binding {
        return Ok(expression);
    }
    match expression {
        Expr::Assign { target, expr } if target.root.as_str() == PROCESS_LITERAL_BINDING => {
            Ok(*expr)
        }
        _ => Err("expected a process literal binding".to_string()),
    }
}

/// Parse one editable TypeScript assignment target such as `total`,
/// `state.count` or `rows[0]`, with `globals` in scope.
pub fn parse_typescript_assign_target(
    text: &str,
    globals: &BTreeSet<String>,
    processes: &BTreeSet<String>,
) -> Result<AssignTarget, TypeScriptFragmentError> {
    let root = text
        .split(['.', '['])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut globals = globals.clone();
    globals.insert(root);
    let expression = parse_typescript_expression(text, &globals, processes)?;
    assign_target_from_expression(&expression)
        .ok_or_else(|| TypeScriptFragmentError("expected an assignment target".to_string()))
}

/// Parse one editable TypeScript statement that only a process body accepts,
/// such as the `return` that ends a process.
///
/// The text is reparsed inside a generated process-arrow wrapper, with
/// `globals` re-declared in the run body so an edited reassignment still
/// parses, and the wrapper's single statement is returned. The wrapper name
/// never reaches a graph.
pub fn parse_typescript_process_statement(
    text: &str,
    globals: &BTreeSet<String>,
    processes: &BTreeSet<String>,
) -> Result<Expr, TypeScriptFragmentError> {
    let prelude = globals
        .iter()
        .map(|name| format!("  let {name};\n"))
        .collect::<String>();
    let source = format!("const {OPAQUE_WRAPPER} = async () => {{\n{prelude}{text}\n}};\n");
    let program = crate::parse_workflow_fragment(&source, &BTreeSet::new(), processes)
        .map_err(|error| TypeScriptFragmentError(error.to_string()))?;
    let Some(body) = opaque_wrapper_run_body(&program) else {
        return Err(TypeScriptFragmentError(
            "expected one process statement".to_string(),
        ));
    };
    let statements = super::printer::statement_block_contents(body)
        .iter()
        .skip(globals.len())
        .cloned()
        .collect::<Vec<_>>();
    match statements.len() {
        #[expect(
            clippy::expect_used,
            reason = "this arm matches a length of exactly one, so the iterator yields that statement"
        )]
        1 => Ok(statements.into_iter().next().expect("one statement")),
        found => Err(TypeScriptFragmentError(format!(
            "expected one statement, found {found}"
        ))),
    }
}

/// A rejected editable fragment, carrying the dialect's own message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TypeScriptFragmentError(String);

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
    expression_fragment(text, &globals, &context.process_bindings()).map_err(|message| {
        GraphRenderError::InvalidExpression {
            node_id: node.id.to_string(),
            field,
            message,
        }
    })
}

/// The throwaway binding an `async` arrow field is parsed under.
const PROCESS_LITERAL_BINDING: &str = "__workflow_process_literal";

/// Whether an expression field is an `async` arrow, and so a process literal.
fn is_async_arrow_text(text: &str) -> bool {
    let rest = text.trim_start();
    rest.strip_prefix("async")
        .is_some_and(|rest| rest.starts_with([' ', '(', '\t', '\n']))
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
    let globals = node
        .available_variables
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    parse_typescript_assign_target(text, &globals, &BTreeSet::new()).map_err(|error| {
        GraphRenderError::InvalidAssignmentTarget {
            node_id: node.id.to_string(),
            field,
            message: error.to_string(),
        }
    })
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
