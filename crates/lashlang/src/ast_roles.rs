//! Language-neutral structural roles (FIG-3571).
//!
//! A front end marks the IR it generates with these roles so that no consumer
//! has to infer structure from a binding's spelling. Each role names its shape,
//! and [`super::validate_ast`] refuses a role whose expression lacks it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    AssignPathStep, AstPath, AstString, CatchClause, Declaration, Expr, InvalidAst, Program,
    TryExpr,
};

/// The origin of a [`super::ProcessDecl`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ProcessOrigin {
    /// Authored as a module declaration.
    #[default]
    Declared,
    /// Lifted by the linker from the inline process literal at `site`, the
    /// literal's path in the program before lifting. The lifted declaration's
    /// name digests that literal's body and site ([`super::lifted_process_identity`]).
    Lifted { site: AstPath },
}

impl ProcessOrigin {
    pub fn is_declared(&self) -> bool {
        matches!(self, Self::Declared)
    }

    pub fn is_lifted(&self) -> bool {
        matches!(self, Self::Lifted { .. })
    }
}

/// The structural roles a front end marks its generated IR with.
///
/// Each role is language-neutral: it names what a shape does, not the source
/// construct that produced it, and each front end chooses which of its
/// constructs lower to which role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum StructuralRole {
    /// An authored nested statement scope. `expr` is a `Block` whose every
    /// element is a statement.
    Scope,
    /// A statement list closed by a generated completion value. `expr` is a
    /// non-empty `Block`; every element but the last is a statement, and the
    /// last is the pure completion value the list evaluates to.
    Completion,
    /// A member assignment `root.path = value` that pins its reference base
    /// before evaluating the value. `expr` is `Block([base = object,
    /// (key = index)?, result = value, base.step = result, result])`, where
    /// `step` reads `key` when present.
    AttributeAssign,
    /// A callback-driven collection transform (`map`, `filter`, ...). `expr`
    /// is a `Block` that binds the receiver, then the callback, and evaluates
    /// to the transform's result. `operation` is the front end's name for the
    /// transform; structure never depends on it.
    CollectionTransform { operation: AstString },
    /// The process failure wrapper around an authored run body. `expr` is
    /// `Try { body: Finish(Call { function: run, args }), catch e: Fail(e) }`,
    /// where `run` is a `Function` or a builtin call whose first argument is
    /// one; the run function's body is the authored process body.
    ProcessWrapper,
}

pub(super) fn check_program_roles(program: &Program) -> Result<(), InvalidAst> {
    let mut pending = vec![&program.main];
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => pending.push(&process.body),
            Declaration::Function(function) => pending.push(&function.body),
            Declaration::Type(_) => {}
        }
    }
    while let Some(expr) = pending.pop() {
        if let Expr::Role { role, expr } = expr {
            role.check_shape(expr)?;
        }
        pending.extend(expr.children());
    }
    Ok(())
}

impl StructuralRole {
    /// The role's name as diagnostics and hashes spell it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Completion => "completion",
            Self::AttributeAssign => "attribute_assign",
            Self::CollectionTransform { .. } => "collection_transform",
            Self::ProcessWrapper => "process_wrapper",
        }
    }

    /// Refuses `expr` unless it has this role's shape.
    pub fn check_shape(&self, expr: &Expr) -> Result<(), InvalidAst> {
        let malformed = |reason| InvalidAst::MalformedRole {
            role: self.name(),
            reason,
        };
        match self {
            Self::Scope => match expr {
                Expr::Block(_) => Ok(()),
                _ => Err(malformed("a scope wraps a block")),
            },
            Self::Completion => match expr {
                Expr::Block(items) if items.last().is_some_and(crate::is_pure_expr) => Ok(()),
                Expr::Block(_) => Err(malformed("a completion block ends with a pure value")),
                _ => Err(malformed("a completion wraps a block")),
            },
            Self::AttributeAssign => AttributeAssignParts::of(expr)
                .map(|_| ())
                .ok_or_else(|| malformed("an attribute assignment pins a base, evaluates a value, stores through the base and yields the value")),
            Self::CollectionTransform { .. } => match expr {
                Expr::Block(items)
                    if items.len() >= 3
                        && matches!(&items[0], Expr::Assign { target, .. } if target.is_simple())
                        && matches!(&items[1], Expr::Assign { target, .. } if target.is_simple()) =>
                {
                    Ok(())
                }
                _ => Err(malformed(
                    "a collection transform binds its receiver and callback before it runs",
                )),
            },
            Self::ProcessWrapper => process_wrapper_run_path(expr)
                .map(|_| ())
                .ok_or_else(|| malformed("a process wrapper finishes with its run call and fails with what it catches")),
        }
    }
}

