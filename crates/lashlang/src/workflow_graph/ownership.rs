//! The one structural walk from IR to workflow nodes.
//!
//! Every consumer that needs to know which expressions are visible statements
//! — node identity, the compiler's execution sites, the graph projector and a
//! dialect's printer — reads it from here. Structure comes only from IR forms
//! and [`StructuralRole`]s, never from a binding's spelling: a program whose
//! private binders are renamed walks identically (FIG-3571 law L3).

use rustc_hash::FxHashMap;

use crate::{AssignPathStep, AstPath, Expr, Program, StructuralRole};

use super::child_path;

/// The owner-relative structural path of one workflow node.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkflowNodePath(Vec<u32>);

impl WorkflowNodePath {
    fn from_indices(indices: &[u32]) -> Self {
        Self(indices.to_vec())
    }

    pub fn indices(&self) -> &[u32] {
        &self.0
    }
}

/// The assignment of every AST expression under a visible body to the
/// workflow node that owns it.
#[derive(Clone, Debug, Default)]
pub struct WorkflowOwnership {
    paths: FxHashMap<AstPath, WorkflowNodePath>,
}

impl WorkflowOwnership {
    pub fn path_for_ast(&self, ast_path: &AstPath) -> Option<&WorkflowNodePath> {
        self.paths.get(ast_path)
    }
}

/// Which child body of a container statement a [`WorkflowBody`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowBodySlot {
    Then,
    Else,
    LoopBody,
    ComprehensionElement,
}

/// One visible statement, in authored order.
#[derive(Clone, Debug)]
pub struct WorkflowStatement<'a> {
    /// The statement's expression, with no structural wrapper around it.
    pub expr: &'a Expr,
    /// The expression's rooted AST path.
    pub ast_path: AstPath,
    /// The owner-relative node path the statement mints its id from.
    pub node_path: WorkflowNodePath,
    /// The child bodies a container statement owns, in slot order.
    pub bodies: Vec<WorkflowBody<'a>>,
}

/// One ordered list of visible statements.
#[derive(Clone, Debug)]
pub struct WorkflowBody<'a> {
    pub slot: Option<WorkflowBodySlot>,
    /// The rooted AST path of the body expression.
    pub ast_path: AstPath,
    pub statements: Vec<WorkflowStatement<'a>>,
}

impl<'a> WorkflowStatement<'a> {
    /// The child body in `slot`, if this statement owns one.
    pub fn body(&self, slot: WorkflowBodySlot) -> Option<&WorkflowBody<'a>> {
        self.bodies.iter().find(|body| body.slot == Some(slot))
    }
}

/// A visible workflow body together with the ownership derived from it.
pub struct WorkflowProjection<'a> {
    body: WorkflowBody<'a>,
    ownership: WorkflowOwnership,
}

impl<'a> WorkflowProjection<'a> {
    /// The projection of `Program::main`.
    pub fn for_main(program: &'a Program) -> Self {
        workflow_projection(&program.main, AstPath::main(Vec::new()), Vec::new())
    }

    /// The projection of a process body at `ast_root`, reserving the empty
    /// path for the process container itself.
    ///
    /// A body wrapped in a [`StructuralRole::ProcessWrapper`] projects its
    /// authored run body, addressed through the wrapper. Any other body is
    /// projected whole from `[0]`, so no executable node can mint the process
    /// root's empty-path identifier.
    pub fn for_process(body: &'a Expr, ast_root: AstPath) -> Self {
        if let Expr::Role {
            role: StructuralRole::ProcessWrapper,
            expr,
        } = body
            && let Some((steps, run_body)) = crate::process_wrapper_run_path(expr)
        {
            let mut prefix = vec![0];
            prefix.extend(steps);
            let mut ast_path = ast_root;
            ast_path.steps.extend(prefix.iter().copied());
            return workflow_projection(run_body, ast_path, prefix);
        }
        workflow_projection(body, ast_root, vec![0])
    }

    /// The ordered visible hierarchy.
    pub fn body(&self) -> &WorkflowBody<'a> {
        &self.body
    }

    pub fn ownership_map(&self) -> &WorkflowOwnership {
        &self.ownership
    }

    pub(crate) fn into_ownership_map(self) -> WorkflowOwnership {
        self.ownership
    }
}

fn workflow_projection(
    expression: &Expr,
    ast_path: AstPath,
    node_path: Vec<u32>,
) -> WorkflowProjection<'_> {
    let mut paths = FxHashMap::default();
    let body = collect_body(expression, &ast_path, &node_path, None, &mut paths);
    WorkflowProjection {
        body,
        ownership: WorkflowOwnership { paths },
    }
}

/// One statement of a statement list: its expression and its
/// [`Expr::children`] index path from the list expression.
pub struct ListedStatement<'a> {
    pub expr: &'a Expr,
    pub steps: Vec<u32>,
}

/// The statements of a statement-list body, in order.
///
/// A [`StructuralRole::Completion`] contributes every element but its
/// completion value, and a plain `Block` every element. Any other expression,
/// a [`StructuralRole::Scope`] included, is a single statement. A statement
/// that is itself a completion list is flattened into the list one step
/// deeper, so a statement the front end gave a completion value is still the
/// statement it wraps.
pub fn statement_list(expression: &Expr) -> Vec<ListedStatement<'_>> {
    let mut out = Vec::new();
    push_statement_list(expression, &mut Vec::new(), &mut out);
    out
}

