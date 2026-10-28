//! The source node of each lowered operation. Placement changes only operands;
//! it must not change the node whose holding event that operation implements.

use super::{OpRange, Stream, room};
use lash_kernel_doc::{Expr, Node, Stmt};
use lash_kernel_exec::Op;

impl Stream {
    /// Cold verification against the independent source tree. Ordinary worker
    /// activation does not walk that tree or force its memory description.
    pub(super) fn verify_holding_sites(&self, source: &room::Source, range: OpRange) -> bool {
        if self.source_sites.len() != self.ops.len() {
            return false;
        }
        for pc in range.start..range.end() {
            let op = self.ops[pc as usize];
            let site = &self.source_sites[pc as usize];
            if matches!(op, Op::Region { .. }) {
                if site.is_some() {
                    return false;
                }
                continue;
            }
            let Some(site) = site else {
                return false;
            };
            if site.unit != source.site.unit || !site.path.starts_with(&source.site.path) {
                return false;
            }
            let mut node = Node::Block(&source.body);
            for index in &site.path[source.site.path.len()..] {
                let Some(child) = node.children().get(*index as usize).copied() else {
                    return false;
                };
                node = child;
            }
            if !implements(op, node) {
                return false;
            }
        }
        true
    }
}

fn implements(op: Op, node: Node<'_>) -> bool {
    match op {
        Op::Const { .. } => matches!(node, Node::Expr(Expr::Literal(_) | Expr::Call { .. })),
        // A dynamic callee read belongs to the action's own coordinate. Its
        // arguments retain their separate child coordinates.
        Op::Local { .. } | Op::Session { .. } => {
            matches!(node, Node::Expr(Expr::Variable(_) | Expr::Act(_)))
        }
        Op::Unbound { .. } => true,
        Op::Field { .. } | Op::Index { .. } => matches!(node, Node::Expr(Expr::Member(_))),
        Op::MakeTuple { .. } => matches!(node, Node::Expr(Expr::Tuple(_))),
        Op::MakeList { .. } => matches!(node, Node::Expr(Expr::List(_))),
        Op::MakeSet { .. } => matches!(node, Node::Expr(Expr::Set(_))),
        Op::MakeMap { .. } => matches!(node, Node::Expr(Expr::Map(_))),
        Op::MakeRecord { .. } => matches!(node, Node::Expr(Expr::Record(_))),
        Op::Closure { .. } => matches!(node, Node::Expr(Expr::Closure(_))),
        Op::Call { .. } | Op::Operator { .. } => matches!(node, Node::Expr(Expr::Call { .. })),
        Op::Nested { .. } => matches!(node, Node::Expr(Expr::Call { .. } | Expr::Act(_))),
        Op::Pin { .. }
        | Op::WriteLocal { .. }
        | Op::WriteSession { .. }
        | Op::WriteUnbound { .. }
        | Op::SetField { .. }
        | Op::SetIndex { .. } => {
            matches!(node, Node::Stmt(Stmt::Assign { .. }))
        }
        Op::Act => matches!(node, Node::Action(_)),
        Op::Bind { .. } | Op::BindShared { .. } | Op::BindSession { .. } => {
            matches!(node, Node::Stmt(Stmt::Let { .. }))
        }
        Op::RemoveField { .. } | Op::RemoveIndex { .. } => {
            matches!(node, Node::Stmt(Stmt::Remove { .. }))
        }
        Op::Jump { .. } | Op::JumpIfFalse { .. } => matches!(node, Node::Expr(Expr::If { .. })),
        Op::LetBind { .. } | Op::LetBindShared { .. } | Op::LetEnd { .. } => {
            matches!(node, Node::Expr(Expr::Let { .. }))
        }
        Op::Branch { .. } => matches!(node, Node::Stmt(Stmt::If { .. })),
        Op::ForIn { .. } => matches!(node, Node::Stmt(Stmt::For { .. })),
        Op::While | Op::Test { .. } => matches!(node, Node::Stmt(Stmt::While { .. })),
        Op::Try { .. } => matches!(node, Node::Stmt(Stmt::Try(_))),
        Op::Break => matches!(node, Node::Stmt(Stmt::Break)),
        Op::Continue => matches!(node, Node::Stmt(Stmt::Continue)),
        Op::Return { .. } => matches!(node, Node::Stmt(Stmt::Return { .. })),
        Op::ReturnVia { .. } => matches!(node, Node::Stmt(Stmt::ReturnVia { .. })),
        Op::Throw { .. } => matches!(node, Node::Stmt(Stmt::Throw { .. })),
        Op::Print { .. } => matches!(node, Node::Stmt(Stmt::Print { .. })),
        Op::Finish { .. } => matches!(node, Node::Stmt(Stmt::Finish { .. })),
        Op::Fail { .. } => matches!(node, Node::Stmt(Stmt::Fail { .. })),
        Op::End => matches!(node, Node::Stmt(Stmt::Do { .. })),
        Op::Region { .. } => false,
    }
}
