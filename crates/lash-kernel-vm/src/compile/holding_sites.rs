//! The source node of each lowered operation. Placement changes only operands;
//! it must not change the node whose holding event that operation implements.

use super::{Form, OpRange, Stmt as CompiledStmt, Stream, room};
use lash_kernel_doc::{Expr, Node, Stmt};
use lash_kernel_exec::Op;
use lash_kernel_value::sites::Site;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

pub(crate) type SourceRoleList = Arc<[(lash_kernel_exec::Reg, Arc<Site>)]>;
#[derive(Default)]
pub(crate) struct SourceRoles {
    roles: BTreeMap<u32, SourceRoleList>,
    verified: Vec<VerifiedHolding>,
}

struct VerifiedHolding {
    body: Weak<lash_kernel_doc::Block>,
    site: Site,
    range: OpRange,
    regs: u32,
    ops: Box<[Op]>,
    sites: Box<[Option<Arc<Site>>]>,
    joins: BTreeMap<u32, Vec<super::SourceJoin>>,
}
impl VerifiedHolding {
    fn matches(
        &mut self,
        stream: &Stream,
        source: &room::Source,
        range: OpRange,
        regs: u32,
    ) -> bool {
        let matches = self.range == range
            && self.regs == regs
            && self.site == source.site
            && (self.body.as_ptr() == Arc::as_ptr(&source.body)
                || self.body.upgrade().is_some_and(|body| body == source.body))
            && self.ops.as_ref()
                == stream
                    .ops
                    .get(range.start as usize..range.end() as usize)
                    .unwrap_or(&[])
            && self.sites.as_ref()
                == stream
                    .source_sites
                    .get(range.start as usize..range.end() as usize)
                    .unwrap_or(&[])
            && stream.source_sites.len() == stream.ops.len()
            && self.joins.len() == stream.source_joins.range(range.start..range.end()).count()
            && self
                .joins
                .iter()
                .zip(stream.source_joins.range(range.start..range.end()))
                .all(|((pc, joins), (other_pc, other))| {
                    pc == other_pc
                        && joins.len() == other.len()
                        && joins
                            .iter()
                            .zip(other)
                            .all(|(a, b)| a.from == b.from && a.to == b.to && a.value == b.value)
                });
        if matches && self.body.as_ptr() != Arc::as_ptr(&source.body) {
            self.body = Arc::downgrade(&source.body);
        }
        matches
    }
}

/// Child lists belong to this verification only, so altered streams still
/// resolve every site against the independent source tree.
struct SourceNodes<'a> {
    source: &'a room::Source,
    children: BTreeMap<Vec<u32>, Vec<Node<'a>>>,
}

impl<'a> SourceNodes<'a> {
    fn at(&mut self, site: &Site) -> Option<Node<'a>> {
        if site.unit != self.source.site.unit || !site.path.starts_with(&self.source.site.path) {
            return None;
        }
        let path = &site.path[self.source.site.path.len()..];
        let mut node = Node::Block(&self.source.body);
        for (depth, index) in path.iter().enumerate() {
            let children = self
                .children
                .entry(path[..depth].to_vec())
                .or_insert_with(|| node.children());
            node = *children.get(*index as usize)?;
        }
        Some(node)
    }
}

impl CompiledStmt {
    /// A verdict uses the exact live source tree and immutable span inputs.
    /// Expired bodies and altered spans require independent verification.
    pub(crate) fn verify_holding_sites(
        &self,
        stream: &Stream,
        source: &room::Source,
        range: OpRange,
    ) -> bool {
        let Ok(mut cache) = self.source_roles.lock() else {
            return false;
        };
        if cache
            .verified
            .iter_mut()
            .any(|entry| entry.matches(stream, source, range, self.regs))
        {
            return true;
        }
        if !stream.verify_holding_sites(source, range, self.regs) {
            return false;
        }
        // A statement has only its canonical span and, for a while, its test.
        // Fresh executions replace an old verdict instead of retaining bodies.
        cache.verified.retain(|entry| entry.range != range);
        cache.verified.push(VerifiedHolding {
            body: Arc::downgrade(&source.body),
            site: source.site.clone(),
            range,
            regs: self.regs,
            ops: stream.ops[range.start as usize..range.end() as usize].into(),
            sites: stream.source_sites[range.start as usize..range.end() as usize].into(),
            joins: stream
                .source_joins
                .range(range.start..range.end())
                .map(|(pc, joins)| (*pc, joins.clone()))
                .collect(),
        });
        true
    }

