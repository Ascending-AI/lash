//! Canonical names for an extracted document (`K-CLO-004`): every declared
//! function but the entry is named by the order the entry's code first
//! reaches it, and every variable a function binds by the order its code
//! binds it. Spellings of bindings, object identities and allocation order
//! never reach the result, so equal rooted code with equal captures is one
//! document.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ActKind, Action, Atom, Callee, Catch, Document, Expr, Function, Literal, Member, Name, Place,
    Rhs, Stmt,
};

use super::reads::CodeReads;

/// Renames `document`'s functions and variables canonically: `entry`
/// becomes `entry_as` and keeps its parameters, which its signature names.
pub(crate) fn canonical(document: &mut Document, entry: &Name, entry_as: &Name) {
    let order = reach_order(document, entry);
    let mut functions = BTreeMap::new();
    let mut names = BTreeMap::new();
    for (index, name) in order.iter().enumerate() {
        let renamed = if name == entry {
            entry_as.clone()
        } else {
            Name::new(format!("f{}", index - 1))
        };
        names.insert(name.clone(), renamed);
    }
    let old = std::mem::take(&mut document.functions);
    for (name, mut function) in old {
        let Some(renamed) = names.get(&name).cloned() else {
            // Code the entry does not reach is not part of the value.
            continue;
        };
        let keep_params = name == *entry;
        rename_function(&mut function, &names, keep_params);
        functions.insert(renamed, function);
    }
    document.functions = functions;
}

/// The declared functions in the order the entry's code first names them,
/// the entry first.
fn reach_order(document: &Document, entry: &Name) -> Vec<Name> {
    let mut order = vec![entry.clone()];
    let mut seen: BTreeSet<Name> = order.iter().cloned().collect();
    let mut at = 0;
    while at < order.len() {
        if let Some(function) = document.functions.get(&order[at]) {
            let mut named = Vec::new();
            for stmt in &function.body {
                declared_in_stmt(stmt, &mut named);
            }
            for name in named {
                if document.functions.contains_key(&name) && seen.insert(name.clone()) {
                    order.push(name);
                }
            }
        }
        at += 1;
    }
    order
}

fn declared_in_stmt(stmt: &Stmt, out: &mut Vec<Name>) {
    let mut walker = Renamer {
        functions: &BTreeMap::new(),
        scopes: Vec::new(),
        next: 0,
        taken: BTreeSet::new(),
        found: Some(out),
    };
    let mut stmt = stmt.clone();
    walker.stmt(&mut stmt);
}

fn rename_function(function: &mut Function, functions: &BTreeMap<Name, Name>, keep_params: bool) {
    let reads = CodeReads::of(&function.params, &function.body);
    let mut renamer = Renamer {
        functions,
        scopes: vec![BTreeMap::new()],
        next: 0,
        taken: reads.free,
        found: None,
    };
    for param in &mut function.params {
        if keep_params {
            renamer.keep(param);
        } else {
            *param = renamer.bind(param);
        }
    }
    renamer.block(&mut function.body);
}

struct Renamer<'a, 'o> {
    functions: &'a BTreeMap<Name, Name>,
    /// The variables in scope, innermost last, each to its new name.
    scopes: Vec<BTreeMap<Name, Name>>,
    next: usize,
    /// The free names of the function, which no new name may take.
    taken: BTreeSet<Name>,
    /// Collects the declared functions named, in order, instead of
    /// renaming.
    found: Option<&'o mut Vec<Name>>,
}

impl Renamer<'_, '_> {
    fn fresh(&mut self) -> Name {
        loop {
            let name = Name::new(format!("v{}", self.next));
            self.next += 1;
            if !self.taken.contains(&name) {
                return name;
            }
        }
    }

