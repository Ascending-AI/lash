//! What code reads of the world outside it: the names it does not bind,
//! the effects it performs, the library functions it holds and the
//! declared functions it names.

use std::collections::BTreeSet;

use crate::ActKind;
use crate::Action;
use crate::Atom;
use crate::Callee;
use crate::Expr;
use crate::Literal;
use crate::Member;
use crate::Place;
use crate::Rhs;
use crate::Stmt;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::Name;

/// What a body reads of the world outside it.
#[derive(Clone, Debug, Default)]
pub struct CodeReads {
    /// The variables it names and does not declare.
    pub free: BTreeSet<Name>,
    pub effects: BTreeSet<EffectName>,
    /// Every library function it holds ([`crate::Node::functions_within`]).
    pub library: BTreeSet<FunctionId>,
    /// The declared functions it names.
    pub declared: BTreeSet<Name>,
}

impl CodeReads {
    /// What the code of `params` and `body` reads.
    pub fn of(params: &[Name], body: &[Stmt]) -> Self {
        let mut reads = Self {
            library: crate::Node::Block(body).functions_within(),
            ..Self::default()
        };
        reads.block(body, &mut params.to_vec());
        reads
    }

    fn name(&mut self, name: &Name, bound: &[Name]) {
        if !bound.contains(name) {
            self.free.insert(name.clone());
        }
    }

    /// Walks a block in a scope of its own: what it declares ends with it.
    fn block(&mut self, block: &[Stmt], bound: &mut Vec<Name>) {
        let outer = bound.len();
        for stmt in block {
            self.stmt(stmt, bound);
        }
        bound.truncate(outer);
    }

    fn scoped(&mut self, binding: &Name, block: &[Stmt], bound: &mut Vec<Name>) {
        bound.push(binding.clone());
        self.block(block, bound);
        bound.pop();
    }

    fn stmt(&mut self, stmt: &Stmt, bound: &mut Vec<Name>) {
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.rhs(value, bound);
                bound.push(name.clone());
            }
            Stmt::Assign { place, value, .. } => {
                match place {
                    Place::Variable(name) => self.name(name, bound),
                    Place::Member(member) => self.member(member, bound),
                }
                self.rhs(value, bound);
            }
            Stmt::Remove { member } => self.member(member, bound),
            Stmt::Do { value } => self.expr(value, bound),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, bound);
                self.block(then_block, bound);
                self.block(else_block, bound);
            }
            Stmt::For {
                binding,
                iterable,
                body,
                else_block,
                ..
            } => {
                self.expr(iterable, bound);
                self.scoped(binding, body, bound);
                self.block(else_block, bound);
            }
            Stmt::While {
                condition,
                body,
                else_block,
                ..
            } => {
                self.expr(condition, bound);
                self.block(body, bound);
                self.block(else_block, bound);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::ReturnVia { value, .. }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value, bound),
            Stmt::Try(scope) => {
                self.block(&scope.body, bound);
                if let Some(catch) = &scope.catch {
                    self.scoped(&catch.binding, &catch.body, bound);
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally, bound);
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &Rhs, bound: &mut Vec<Name>) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, bound),
            Rhs::Action(action) => self.action(action, bound),
        }
    }

    fn atom(&mut self, atom: &Atom, bound: &[Name]) {
        match atom {
            Atom::Variable(name) => self.name(name, bound),
            Atom::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
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
            Action::Perform { effect, args, .. } => {
                self.effects.insert(effect.clone());
                args.iter().for_each(|arg| self.atom(arg, bound));
            }
            Action::Sleep { duration: atom, .. }
            | Action::Await { value: atom, .. }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.atom(atom, bound),
            Action::Yield => {}
        }
    }

    fn callee(&mut self, callee: &Callee, bound: &[Name]) {
        match callee {
            Callee::Declared(name) => {
                self.declared.insert(name.clone());
            }
            Callee::Value(name) => self.name(name, bound),
            Callee::Library(_) => {}
        }
    }

    fn member(&mut self, member: &Member, bound: &mut Vec<Name>) {
        match member {
            Member::Field { target, .. } => self.expr(target, bound),
            Member::Index { target, index } => {
                self.expr(target, bound);
                self.expr(index, bound);
            }
        }
    }

    fn expr(&mut self, expr: &Expr, bound: &mut Vec<Name>) {
        match expr {
            Expr::Literal(Literal::Function(name)) => {
                self.declared.insert(name.clone());
            }
            Expr::Literal(_) | Expr::Clock | Expr::Random => {}
            Expr::Variable(name) => self.name(name, bound),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items.iter().for_each(|item| self.expr(item, bound));
            }
            Expr::Map(entries) => entries.iter().for_each(|entry| {
                self.expr(&entry.key, bound);
                self.expr(&entry.value, bound);
            }),
            Expr::Record(entries) => entries
                .iter()
                .for_each(|entry| self.expr(&entry.value, bound)),
            Expr::Member(member) => self.member(member, bound),
            Expr::Closure(closure) => {
                let outer = bound.len();
                bound.extend(closure.params.iter().cloned());
                self.block(&closure.body, bound);
                bound.truncate(outer);
            }
            Expr::Call { args, .. } => args.iter().for_each(|arg| self.expr(arg, bound)),
            Expr::If { cond, then, r#else } => {
                self.expr(cond, bound);
                self.expr(then, bound);
                self.expr(r#else, bound);
            }
            Expr::Let { name, value, body } => {
                self.expr(value, bound);
                bound.push(name.clone());
                self.expr(body, bound);
                bound.pop();
            }
            Expr::Read(read) => {
                self.expr(&read.handle, bound);
                self.expr(&read.request, bound);
            }
            Expr::Act(act) => {
                match &act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => self.callee(callee, bound),
                    ActKind::Perform { effect, .. } => {
                        self.effects.insert(effect.clone());
                    }
                    ActKind::Await(_)
                    | ActKind::Sleep { .. }
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Yield
                    | ActKind::Cancel => {}
                }
                act.args.iter().for_each(|arg| self.expr(arg, bound));
            }
        }
    }
}