fn push_statement_list<'a>(
    expression: &'a Expr,
    steps: &mut Vec<u32>,
    out: &mut Vec<ListedStatement<'a>>,
) {
    let (items, prefix, completion) = match expression {
        Expr::Role {
            role: StructuralRole::Completion,
            expr,
        } => match expr.as_ref() {
            Expr::Block(items) => (items.as_slice(), Some(0), true),
            _ => return,
        },
        Expr::Block(items) => (items.as_slice(), None, false),
        expression => {
            out.push(ListedStatement {
                expr: expression,
                steps: steps.clone(),
            });
            return;
        }
    };
    if let Some(prefix) = prefix {
        steps.push(prefix);
    }
    let visible = if completion {
        &items[..items.len().saturating_sub(1)]
    } else {
        items
    };
    for (index, item) in visible.iter().enumerate() {
        steps.push(index as u32);
        if is_statement_list(item) {
            push_statement_list(item, steps, out);
        } else {
            out.push(ListedStatement {
                expr: item,
                steps: steps.clone(),
            });
        }
        steps.pop();
    }
    if prefix.is_some() {
        steps.pop();
    }
}

fn is_statement_list(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Role {
            role: StructuralRole::Completion,
            ..
        }
    )
}

fn collect_body<'a>(
    expression: &'a Expr,
    ast_path: &AstPath,
    node_base: &[u32],
    slot: Option<WorkflowBodySlot>,
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) -> WorkflowBody<'a> {
    // A body's structural wrappers and completion value are not statements:
    // they stay owned by whatever owns the body expression itself.
    let statements = statement_list(expression)
        .into_iter()
        .map(|listed| {
            let mut statement_ast = ast_path.clone();
            statement_ast.steps.extend(listed.steps.iter().copied());
            let mut node_path = node_base.to_vec();
            node_path.extend(listed.steps.iter().copied());
            collect_statement(listed.expr, statement_ast, &node_path, paths)
        })
        .collect();
    WorkflowBody {
        slot,
        ast_path: ast_path.clone(),
        statements,
    }
}

fn collect_statement<'a>(
    expression: &'a Expr,
    ast_path: AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) -> WorkflowStatement<'a> {
    map_node_subtree(expression, &ast_path, node_path, paths);

    let (value, value_ast, value_path) = statement_value(expression, &ast_path, node_path);
    let mut bodies = Vec::new();
    match value {
        Expr::If {
            then_block,
            else_block,
            ..
        } => {
            bodies.push(collect_body(
                then_block,
                &value_ast.child(1),
                &child_path(&value_path, 1),
                Some(WorkflowBodySlot::Then),
                paths,
            ));
            bodies.push(collect_body(
                else_block,
                &value_ast.child(2),
                &child_path(&value_path, 2),
                Some(WorkflowBodySlot::Else),
                paths,
            ));
        }
        Expr::For { bind, body, .. } => {
            let index = Expr::for_body_index(bind.as_deref());
            bodies.push(collect_body(
                body,
                &value_ast.child(index),
                &child_path(&value_path, index),
                Some(WorkflowBodySlot::LoopBody),
                paths,
            ));
        }
        Expr::While { body, .. } => bodies.push(collect_body(
            body,
            &value_ast.child(1),
            &child_path(&value_path, 1),
            Some(WorkflowBodySlot::LoopBody),
            paths,
        )),
        Expr::ListComprehension { element, clauses } => {
            let index = clauses.len() as u32;
            bodies.push(collect_body(
                element,
                &value_ast.child(index),
                &child_path(&value_path, index),
                Some(WorkflowBodySlot::ComprehensionElement),
                paths,
            ));
        }
        _ => {}
    }
    WorkflowStatement {
        expr: expression,
        ast_path,
        node_path: WorkflowNodePath::from_indices(node_path),
        bodies,
    }
}

/// The value a statement computes: through a label, then through an
/// assignment's target to its value, with the value's AST and node paths.
pub(crate) fn statement_value<'e>(
    expression: &'e Expr,
    ast_path: &AstPath,
    node_path: &[u32],
) -> (&'e Expr, AstPath, Vec<u32>) {
    let (expression, ast_path) = match expression {
        Expr::LabelAnnotated { expr, .. } => (expr.as_ref(), ast_path.child(0)),
        expression => (expression, ast_path.clone()),
    };
    match expression {
        Expr::Assign { target, expr } => {
            let value_index = target
                .steps
                .iter()
                .filter(|step| matches!(step, AssignPathStep::Index(_)))
                .count() as u32;
            (
                expr.as_ref(),
                ast_path.child(value_index),
                child_path(node_path, value_index),
            )
        }
        expression => (expression, ast_path, node_path.to_vec()),
    }
}

fn map_node_subtree(
    expression: &Expr,
    ast_path: &AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) {
    paths.insert(ast_path.clone(), WorkflowNodePath::from_indices(node_path));
    for (index, child) in expression.children().enumerate() {
        map_node_subtree(child, &ast_path.child(index as u32), node_path, paths);
    }
}

#[cfg(test)]
mod tests;
