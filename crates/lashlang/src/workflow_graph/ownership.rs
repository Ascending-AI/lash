use rustc_hash::FxHashMap;

use crate::runtime::{is_pure_expr, lowered_for_of_parts};
use crate::{AssignPathStep, AstPath, Expr, Program};

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

/// The single IR-derived assignment of AST expressions to workflow nodes.
#[derive(Clone, Debug, Default)]
pub struct WorkflowOwnership {
    paths: FxHashMap<AstPath, WorkflowNodePath>,
}

impl WorkflowOwnership {
    pub fn path_for_ast(&self, ast_path: &AstPath) -> Option<&WorkflowNodePath> {
        self.paths.get(ast_path)
    }
}

/// A visible workflow body together with the ownership map derived from it.
pub struct WorkflowProjection<'a> {
    expression: &'a Expr,
    ast_path: AstPath,
    ownership: WorkflowOwnership,
}

impl<'a> WorkflowProjection<'a> {
    pub fn for_main(expression: &'a Expr, ast_root: AstPath) -> Self {
        workflow_projection(expression, ast_root, Vec::new())
    }

    pub fn visible_expression(&self) -> &'a Expr {
        self.expression
    }

    pub fn ast_root(&self) -> &AstPath {
        &self.ast_path
    }

    pub fn ownership_map(&self) -> &WorkflowOwnership {
        &self.ownership
    }

    pub(crate) fn into_ownership_map(self) -> WorkflowOwnership {
        self.ownership
    }
}

pub(crate) fn main_workflow_projection(program: &Program) -> WorkflowProjection<'_> {
    WorkflowProjection::for_main(&program.main, AstPath::main(Vec::new()))
}

/// Projects a process body while reserving the empty path for its container.
///
/// Lowered TypeScript processes retain the real wrapper prefix. Direct IR
/// processes begin at `[0]`, so no executable body node can mint the process
/// root's empty-path identifier.
pub fn process_workflow_projection(expression: &Expr, ast_root: AstPath) -> WorkflowProjection<'_> {
    match process_execution_body_path(expression) {
        Some((prefix, body)) => {
            let mut ast_path = ast_root;
            ast_path.steps.extend(prefix.iter().copied());
            workflow_projection(body, ast_path, prefix)
        }
        None => workflow_projection(expression, ast_root, vec![0]),
    }
}

fn workflow_projection(
    expression: &Expr,
    ast_path: AstPath,
    node_path: Vec<u32>,
) -> WorkflowProjection<'_> {
    let mut paths = FxHashMap::default();
    collect_workflow_block_paths(expression, &ast_path, &node_path, &mut paths);
    WorkflowProjection {
        expression,
        ast_path,
        ownership: WorkflowOwnership { paths },
    }
}

fn process_execution_body_path(wrapper: &Expr) -> Option<(Vec<u32>, &Expr)> {
    let Expr::Try(try_expr) = wrapper else {
        return None;
    };
    let crate::TryExpr {
        body,
        catch: Some(crate::CatchClause {
            binding,
            body: catch,
        }),
        finally: None,
    } = try_expr.as_ref()
    else {
        return None;
    };
    let Expr::Fail(caught) = catch.as_ref() else {
        return None;
    };
    if !matches!(caught.as_ref(), Expr::Variable(name) if name == binding) {
        return None;
    }

    let mut path = Vec::new();
    let finish = body.as_ref();
    push_child_index(wrapper, finish, &mut path)?;
    let Expr::Finish(call) = finish else {
        return None;
    };
    let call = call.as_ref();
    push_child_index(finish, call, &mut path)?;
    let Expr::Call { function, .. } = call else {
        return None;
    };
    let function = function.as_ref();
    push_child_index(call, function, &mut path)?;
    let Expr::Function(run) = function else {
        return None;
    };
    push_child_index(function, &run.body, &mut path)?;
    Some((path, &run.body))
}

fn push_child_index(parent: &Expr, child: &Expr, path: &mut Vec<u32>) -> Option<()> {
    let index = parent.children().position(|candidate| {
        std::ptr::eq(std::ptr::from_ref(candidate), std::ptr::from_ref(child))
    })?;
    path.push(u32::try_from(index).ok()?);
    Some(())
}

fn collect_workflow_block_paths(
    expression: &Expr,
    ast_path: &AstPath,
    base_path: &[u32],
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) {
    let mut expression = expression;
    let mut ast_path = ast_path.clone();
    let mut base_path = base_path.to_vec();
    let mut unwrapped = false;
    while let Some(inner) = workflow_block_wrapper_inner(expression) {
        expression = inner;
        ast_path = ast_path.child(0);
        base_path.push(0);
        unwrapped = true;
    }
    let mut expressions = match expression {
        Expr::Block(expressions) => expressions.as_slice(),
        expression => std::slice::from_ref(expression),
    };
    if let [rest @ .., last] = expressions
        && (matches!(last, Expr::Undefined) || (unwrapped && is_pure_expr(last)))
    {
        expressions = rest;
    }
    let start = matches!(expression, Expr::Block(_)).then_some(0);
    collect_workflow_statement_paths(expressions, &ast_path, &base_path, start, paths);
}

