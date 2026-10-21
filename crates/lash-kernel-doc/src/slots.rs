//! Where code holds a library function by identity (`K-DOC-002`): the one
//! inventory of library-reference slots.
//!
//! A library identity sits in a call, a spawn or a native call, in an
//! `await`'s policy in either spelling, in a returning policy, in a loop's
//! protocol hooks, in a closure's or a declared function's start adapter
//! and in an entry's convention. Each slot is listed here once, and the
//! reading and the writing inventories are spelled from that one listing.
//! A consumer decides what a slot means to it (a reference, a call, a name
//! to rebind); it never lists the slots itself.

use std::collections::BTreeSet;

use crate::ast::ActKind;
use crate::ast::Action;
use crate::ast::Callee;
use crate::ast::Expr;
use crate::ast::Node;
use crate::ast::NodeMut;
use crate::ast::Protocol;
use crate::ast::Stmt;
use crate::document::Document;
use crate::function_value::StartConvention;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

/// Spells each slot listing once for reading (no `mut`) and once for
/// writing (`mut`), so the two inventories cannot drift.
macro_rules! inventory {
    (
        $callee:ident,
        $protocol:ident,
        $convention:ident,
        $stmt:ident,
        $action:ident,
        $expr:ident,
        $header:ident
        $(, $m:tt)?
    ) => {
        fn $callee(callee: &$($m)? Callee) -> Option<&$($m)? FunctionId> {
            match callee {
                Callee::Library(function) => Some(function),
                Callee::Declared(_) | Callee::Value(_) => None,
            }
        }

        /// Its hooks, in the order kernel text writes them.
        fn $protocol(protocol: &$($m)? Protocol) -> Vec<&$($m)? FunctionId> {
            let Protocol {
                open,
                next,
                item,
                check,
            } = protocol;
            std::iter::once(open)
                .chain(next)
                .chain(item)
                .chain(check)
                .collect()
        }

        fn $convention(convention: &$($m)? StartConvention) -> Vec<&$($m)? FunctionId> {
            match convention {
                StartConvention::Direct => Vec::new(),
                StartConvention::TsDirect { await_fn } => vec![await_fn],
                StartConvention::TsArgsList { await_fn, same_fn } => vec![await_fn, same_fn],
                StartConvention::PyDirect { cleanup } => vec![cleanup],
            }
        }

        fn $stmt(stmt: &$($m)? Stmt) -> Vec<&$($m)? FunctionId> {
            match stmt {
                Stmt::ReturnVia { policy, .. } => vec![policy],
                Stmt::For { via, .. } => via.into_iter().flat_map($protocol).collect(),
                Stmt::Let { .. }
                | Stmt::Assign { .. }
                | Stmt::Remove { .. }
                | Stmt::Do { .. }
                | Stmt::If { .. }
                | Stmt::While { .. }
                | Stmt::Break
                | Stmt::Continue
                | Stmt::Return { .. }
                | Stmt::Try(_)
                | Stmt::Throw { .. }
                | Stmt::Print { .. }
                | Stmt::Finish { .. }
                | Stmt::Fail { .. } => Vec::new(),
            }
        }

        fn $action(action: &$($m)? Action) -> Vec<&$($m)? FunctionId> {
            match action {
                Action::Call { callee, .. } | Action::Spawn { callee, .. } => {
                    $callee(callee).into_iter().collect()
                }
                Action::Await { policy, .. } => vec![policy],
                Action::Perform { .. }
                | Action::Sleep { .. }
                | Action::Join { .. }
                | Action::JoinMany { .. }
                | Action::Yield
                | Action::Cancel { .. } => Vec::new(),
            }
        }

        fn $expr(expr: &$($m)? Expr) -> Vec<&$($m)? FunctionId> {
            match expr {
                Expr::Call { function, .. } => vec![function],
                Expr::Act(act) => match &$($m)? act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => {
                        $callee(callee).into_iter().collect()
                    }
                    ActKind::Await(policy) => vec![policy],
                    ActKind::Perform { .. }
                    | ActKind::Sleep { .. }
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Yield
                    | ActKind::Cancel => Vec::new(),
                },
                Expr::Closure(closure) => (&$($m)? closure.start)
                    .into_iter()
                    .flat_map(|start| $convention(&$($m)? start.convention))
                    .collect(),
                Expr::Literal(_)
                | Expr::Variable(_)
                | Expr::If { .. }
                | Expr::Let { .. }
                | Expr::Tuple(_)
                | Expr::List(_)
                | Expr::Map(_)
                | Expr::Set(_)
                | Expr::Record(_)
                | Expr::Member(_)
                | Expr::Clock
                | Expr::Random
                | Expr::Read(_) => Vec::new(),
            }
        }

        /// The header's slots, each with the declared function whose root
        /// holds it: every declared function's start adapter, then every
        /// entry's convention.
        fn $header(document: &$($m)? Document) -> Vec<(&Name, &$($m)? FunctionId)> {
            let Document {
                functions, entries, ..
            } = document;
            let starts = functions.into_iter().flat_map(|(name, function)| {
                (&$($m)? function.start)
                    .into_iter()
                    .flat_map(|start| $convention(&$($m)? start.convention))
                    .map(move |slot| (name, slot))
            });
            let conventions = entries.into_iter().flat_map(|(name, entry)| {
                $convention(&$($m)? entry.convention)
                    .into_iter()
                    .map(move |slot| (name, slot))
            });
            starts.chain(conventions).collect()
        }
    };
}