    /// Cache only requested resume points. A representative forward path
    /// suffices because source joins give both arms identical exit owners.
    pub(crate) fn source_roles_at(&self, stream: &Stream, pc: u32) -> Option<SourceRoleList> {
        let mut cache = self.source_roles.lock().ok()?;
        if let Some(roles) = cache.roles.get(&pc) {
            return Some(Arc::clone(roles));
        }
        let range = match self.form {
            Form::While { test, .. } if pc >= test.start && pc < test.end() => test,
            _ => self.ops,
        };
        let roles = stream.form_source_roles(range, self.regs, pc)?;
        cache.roles.insert(pc, Arc::clone(&roles));
        Some(roles)
    }
}

impl Stream {
    fn form_source_roles(&self, range: OpRange, regs: u32, wanted: u32) -> Option<SourceRoleList> {
        if wanted < range.start || wanted >= range.end() {
            return None;
        }
        let mut owners = vec![None::<Arc<Site>>; regs as usize];
        let mut pc = range.start;
        while pc < wanted {
            let op = *self.ops.get(pc as usize)?;
            for reg in op.reads() {
                let owner = owners.get_mut(reg.ix())?;
                owner.as_ref()?;
                if !matches!(op, Op::Pin { .. }) {
                    *owner = None;
                }
            }
            if let Some(reg) = op.writes() {
                *owners.get_mut(reg.ix())? =
                    Some(Arc::clone(self.source_sites.get(pc as usize)?.as_ref()?));
            }
            if let Some(joins) = self.source_joins.get(&pc) {
                for join in joins {
                    let owner = owners.get_mut(join.value.ix())?;
                    if owner.as_ref()? != &join.from {
                        return None;
                    }
                    *owner = Some(Arc::clone(&join.to));
                }
            }
            let next = pc.checked_add(1)?;
            pc = match op {
                Op::Jump { skip } => next.checked_add(skip)?,
                Op::JumpIfFalse { skip, .. } if wanted >= next.checked_add(skip)? => {
                    next.checked_add(skip)?
                }
                _ => next,
            };
        }
        if pc != wanted {
            return None;
        }
        Some(
            owners
                .into_iter()
                .enumerate()
                .filter_map(|(reg, site)| Some((lash_kernel_exec::Reg(reg as u32), site?)))
                .collect::<Vec<_>>()
                .into(),
        )
    }

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
        let mut nodes = SourceNodes {
            source,
            children: BTreeMap::new(),
        };
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
            let Some(node) = nodes.at(site) else {
                return false;
            };
            if !implements(op, node) {
                return false;
            }
        }
        self.verify_source_flow(source, range, regs, &mut nodes)
    }

    fn verify_source_flow(
        &self,
        source: &room::Source,
        range: OpRange,
        regs: u32,
        nodes: &mut SourceNodes<'_>,
    ) -> bool {
        // Only jump targets can have multiple incoming paths. Linear steps
        // transfer their owners without copying the entire register file.
        let targets: BTreeSet<_> = (range.start..range.end())
            .filter_map(|pc| match self.ops[pc as usize] {
                Op::Jump { skip } | Op::JumpIfFalse { skip, .. } => {
                    pc.checked_add(1)?.checked_add(skip)
                }
                _ => None,
            })
            .collect();
        let mut pending = vec![(range.start, vec![None::<Arc<Site>>; regs as usize])];
        let mut seen = BTreeMap::new();
        while let Some((pc, mut owners)) = pending.pop() {
            if pc == range.end() {
                continue;
            }
            if pc > range.end() {
                return false;
            }
            if targets.contains(&pc)
                && let Some(previous) = seen.insert(pc, owners.clone())
            {
                if previous != owners {
                    return false;
                }
                continue;
            }
            let op = self.ops[pc as usize];
            let expected = self.source_sites[pc as usize]
                .as_ref()
                .and_then(|site| inputs(op, site, nodes.at(site)?));
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
                    let Some(node) = nodes.at(&join.to) else {
                        return false;
                    };
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
                Op::Jump { skip } => {
                    let Some(target) = next.checked_add(skip) else {
                        return false;
                    };
                    pending.push((target, owners));
                }
                Op::JumpIfFalse { skip, .. } => {
                    let Some(target) = next.checked_add(skip) else {
                        return false;
                    };
                    pending.push((target, owners.clone()));
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