fn collect_workflow_statement_paths(
    expressions: &[Expr],
    ast_base: &AstPath,
    node_base: &[u32],
    start: Option<u32>,
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) {
    for (index, expression) in expressions.iter().enumerate() {
        let mut statement_ast_path = ast_base.clone();
        let mut node_path = node_base.to_vec();
        if let Some(start) = start {
            let step = start + index as u32;
            statement_ast_path = statement_ast_path.child(step);
            node_path.push(step);
        }
        let mut statement = expression;
        while let Some(inner) = authored_workflow_statement(statement) {
            statement = inner;
            statement_ast_path = statement_ast_path.child(0);
            node_path.push(0);
        }
        collect_workflow_node_paths(statement, &statement_ast_path, &node_path, paths);
    }
}

fn collect_workflow_node_paths(
    expression: &Expr,
    ast_path: &AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) {
    map_workflow_node_subtree(expression, ast_path, node_path, paths);

    let (expression, expression_ast_path) = match expression {
        Expr::LabelAnnotated { expr, .. } => (expr.as_ref(), ast_path.child(0)),
        expression => (expression, ast_path.clone()),
    };
    let (value, value_ast_path, value_path) = match expression {
        Expr::Assign { target, expr } => {
            let value_index = target
                .steps
                .iter()
                .filter(|step| matches!(step, AssignPathStep::Index(_)))
                .count() as u32;
            (
                expr.as_ref(),
                expression_ast_path.child(value_index),
                child_path(node_path, value_index),
            )
        }
        expression => (expression, expression_ast_path, node_path.to_vec()),
    };

    match value {
        Expr::If {
            then_block,
            else_block,
            ..
        } => {
            collect_workflow_block_paths(
                then_block,
                &value_ast_path.child(1),
                &child_path(&value_path, 1),
                paths,
            );
            collect_workflow_block_paths(
                else_block,
                &value_ast_path.child(2),
                &child_path(&value_path, 2),
                paths,
            );
        }
        Expr::For {
            binding,
            iterable,
            body,
        } => {
            let body_ast_base = value_ast_path.child(1);
            let body_node_base = child_path(&value_path, 1);
            match lowered_for_of_parts(binding, iterable, body) {
                Some((_, _, [single])) if matches!(single, Expr::Block(_)) => {
                    collect_workflow_block_paths(
                        single,
                        &body_ast_base.child(1),
                        &child_path(&body_node_base, 1),
                        paths,
                    );
                }
                Some((_, _, rest)) => collect_workflow_statement_paths(
                    rest,
                    &body_ast_base,
                    &body_node_base,
                    Some(1),
                    paths,
                ),
                None => collect_workflow_block_paths(body, &body_ast_base, &body_node_base, paths),
            }
        }
        Expr::While { body, .. } => collect_workflow_block_paths(
            body,
            &value_ast_path.child(1),
            &child_path(&value_path, 1),
            paths,
        ),
        Expr::ListComprehension { element, clauses } => {
            let index = clauses.len() as u32;
            collect_workflow_block_paths(
                element,
                &value_ast_path.child(index),
                &child_path(&value_path, index),
                paths,
            );
        }
        _ => {}
    }
}

fn map_workflow_node_subtree(
    expression: &Expr,
    ast_path: &AstPath,
    node_path: &[u32],
    paths: &mut FxHashMap<AstPath, WorkflowNodePath>,
) {
    paths.insert(ast_path.clone(), WorkflowNodePath::from_indices(node_path));
    for (index, child) in expression.children().enumerate() {
        map_workflow_node_subtree(child, &ast_path.child(index as u32), node_path, paths);
    }
}

fn workflow_block_wrapper_inner(expression: &Expr) -> Option<&Expr> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    let inner = match statements.as_slice() {
        [inner @ Expr::Block(_), Expr::Undefined] | [inner @ Expr::Block(_)] => inner,
        _ => return None,
    };
    (!is_lowered_member_assignment(inner)).then_some(inner)
}

fn authored_workflow_statement(expression: &Expr) -> Option<&Expr> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    match statements.as_slice() {
        [single, last] if is_pure_expr(last) && !is_lowered_member_assignment(expression) => {
            Some(single)
        }
        _ => None,
    }
}

fn is_lowered_member_assignment(expression: &Expr) -> bool {
    let Expr::Block(statements) = expression else {
        return false;
    };
    let [
        Expr::Assign {
            target: base_target,
            expr: base,
        },
        Expr::Assign {
            target: result_target,
            ..
        },
        Expr::Assign {
            target: store,
            expr: stored,
        },
        Expr::Variable(completion),
    ] = statements.as_slice()
    else {
        return false;
    };
    base_target.root.starts_with("__typescript_")
        && result_target.root.starts_with("__typescript_")
        && store.root == base_target.root
        && !store.steps.is_empty()
        && matches!(stored.as_ref(), Expr::Variable(name) if *name == result_target.root)
        && matches!(base.as_ref(), Expr::Variable(_))
        && *completion == result_target.root
}
