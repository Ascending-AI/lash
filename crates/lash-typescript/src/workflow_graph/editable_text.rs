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

use lashlang::{AssignPathStep, AssignTarget, Expr, Program};

use super::printer::typescript_statement_source;
use super::{GraphRenderError, RenderContext, RenderScope, WorkflowNode};

pub(super) fn statement_text(expression: &Expr, bound: &[String]) -> String {
    typescript_statement_source(expression, bound)
        .unwrap_or_else(|error| format!("<non-sourceable statement: {error}>"))
}

/// Returns the parsed program and the number of leading statements the wrapper
/// itself contributed, which the caller skips.
pub(super) fn parse_typescript_fragment(
    node: &WorkflowNode,
    text: &str,
    context: RenderContext<'_>,
) -> Result<(Program, usize), GraphRenderError> {
    let names = fragment_bindings(node);
    // The session's globals are readable exactly as the cell's own link bound
    // them. A name the node itself carries is the fragment's binding, so the
    // session set drops it — which also keeps a process wrapper's `let`
    // re-declaration of it from being shadowed by the ambient one.
    let local: BTreeSet<String> = names.iter().cloned().collect();
    let session: BTreeSet<String> = context.globals.difference(&local).cloned().collect();
    let (source, globals, prelude) = match context.scope {
        RenderScope::Main => (text.to_string(), local, 0),
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
    let program =
        crate::parse_workflow_fragment(&source, &globals, &session, &context.process_bindings())
            .map_err(|error| GraphRenderError::InvalidOpaqueSource {
                node_id: node.id.to_string(),
                message: error.to_string(),
            })?;
    Ok((program, prelude))
}

/// The names a fragment may read, in the order the wrapper declares them.
fn fragment_bindings(node: &WorkflowNode) -> Vec<String> {
    node.available_variables.clone()
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
    let [Expr::Assign { target, expr }] =
        super::printer::statement_block_contents(&program.main).as_slice()
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
    let program = crate::parse_workflow_fragment(&fragment, globals, &BTreeSet::new(), processes)
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
    let program =
        crate::parse_workflow_fragment(&source, &BTreeSet::new(), &BTreeSet::new(), processes)
            .map_err(|error| TypeScriptFragmentError(error.to_string()))?;
    let Some(body) = opaque_wrapper_run_body(&program) else {
        return Err(TypeScriptFragmentError(
            "expected one process statement".to_string(),
        ));
    };
    let statements = super::printer::statement_block_contents(body)
        .into_iter()
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

/// The throwaway binding an `async` arrow field is parsed under.
const PROCESS_LITERAL_BINDING: &str = "__workflow_process_literal";

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

/// The dialect has no production for a bare target, so the text is parsed as
/// the member expression it is and converted; that keeps one grammar in play
/// rather than a second hand-written path parser.
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