    fn bind(&mut self, name: &Name) -> Name {
        let renamed = self.fresh();
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), renamed.clone());
        }
        renamed
    }

    fn keep(&mut self, name: &Name) {
        self.taken.insert(name.clone());
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), name.clone());
        }
    }

    fn variable(&self, name: &mut Name) {
        if self.found.is_some() {
            return;
        }
        if let Some(renamed) = self.scopes.iter().rev().find_map(|scope| scope.get(name)) {
            *name = renamed.clone();
        }
    }

    fn function(&mut self, name: &mut Name) {
        if let Some(found) = self.found.as_deref_mut() {
            found.push(name.clone());
            return;
        }
        if let Some(renamed) = self.functions.get(name) {
            *name = renamed.clone();
        }
    }

    fn binding(&mut self, name: &mut Name) {
        if self.found.is_some() {
            return;
        }
        *name = self.bind(name);
    }

    fn scoped<T>(&mut self, inner: impl FnOnce(&mut Self) -> T) -> T {
        self.scopes.push(BTreeMap::new());
        let result = inner(self);
        self.scopes.pop();
        result
    }

    fn block(&mut self, block: &mut [Stmt]) {
        self.scoped(|this| {
            for stmt in block {
                this.stmt(stmt);
            }
        });
    }

    fn stmt(&mut self, stmt: &mut Stmt) {
        match stmt {
            Stmt::Let { name, value } => {
                self.rhs(value);
                self.binding(name);
            }
            Stmt::Assign { place, value, .. } => {
                match place {
                    Place::Variable(name) => self.variable(name),
                    Place::Member(member) => self.member(member),
                }
                self.rhs(value);
            }
            Stmt::Remove { member } => self.member(member),
            Stmt::Do { value } => self.expr(value),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition);
                self.block(then_block);
                self.block(else_block);
            }
            Stmt::For {
                binding,
                iterable,
                body,
                else_block,
                ..
            } => {
                self.expr(iterable);
                self.scoped(|this| {
                    this.binding(binding);
                    this.block(body);
                });
                self.block(else_block);
            }
            Stmt::While {
                condition,
                body,
                else_block,
            } => {
                self.expr(condition);
                self.block(body);
                self.block(else_block);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::ReturnVia { value, .. }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => self.expr(value),
            Stmt::Try(scope) => {
                self.block(&mut scope.body);
                if let Some(Catch { binding, body }) = &mut scope.catch {
                    self.scoped(|this| {
                        this.binding(binding);
                        this.block(body);
                    });
                }
                if let Some(finally) = &mut scope.finally {
                    self.block(finally);
                }
            }
        }
    }

    fn rhs(&mut self, rhs: &mut Rhs) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr),
            Rhs::Action(action) => self.action(action),
        }
    }

    fn atom(&mut self, atom: &mut Atom) {
        match atom {
            Atom::Variable(name) => self.variable(name),
            Atom::Literal(Literal::Function(name)) => self.function(name),
            Atom::Literal(_) => {}
        }
    }

    fn action(&mut self, action: &mut Action) {
        match action {
            Action::Call { callee, args } | Action::Spawn { callee, args } => {
                match callee {
                    Callee::Declared(name) => self.function(name),
                    Callee::Value(name) => self.variable(name),
                    Callee::Library(_) => {}
                }
                args.iter_mut().for_each(|arg| self.atom(arg));
            }
            Action::Perform { args, .. } => args.iter_mut().for_each(|arg| self.atom(arg)),
            Action::Await { value: atom, .. }
            | Action::Sleep { duration: atom, .. }
            | Action::Join { task: atom }
            | Action::JoinMany { tasks: atom, .. }
            | Action::Cancel { task: atom } => self.atom(atom),
            Action::Yield => {}
        }
    }

    fn member(&mut self, member: &mut Member) {
        match member {
            Member::Field { target, .. } => self.expr(target),
            Member::Index { target, index } => {
                self.expr(target);
                self.expr(index);
            }
        }
    }

    fn expr(&mut self, expr: &mut Expr) {
        match expr {
            Expr::Literal(Literal::Function(name)) => self.function(name),
            Expr::Literal(_) | Expr::Clock | Expr::Random => {}
            Expr::Variable(name) => self.variable(name),
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
                items.iter_mut().for_each(|item| self.expr(item));
            }
            Expr::Map(entries) => entries.iter_mut().for_each(|entry| {
                self.expr(&mut entry.key);
                self.expr(&mut entry.value);
            }),
            Expr::Record(entries) => entries
                .iter_mut()
                .for_each(|entry| self.expr(&mut entry.value)),
            Expr::Member(member) => self.member(member),
            Expr::Closure(closure) => self.scoped(|this| {
                for param in &mut closure.params {
                    this.binding(param);
                }
                this.block(&mut closure.body);
            }),
            Expr::Call { args, .. } => args.iter_mut().for_each(|arg| self.expr(arg)),
            Expr::If { cond, then, r#else } => {
                self.expr(cond);
                self.expr(then);
                self.expr(r#else);
            }
            Expr::Let { name, value, body } => {
                self.expr(value);
                self.scoped(|this| {
                    this.binding(name);
                    this.expr(body);
                });
            }
            Expr::Read(read) => {
                self.expr(&mut read.handle);
                self.expr(&mut read.request);
            }
            Expr::Act(act) => {
                match &mut act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => match callee {
                        Callee::Declared(name) => self.function(name),
                        Callee::Value(name) => self.variable(name),
                        Callee::Library(_) => {}
                    },
                    ActKind::Perform { .. }
                    | ActKind::Await(_)
                    | ActKind::Sleep { .. }
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Yield
                    | ActKind::Cancel => {}
                }
                act.args.iter_mut().for_each(|arg| self.expr(arg));
            }
        }
    }
}
