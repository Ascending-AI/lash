//! The names a body shares with the scope around it, and the closures it
//! makes: the compiler's variable analysis (`K-FORM-003`, `K-CLO-001`).
//!
//! The walk keeps the body's lexical scopes as `K-FORM-003` states them: a
//! `let` declares from the statement after it to its block's end, a `for`
//! binding and a `catch` binding in their bodies, a scoped expression
//! binding in its body. A name read or written where no scope of the body
//! declares it is free: the closure shares it with its maker, or in `main`
//! it is a session binding.

use std::collections::BTreeSet;

use lash_kernel_doc::{ActKind, Action, Atom, Callee, Expr, Member, Place, Rhs, Stmt};
use lash_kernel_value::names::{FunctionId, Name};

use super::compile::Result;

/// What a walk found.
pub(crate) struct Names<'f> {
    scopes: Vec<BTreeSet<Name>>,
    /// Every name a scope of the body declares, parameters excluded.
    pub declared: BTreeSet<Name>,
    /// The names used where no scope of the body declares them.
    pub free: BTreeSet<Name>,
    /// The closures the body makes, in order.
    pub closures: Vec<FunctionId>,
    /// The free names of a closure the body makes.
    closure_free: &'f mut dyn FnMut(FunctionId) -> Result<BTreeSet<Name>>,
}

impl<'f> Names<'f> {
    /// A walk of a body whose parameters are `params`.
    pub(crate) fn new(
        params: impl IntoIterator<Item = Name>,
        closure_free: &'f mut dyn FnMut(FunctionId) -> Result<BTreeSet<Name>>,
    ) -> Self {
        Self {
            scopes: vec![params.into_iter().collect()],
            declared: BTreeSet::new(),
            free: BTreeSet::new(),
            closures: Vec::new(),
            closure_free,
        }
    }

    fn declare(&mut self, name: &Name) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone());
        }
        self.declared.insert(name.clone());
    }

    fn use_(&mut self, name: &Name) {
        if !self.scopes.iter().any(|scope| scope.contains(name)) {
            self.free.insert(name.clone());
        }
    }

    fn scoped(&mut self, walk: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        self.scopes.push(BTreeSet::new());
        let walked = walk(self);
        self.scopes.pop();
        walked
    }

    pub(crate) fn block(&mut self, block: &[Stmt]) -> Result<()> {
        self.scoped(|names| {
            for stmt in block {
                names.statement(stmt)?;
            }
            Ok(())
        })
    }

    fn statement(&mut self, stmt: &Stmt) -> Result<()> {
        match stmt {
            Stmt::Let { name, value } => {
                self.rhs(value)?;
                self.declare(name);
            }
            Stmt::Assign { place, value, .. } => {
                match place {
                    Place::Variable(name) => self.use_(name),
                    Place::Member(target) => self.member(target)?,
                }
                self.rhs(value)?;
            }
            Stmt::Remove { member } => self.member(member)?,
            Stmt::Do { value }
            | Stmt::Return { value }
            | Stmt::ReturnVia { value, .. }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value)?,
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition)?;
                self.block(then_block)?;
                self.block(else_block)?;
            }
            Stmt::For {
                binding,
                iterable,
                body,
                else_block,
                ..
            } => {
                self.expr(iterable)?;
                self.scoped(|names| {
                    names.declare(binding);
                    names.block(body)
                })?;
                self.block(else_block)?;
            }
            Stmt::While {
                condition,
                body,
                else_block,
                ..
            } => {
                self.expr(condition)?;
                self.block(body)?;
                self.block(else_block)?;
            }
            Stmt::Try(scope) => {
                self.block(&scope.body)?;
                if let Some(catch) = &scope.catch {
                    self.scoped(|names| {
                        names.declare(&catch.binding);
                        names.block(&catch.body)
                    })?;
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally)?;
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
        Ok(())
    }

    fn rhs(&mut self, value: &Rhs) -> Result<()> {
        match value {
            Rhs::Expr(value) => self.expr(value),
            Rhs::Action(action) => {
                match action {
                    Action::Call { callee, args } | Action::Spawn { callee, args } => {
                        self.callee(callee);
                        args.iter().for_each(|arg| self.atom(arg));
                    }
                    Action::Perform { args, .. } => args.iter().for_each(|arg| self.atom(arg)),
                    Action::Join { task }
                    | Action::Cancel { task }
                    | Action::CancelEffect { task } => self.atom(task),
                    Action::JoinMany { tasks, .. } => self.atom(tasks),
                    Action::Await { value, .. } => self.atom(value),
                    Action::Yield => {}
                }
                Ok(())
            }
        }
    }

    fn callee(&mut self, callee: &Callee) {
        if let Callee::Value(name) = callee {
            self.use_(name);
        }
    }

    fn atom(&mut self, value: &Atom) {
        if let Atom::Variable(name) = value {
            self.use_(name);
        }
    }

    fn member(&mut self, target: &Member) -> Result<()> {
        match target {
            Member::Field { target, .. } => self.expr(target),
            Member::Index { target, index } => {
                self.expr(target)?;
                self.expr(index)
            }
        }
    }

    fn expr(&mut self, value: &Expr) -> Result<()> {
        match value {
            Expr::Literal(_) => {}
            Expr::Variable(name) => self.use_(name),
            Expr::If { cond, then, r#else } => {
                self.expr(cond)?;
                self.expr(then)?;
                self.expr(r#else)?;
            }
            Expr::Let { name, value, body } => {
                self.expr(value)?;
                self.scoped(|names| {
                    names.declare(name);
                    names.expr(body)
                })?;
            }
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                for item in items {
                    self.expr(item)?;
                }
            }
            Expr::Map(entries) => {
                for entry in entries {
                    self.expr(&entry.key)?;
                    self.expr(&entry.value)?;
                }
            }
            Expr::Record(entries) => {
                for entry in entries {
                    self.expr(&entry.value)?;
                }
            }
            Expr::Member(target) => self.member(target)?,
            Expr::Closure(closure) => {
                self.closures.push(closure.function);
                for name in (self.closure_free)(closure.function)? {
                    self.use_(&name);
                }
            }
            Expr::Call { args, .. } => {
                for arg in args {
                    self.expr(arg)?;
                }
            }
            Expr::Act(act) => {
                if let ActKind::Call(callee) | ActKind::Spawn(callee) = &act.kind {
                    self.callee(callee);
                }
                for arg in &act.args {
                    self.expr(arg)?;
                }
            }
        }
        Ok(())
    }
}
