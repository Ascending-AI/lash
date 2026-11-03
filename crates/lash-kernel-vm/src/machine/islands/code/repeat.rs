//! Fixed recurrence graphs. These descriptors have named graph positions,
//! not an instruction array. Other graphs use the ordinary packet stream.

use super::super::compiled::{OperatorSite, operator_site};
use super::*;
use crate::compile::LibRun;
use crate::compile::expr::{Expr, Kind as Expression};
use crate::compile::islands::{Kind as StatementKind, Statement};

mod form;
pub(in crate::machine::islands) mod proof;
mod verify;

/// Values read by the fixed graph. Invariants cannot be written by its body.
#[derive(Clone, Copy, Debug)]
pub(in crate::machine::islands) enum Input {
    Counter,
    Previous,
    Accumulator,
    Literal(Value),
    Invariant(u32),
}

/// The canonical primitive and its scalar fee table, never a private price.
pub(in crate::machine::islands) struct Scalar {
    pub(in crate::machine::islands) site: Option<OperatorSite>,
    pub(in crate::machine::islands) pair: Option<lash_kernel_doc::scalar::Pair>,
}

pub(in crate::machine::islands) struct Binary {
    pub(in crate::machine::islands) scalar: Scalar,
    pub(in crate::machine::islands) inputs: [Input; 2],
}

pub(in crate::machine::islands) struct Copy {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) keep: bool,
    pub(in crate::machine::islands) unary: Option<Scalar>,
}

pub(in crate::machine::islands) struct Update {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) operation: Binary,
}

pub(in crate::machine::islands) struct Reduction {
    pub(in crate::machine::islands) at: u32,
    pub(in crate::machine::islands) scale: Option<Binary>,
    pub(in crate::machine::islands) fold: Scalar,
    pub(in crate::machine::islands) term: Input,
    pub(in crate::machine::islands) reverse: bool,
}

pub(in crate::machine::islands) struct Repeat {
    pub(in crate::machine::islands) source_room: std::sync::Arc<crate::compile::room::Source>,
    pub(in crate::machine::islands) source_site: lash_kernel_value::sites::Site,
    pub(in crate::machine::islands) proof: proof::Proof,
    pub(in crate::machine::islands) test: Binary,
    pub(in crate::machine::islands) before: Option<Copy>,
    pub(in crate::machine::islands) copy_after_reduction: bool,
    pub(in crate::machine::islands) reduction: Option<Reduction>,
    pub(in crate::machine::islands) update: Update,
    pub(in crate::machine::islands) after: Option<Copy>,
}

fn arguments(expr: &Expr) -> Option<&[Expr]> {
    match &unpin(expr).kind {
        Expression::Operator { args, .. } | Expression::Call { args, .. } => Some(args),
        _ => None,
    }
}
impl Scalar {
    fn of(expr: &Expr, exe: &Executable, comparison: bool) -> Option<Self> {
        let args = arguments(expr)?;
        let site = operator_site(expr, exe);
        let pair = match &site {
            Some(site) => site.pair(exe),
            None => {
                let Expression::Call {
                    callee: crate::compile::expr::Callee::Declared(code),
                    ..
                } = expr.kind
                else {
                    return None;
                };
                let source = &exe.code(code).source_room;
                let mut pending = vec![lash_kernel_doc::Node::Block(&source.body)];
                let mut pair = None;
                while let Some(node) = pending.pop() {
                    if let lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call { function, args: inner }) = node
                        && inner.len() == 2 && source.params.len() == 2
                        && inner.iter().zip(&source.params).all(|(arg, param)| matches!(arg, lash_kernel_doc::Expr::Variable(name) if name == param))
                        && let Some(lib) = exe.library_lib(function)
                        && let Some(candidate) = exe.lib(lib).kernels(exe).pairs[0]
                        && [candidate.ints, candidate.floats].into_iter().flatten().any(|op| op.compares() == comparison)
                    { pair = Some(candidate); }
                    pending.extend(node.children());
                }
                pair
            }
        };
        match args.len() {
            1 if !comparison => {}
            2 if [pair?.ints, pair?.floats]
                .into_iter()
                .flatten()
                .any(|op| op.compares() == comparison) => {}
            _ => return None,
        }
        Some(Self { site, pair })
    }
}

fn unpin(mut expr: &Expr) -> &Expr {
    while let Expression::Pin(value) = &expr.kind {
        expr = value;
    }
    expr
}

fn local(expr: &Expr) -> Option<u32> {
    match unpin(expr).kind {
        Expression::Local {
            local,
            scoped: false,
            ..
        } => Some(local.at),
        _ => None,
    }
}

fn write(statement: &Statement) -> Option<(u32, bool, &Expr)> {
    match &statement.kind {
        StatementKind::Write {
            at, bind, value, ..
        } => {
            let value = if matches!(unpin(value).kind, Expression::Act) {
                statement.root.as_ref()?
            } else {
                value
            };
            Some((*at, *bind, unpin(value)))
        }
        _ => None,
    }
}
