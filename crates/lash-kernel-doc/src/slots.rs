//! The sole inventory of local function and Import references in executable nodes.
use crate::{
    ActKind, Action, Callee, Document, Expr, FunctionId, FunctionTarget, Node, NodeMut, Protocol,
    StartConvention, Stmt,
};
use lash_vocabulary::names::ImportSlot;
use lash_vocabulary::sites::Site;
use std::collections::BTreeSet;

macro_rules! inventory {
    ($target:ident,$callee:ident,$protocol:ident,$convention:ident,$stmt:ident,$action:ident,$expr:ident $(,$m:tt)?) => {
        fn $target(target:&$($m)? FunctionTarget)->Option<&$($m)? FunctionId> {
            match target {FunctionTarget::Local(id)=>Some(id),FunctionTarget::Import(_)=>None}
        }
        fn $callee(callee:&$($m)? Callee)->Option<&$($m)? FunctionId> {
            match callee {Callee::Local(id)=>Some(id),Callee::Import(_)|Callee::Value(_)=>None}
        }
        fn $protocol(protocol:&$($m)? Protocol)->Vec<&$($m)? FunctionId> {
            let Protocol {open,next,item,check}=protocol;
            std::iter::once(open).chain(next).chain(item).chain(check).filter_map($target).collect()
        }
        fn $convention(convention:&$($m)? StartConvention)->Vec<&$($m)? FunctionId> {
            match convention {
                StartConvention::Direct=>vec![],
                StartConvention::TsDirect {await_fn}=>$target(await_fn).into_iter().collect(),
                StartConvention::TsArgsList {await_fn,same_fn}=>[await_fn,same_fn].into_iter().filter_map($target).collect(),
                StartConvention::PyDirect {cleanup}=>$target(cleanup).into_iter().collect(),
            }
        }
        fn $stmt(stmt:&$($m)? Stmt)->Vec<&$($m)? FunctionId> {
            match stmt {Stmt::ReturnVia {policy,..}=>$target(policy).into_iter().collect(),Stmt::For {via,..}=>via.into_iter().flat_map($protocol).collect(),_=>vec![]}
        }
        fn $action(action:&$($m)? Action)->Vec<&$($m)? FunctionId> {
            match action {
                Action::Call {callee,..}|Action::Spawn {callee,..}=>$callee(callee).into_iter().collect(),
                Action::Await {policy,..}=>$target(policy).into_iter().collect(),_=>vec![],
            }
        }
        fn $expr(expr:&$($m)? Expr)->Vec<&$($m)? FunctionId> {
            match expr {
                Expr::Call {function,..}=>vec![function],
                Expr::Literal(crate::Literal::Function(id))=>vec![id],
                Expr::Act(act)=>match &$($m)? act.kind {
                    ActKind::Call(callee)|ActKind::Spawn(callee)=>$callee(callee).into_iter().collect(),
                    ActKind::Await(policy)=>$target(policy).into_iter().collect(),_=>vec![],
                },
                Expr::Closure(closure)=>std::iter::once(&$($m)? closure.function).chain(
                    (&$($m)? closure.presentation).into_iter().flat_map(|start|$convention(&$($m)? start.convention))).collect(),
                _=>vec![],
            }
        }
    };
}
inventory!(target, callee, protocol, convention, stmt, action, expr);
inventory!(
    target_mut,
    callee_mut,
    protocol_mut,
    convention_mut,
    stmt_mut,
    action_mut,
    expr_mut,
    mut
);
impl Protocol {
    pub fn functions(&self) -> Vec<&FunctionId> {
        protocol(self)
    }
    pub fn targets(&self) -> impl Iterator<Item = &FunctionTarget> {
        std::iter::once(&self.open)
            .chain(&self.next)
            .chain(&self.item)
            .chain(&self.check)
    }
}
impl StartConvention {
    pub fn targets_mut(&mut self) -> Vec<&mut FunctionTarget> {
        match self {
            Self::Direct => vec![],
            Self::TsDirect { await_fn } => vec![await_fn],
            Self::TsArgsList { await_fn, same_fn } => vec![await_fn, same_fn],
            Self::PyDirect { cleanup } => vec![cleanup],
        }
    }
    pub fn functions(&self) -> Vec<&FunctionId> {
        convention(self)
    }
    pub fn targets(&self) -> Vec<&FunctionTarget> {
        match self {
            Self::Direct => vec![],
            Self::TsDirect { await_fn } => vec![await_fn],
            Self::TsArgsList { await_fn, same_fn } => vec![await_fn, same_fn],
            Self::PyDirect { cleanup } => vec![cleanup],
        }
    }
    pub fn semantic(&self) -> lash_vocabulary::names::Convention {
        use lash_vocabulary::names::Convention;
        match self {
            Self::Direct => Convention::Direct,
            Self::TsDirect { .. } => Convention::TsDirect,
            Self::TsArgsList { .. } => Convention::TsArgsList,
            Self::PyDirect { .. } => Convention::PyDirect,
        }
    }
}
impl FunctionTarget {
    pub fn callee(&self) -> Callee {
        match self {
            Self::Local(id) => Callee::Local(*id),
            Self::Import(slot) => Callee::Import(*slot),
        }
    }
}
impl<'a> Node<'a> {
    pub fn functions(self) -> Vec<&'a FunctionId> {
        match self {
            Self::Block(_) => vec![],
            Self::Stmt(v) => stmt(v),
            Self::Action(v) => action(v),
            Self::Expr(v) => expr(v),
        }
    }
    pub fn functions_within(self) -> BTreeSet<FunctionId> {
        let mut found = BTreeSet::new();
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            found.extend(node.functions());
            pending.extend(node.children());
        }
        found
    }
    /// Import slots named by this node, including protocol and convention helpers.
    pub fn imports(self) -> Vec<ImportSlot> {
        let mut found = Vec::new();
        let mut target = |v: &FunctionTarget| {
            if let FunctionTarget::Import(slot) = v {
                found.push(*slot);
            }
        };
        match self {
            Self::Stmt(Stmt::ReturnVia { policy, .. }) => target(policy),
            Self::Stmt(Stmt::For { via: Some(via), .. }) => {
                for v in via.targets() {
                    target(v)
                }
            }
            Self::Action(Action::Perform { slot, .. }) => found.push(*slot),
            Self::Action(
                Action::Call {
                    callee: Callee::Import(slot),
                    ..
                }
                | Action::Spawn {
                    callee: Callee::Import(slot),
                    ..
                },
            ) => found.push(*slot),
            Self::Action(Action::Await { policy, .. }) => target(policy),
            Self::Expr(Expr::Act(act)) => match &act.kind {
                ActKind::Perform { slot, .. } => found.push(*slot),
                ActKind::Call(Callee::Import(slot)) | ActKind::Spawn(Callee::Import(slot)) => {
                    found.push(*slot)
                }
                ActKind::Await(policy) => target(policy),
                _ => {}
            },
            Self::Expr(Expr::Closure(closure)) => {
                if let Some(start) = &closure.presentation {
                    for v in start.convention.targets() {
                        target(v)
                    }
                }
            }
            _ => {}
        }
        found
    }
    pub fn imports_within(self) -> BTreeSet<ImportSlot> {
        let mut found = BTreeSet::new();
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            found.extend(node.imports());
            pending.extend(node.children());
        }
        found
    }
}
impl NodeMut<'_> {
    pub fn functions_mut(&mut self) -> Vec<&mut FunctionId> {
        match self {
            Self::Block(_) => vec![],
            Self::Stmt(v) => stmt_mut(v),
            Self::Action(v) => action_mut(v),
            Self::Expr(v) => expr_mut(v),
        }
    }
    pub fn try_each_function_mut<E>(
        self,
        site: Site,
        visit: &mut impl FnMut(&Site, &mut FunctionId) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pending = vec![(site, self)];
        while let Some((site, mut node)) = pending.pop() {
            for id in node.functions_mut() {
                visit(&site, id)?;
            }
            let children: Vec<_> = (0u32..)
                .zip(node.children())
                .map(|(index, child)| (site.child(index), child))
                .collect();
            pending.extend(children.into_iter().rev());
        }
        Ok(())
    }
}
impl Document {
    pub fn try_each_function_mut<E>(
        &mut self,
        mut visit: impl FnMut(&Site, &mut FunctionId) -> Result<(), E>,
    ) -> Result<(), E> {
        if let Some(main) = &mut self.main {
            NodeMut::Block(main)
                .try_each_function_mut(Site::new(crate::Unit::Main, []), &mut visit)?;
        }
        for (id, definition) in &mut self.functions {
            if let Some(body) = definition.body_mut() {
                NodeMut::Block(&mut body.block)
                    .try_each_function_mut(Site::new(crate::Unit::Library(*id), []), &mut visit)?;
            }
        }
        Ok(())
    }
}