inventory!(
    callee_slot,
    protocol_slots,
    convention_slots,
    stmt_slots,
    action_slots,
    expr_slots,
    header_slots
);
inventory!(
    callee_slot_mut,
    protocol_slots_mut,
    convention_slots_mut,
    stmt_slots_mut,
    action_slots_mut,
    expr_slots_mut,
    header_slots_mut,
    mut
);

impl Protocol {
    /// Its functions, in the order kernel text writes them.
    pub fn functions(&self) -> Vec<&FunctionId> {
        protocol_slots(self)
    }
}

impl StartConvention {
    /// Library identities reached by this convention.
    pub fn functions(&self) -> Vec<&FunctionId> {
        convention_slots(self)
    }
}

impl<'a> Node<'a> {
    /// The library functions this node itself holds, not its children's: a
    /// closure holds its start adapter's.
    pub fn functions(self) -> Vec<&'a FunctionId> {
        match self {
            Node::Block(_) => Vec::new(),
            Node::Stmt(stmt) => stmt_slots(stmt),
            Node::Action(action) => action_slots(action),
            Node::Expr(expr) => expr_slots(expr),
        }
    }

    /// Every library function this node and the nodes below it hold.
    pub fn functions_within(self) -> BTreeSet<FunctionId> {
        let mut functions = BTreeSet::new();
        let mut pending = vec![self];
        while let Some(node) = pending.pop() {
            functions.extend(node.functions());
            pending.extend(node.children());
        }
        functions
    }
}

impl NodeMut<'_> {
    /// The library functions this node itself holds, for writing, as
    /// [`Node::functions`] lists them.
    pub fn functions_mut(&mut self) -> Vec<&mut FunctionId> {
        match self {
            NodeMut::Block(_) => Vec::new(),
            NodeMut::Stmt(stmt) => stmt_slots_mut(stmt),
            NodeMut::Action(action) => action_slots_mut(action),
            NodeMut::Expr(expr) => expr_slots_mut(expr),
        }
    }

    /// Calls `visit` on every library function this node, at `site`, and
    /// the nodes below it hold, with the site of the node that holds it, a
    /// node before its children and children in order; stops at the first
    /// error.
    ///
    /// # Errors
    ///
    /// The first error `visit` returns.
    pub fn try_each_function_mut<E>(
        self,
        site: Site,
        visit: &mut impl FnMut(&Site, &mut FunctionId) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut pending = vec![(site, self)];
        while let Some((site, mut node)) = pending.pop() {
            for function in node.functions_mut() {
                visit(&site, function)?;
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
    /// The library functions the document's header holds, each with the
    /// declared function at whose root it is held: every declared
    /// function's start adapter, then every entry's convention.
    pub fn header_functions(&self) -> Vec<(&Name, &FunctionId)> {
        header_slots(self)
    }

    /// Calls `visit` on every library function the document holds, with
    /// the site that holds it: the header's
    /// ([`Self::header_functions`]), each at its function's root, then
    /// `main`'s code, then each declared function's code. Stops at the
    /// first error.
    ///
    /// # Errors
    ///
    /// The first error `visit` returns.
    pub fn try_each_function_mut<E>(
        &mut self,
        mut visit: impl FnMut(&Site, &mut FunctionId) -> Result<(), E>,
    ) -> Result<(), E> {
        let root = |name: &Name| Site::new(Unit::Function(name.clone()), []);
        for (name, function) in header_slots_mut(self) {
            visit(&root(name), function)?;
        }
        NodeMut::Block(&mut self.main)
            .try_each_function_mut(Site::new(Unit::Main, []), &mut visit)?;
        for (name, function) in &mut self.functions {
            NodeMut::Block(&mut function.body).try_each_function_mut(root(name), &mut visit)?;
        }
        Ok(())
    }
}