/// The authored parts of a [`StructuralRole::AttributeAssign`] block.
#[derive(Clone, Copy, Debug)]
pub struct AttributeAssignParts<'a> {
    /// The object whose attribute is written.
    pub object: &'a Expr,
    /// The written attribute: a field name, or an index expression.
    pub step: AttributeStep<'a>,
    /// The assigned value.
    pub value: &'a Expr,
    /// The [`Expr::children`] index of `value` inside the role's block.
    pub value_index: u32,
}

/// The attribute an [`AttributeAssignParts`] writes.
#[derive(Clone, Copy, Debug)]
pub enum AttributeStep<'a> {
    Field(&'a AstString),
    Index(&'a Expr),
}

impl<'a> AttributeAssignParts<'a> {
    /// Reads the parts of an attribute-assignment block, or `None` when the
    /// block does not have the role's shape.
    pub fn of(expr: &'a Expr) -> Option<Self> {
        let Expr::Block(items) = expr else {
            return None;
        };
        let (base, key, result, store, completion) = match items.as_slice() {
            [base, result, store, completion] => (base, None, result, store, completion),
            [base, key, result, store, completion] => (base, Some(key), result, store, completion),
            _ => return None,
        };
        let Expr::Assign {
            target: base_target,
            expr: object,
        } = base
        else {
            return None;
        };
        let Expr::Assign {
            target: result_target,
            expr: value,
        } = result
        else {
            return None;
        };
        let Expr::Assign {
            target: store_target,
            expr: stored,
        } = store
        else {
            return None;
        };
        if !base_target.is_simple()
            || !result_target.is_simple()
            || store_target.root != base_target.root
            || !matches!(stored.as_ref(), Expr::Variable(name) if *name == result_target.root)
            || !matches!(completion, Expr::Variable(name) if *name == result_target.root)
        {
            return None;
        }
        let step = match (store_target.steps.as_slice(), key) {
            ([AssignPathStep::Field(field)], None) => AttributeStep::Field(field),
            (
                [AssignPathStep::Index(Expr::Variable(read))],
                Some(Expr::Assign {
                    target: key_target,
                    expr: index,
                }),
            ) if key_target.is_simple() && *read == key_target.root => AttributeStep::Index(index),
            _ => return None,
        };
        Some(Self {
            object,
            step,
            value,
            value_index: if key.is_some() { 2 } else { 1 },
        })
    }
}

/// The [`Expr::children`] path from a [`StructuralRole::ProcessWrapper`]'s
/// wrapped `Try` to its run function's body, and that body.
pub fn process_wrapper_run_path(wrapper: &Expr) -> Option<(Vec<u32>, &Expr)> {
    let Expr::Try(scope) = wrapper else {
        return None;
    };
    let TryExpr {
        body,
        catch: Some(CatchClause {
            binding,
            body: catch,
        }),
        finally: None,
    } = scope.as_ref()
    else {
        return None;
    };
    let Expr::Fail(caught) = catch.as_ref() else {
        return None;
    };
    if !matches!(caught.as_ref(), Expr::Variable(name) if name == binding) {
        return None;
    }
    // Try's children are [body, catch body]; the run call sits in `body`.
    let Expr::Finish(call) = body.as_ref() else {
        return None;
    };
    let Expr::Call { function, .. } = call.as_ref() else {
        return None;
    };
    // Finish -> Call (child 0) -> function (child 0 of the call).
    let mut path = vec![0, 0, 0];
    let run = match function.as_ref() {
        Expr::Function(run) => run,
        Expr::BuiltinCall { args, .. } => match args.first() {
            Some(Expr::Function(run)) => {
                path.push(0);
                run
            }
            _ => return None,
        },
        _ => return None,
    };
    // The function's only child is its body.
    path.push(0);
    Some((path, &run.body))
}
