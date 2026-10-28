//! The source node of each lowered operation. Placement changes only operands;
//! it must not change the node whose holding event that operation implements.

use super::{OpRange, Stream, room};
use lash_kernel_doc::{Expr, Node, Stmt};
use lash_kernel_exec::Op;
use lash_kernel_value::sites::Site;
use std::collections::BTreeMap;
use std::sync::Arc;

impl Stream {
    /// Cold verification against the independent source tree. Ordinary worker
    /// activation does not walk that tree or force its memory description.
    pub(crate) fn verify_holding_sites(
        &self,
        source: &room::Source,
        range: OpRange,
        regs: u32,
    ) -> bool {
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
        self.verify_source_flow(source, range, regs)
    }

    fn verify_source_flow(&self, source: &room::Source, range: OpRange, regs: u32) -> bool {
        let mut pending = vec![(range.start, vec![None::<Arc<Site>>; regs as usize])];
        let mut seen = BTreeMap::new();
        while let Some((pc, mut owners)) = pending.pop() {
            if pc == range.end() {
                continue;
            }
            if pc > range.end() {
                return false;
            }
            if let Some(previous) = seen.insert(pc, owners.clone()) {
                if previous != owners {
                    return false;
                }
                continue;
            }
            let op = self.ops[pc as usize];
            let expected = self.source_sites[pc as usize].as_ref().and_then(|site| {
                let mut node = Node::Block(&source.body);
                for index in &site.path[source.site.path.len()..] {
                    node = *node.children().get(*index as usize)?;
                }
                inputs(op, site, node)
            });
            for (index, reg) in op.reads().into_iter().enumerate() {
                let Some(owner) = owners.get_mut(reg.ix()) else {
                    return false;
                };
                if owner.is_none() {
                    return false;
                }
                if let Some(expected) = &expected
                    && owner.as_deref() != expected.get(index)
                {
                    return false;
                }
                if !matches!(op, Op::Pin { .. }) {
                    *owner = None;
                }
            }
            if let Some(reg) = op.writes() {
                let Some(owner) = owners.get_mut(reg.ix()) else {
                    return false;
                };
                *owner = self.source_sites[pc as usize].clone();
            }
            if let Some(joins) = self.source_joins.get(&pc) {
                for join in joins {
                    let Some(owner) = owners.get_mut(join.value.ix()) else {
                        return false;
                    };
                    if owner.as_ref() != Some(&join.from)
                        || join.from.unit != join.to.unit
                        || join.from.path.len() != join.to.path.len() + 1
                        || !join.from.path.starts_with(&join.to.path)
                        || !join.to.path.starts_with(&source.site.path)
                    {
                        return false;
                    }
                    let mut node = Node::Block(&source.body);
                    for index in &join.to.path[source.site.path.len()..] {
                        let Some(child) = node.children().get(*index as usize).copied() else {
                            return false;
                        };
                        node = child;
                    }
                    if !matches!(
                        (node, join.from.path.last()),
                        (Node::Expr(Expr::If { .. }), Some(1 | 2))
                            | (Node::Expr(Expr::Let { .. }), Some(1))
                    ) {
                        return false;
                    }
                    *owner = Some(Arc::clone(&join.to));
                }
            }
            let next = pc + 1;
            match op {
                Op::Jump { skip } => pending.push((next + skip, owners)),
                Op::JumpIfFalse { skip, .. } => {
                    pending.push((next + skip, owners.clone()));
                    pending.push((next, owners));
                }
                _ => pending.push((next, owners)),
            }
        }
        true
    }
}

fn inputs(op: Op, site: &Site, node: Node<'_>) -> Option<Vec<Site>> {
    let count = match op {
        Op::MakeTuple { items }
        | Op::MakeList { items }
        | Op::MakeSet { items }
        | Op::MakeMap { entries: items }
        | Op::MakeRecord { values: items, .. }
        | Op::Call { args: items, .. }
        | Op::Operator { args: items, .. } => items.len,
        Op::Nested { args, .. } => {
            if let Node::Expr(Expr::Act(act)) = node
                && args.len as usize == act.args.len() + 1
            {
                return Some(
                    std::iter::once(site.clone())
                        .chain((0..act.args.len() as u32).map(|index| site.child(index)))
                        .collect(),
                );
            }
            args.len
        }
        Op::Field { .. }
        | Op::LetBind { .. }
        | Op::LetBindShared { .. }
        | Op::JumpIfFalse { .. }
        | Op::Branch { .. }
        | Op::Test { .. }
        | Op::ForIn { .. }
        | Op::Bind { .. }
        | Op::BindShared { .. }
        | Op::BindSession { .. }
        | Op::WriteLocal { .. }
        | Op::WriteSession { .. }
        | Op::WriteUnbound { .. }
        | Op::RemoveField { .. }
        | Op::Return { .. }
        | Op::ReturnVia { .. }
        | Op::Throw { .. }
        | Op::Print { .. }
        | Op::Finish { .. }
        | Op::Fail { .. } => 1,
        Op::Index { .. } | Op::SetField { .. } | Op::RemoveIndex { .. } => 2,
        Op::SetIndex { .. } => 3,
        Op::Pin { .. } => {
            return Some(vec![
                site.child(node.children().len().checked_sub(1)? as u32),
            ]);
        }
        _ => return None,
    };
    Some((0..count).map(|index| site.child(index)).collect())
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
