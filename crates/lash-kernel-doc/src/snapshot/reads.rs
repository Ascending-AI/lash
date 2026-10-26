//! What code reads of the world outside it: the names it does not bind,
//! the effects it performs, the library functions it holds and the
//! declared functions it names.

use std::collections::BTreeSet;

use crate::{ActKind, Action, Atom, Callee, Expr, Literal, Member, Place, Rhs, Stmt};

use lash_kernel_value::names::FunctionId;
use lash_kernel_value::names::Name;

/// What a body reads of the world outside it.
#[derive(Clone, Debug, Default)]
pub struct CodeReads {
    /// The variables it names and does not declare.
    pub free: BTreeSet<Name>,
    pub imports: BTreeSet<lash_kernel_value::names::ImportSlot>,
    /// Every library function it holds ([`crate::Node::functions_within`]).
    pub library: BTreeSet<FunctionId>,
    /// The declared functions it names.
    pub declared: BTreeSet<FunctionId>,
}

impl CodeReads {
    /// What the code of `params` and `body` reads.
    pub fn of(params: &[Name], body: &[Stmt], catalog: &dyn crate::FunctionCatalog) -> Self {
        let mut reads = Self {
            library: crate::Node::Block(body).functions_within(),
            ..Self::default()
        };
        reads.block(body, &mut params.to_vec(), catalog, &mut BTreeSet::new());
        reads
    }

    fn name(&mut self, name: &Name, bound: &[Name]) {
        if !bound.contains(name) {
            self.free.insert(name.clone());
        }
    }

    /// Walks a block in a scope of its own: what it declares ends with it.
    fn block(
        &mut self,
        block: &[Stmt],
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        let outer = bound.len();
        for stmt in block {
            self.stmt(stmt, bound, catalog, active);
        }
        bound.truncate(outer);
    }

    fn scoped(
        &mut self,
        binding: &Name,
        block: &[Stmt],
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        bound.push(binding.clone());
        self.block(block, bound, catalog, active);
        bound.pop();
    }

    fn stmt(
        &mut self,
        stmt: &Stmt,
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.rhs(value, bound, catalog, active);
                bound.push(name.clone());
            }
            Stmt::Assign { place, value, .. } => {
                match place {
                    Place::Variable(name) => self.name(name, bound),
                    Place::Member(member) => self.member(member, bound, catalog, active),
                }
                self.rhs(value, bound, catalog, active);
            }
            Stmt::Remove { member } => self.member(member, bound, catalog, active),
            Stmt::Do { value } => self.expr(value, bound, catalog, active),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, bound, catalog, active);
                self.block(then_block, bound, catalog, active);
                self.block(else_block, bound, catalog, active);
            }
            Stmt::For {
                binding,
                iterable,
                body,
                else_block,
                ..
            } => {
                self.expr(iterable, bound, catalog, active);
                self.scoped(binding, body, bound, catalog, active);
                self.block(else_block, bound, catalog, active);
            }
            Stmt::While {
                condition,
                body,
                else_block,
                ..
            } => {
                self.expr(condition, bound, catalog, active);
                self.block(body, bound, catalog, active);
                self.block(else_block, bound, catalog, active);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::ReturnVia { value, .. }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value, bound, catalog, active),
            Stmt::Try(scope) => {
                self.block(&scope.body, bound, catalog, active);
                if let Some(catch) = &scope.catch {
                    self.scoped(&catch.binding, &catch.body, bound, catalog, active);
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally, bound, catalog, active);
                }
            }
        }
    }

    fn rhs(
        &mut self,
        rhs: &Rhs,
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, bound, catalog, active),
            Rhs::Action(action) => self.action(action, bound),
        }
    }

    fn atom(&mut self, atom: &Atom, bound: &[Name]) {
        match atom {
            Atom::Variable(name) => self.name(name, bound),
            Atom::Literal(Literal::Function(name)) => {
                self.declared.insert(*name);
            }
            Atom::Literal(_) => {}
        }
    }

    fn action(&mut self, action: &Action, bound: &[Name]) {
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                self.callee(callee, bound);
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Perform { slot, args, .. } => {
                self.imports.insert(*slot);
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Await { value: atom, .. }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.atom(atom, bound),
            Action::Yield => {}
        }
    }

    fn callee(&mut self, callee: &Callee, bound: &[Name]) {
        match callee {
            Callee::Local(function) => {
                self.declared.insert(*function);
            }
            Callee::Import(slot) => {
                self.imports.insert(*slot);
            }
            Callee::Value(name) => self.name(name, bound),
        }
    }

    fn member(
        &mut self,
        member: &Member,
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        match member {
            Member::Field { target, .. } => self.expr(target, bound, catalog, active),
            Member::Index { target, index } => {
                self.expr(target, bound, catalog, active);
                self.expr(index, bound, catalog, active);
            }
        }
    }

    fn expr(
        &mut self,
        expr: &Expr,
        bound: &mut Vec<Name>,
        catalog: &dyn crate::FunctionCatalog,
        active: &mut BTreeSet<FunctionId>,
    ) {
        match expr {
            Expr::Literal(Literal::Function(name)) => {
                self.declared.insert(*name);
            }
            Expr::Literal(_) => {}
            Expr::Variable(name) => self.name(name, bound),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items
                    .iter()
                    .for_each(|item| self.expr(item, bound, catalog, active));
            }
            Expr::Map(entries) => entries.iter().for_each(|entry| {
                self.expr(&entry.key, bound, catalog, active);
                self.expr(&entry.value, bound, catalog, active);
            }),
            Expr::Record(entries) => entries
                .iter()
                .for_each(|entry| self.expr(&entry.value, bound, catalog, active)),
            Expr::Member(member) => self.member(member, bound, catalog, active),
            Expr::Closure(closure) => {
                let outer = bound.len();
                if active.insert(closure.function) {
                    if let Some(definition) = catalog.definition(&closure.function) {
                        bound.extend(definition.signature.params.iter().map(|p| p.name.clone()));
                        if let Some(body) = definition.body() {
                            self.block(&body.block, bound, catalog, active);
                        }
                    }
                    active.remove(&closure.function);
                }
                bound.truncate(outer);
            }
            Expr::Call { args, .. } => args
                .iter()
                .for_each(|arg| self.expr(arg, bound, catalog, active)),
            Expr::If { cond, then, r#else } => {
                self.expr(cond, bound, catalog, active);
                self.expr(then, bound, catalog, active);
                self.expr(r#else, bound, catalog, active);
            }
            Expr::Let { name, value, body } => {
                self.expr(value, bound, catalog, active);
                bound.push(name.clone());
                self.expr(body, bound, catalog, active);
                bound.pop();
            }
            Expr::Act(act) => {
                match &act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => self.callee(callee, bound),
                    ActKind::Perform { slot, .. } => {
                        self.imports.insert(*slot);
                    }
                    ActKind::Await(_)
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Yield
                    | ActKind::Cancel => {}
                }
                act.args
                    .iter()
                    .for_each(|arg| self.expr(arg, bound, catalog, active));
            }
        }
    }
}
