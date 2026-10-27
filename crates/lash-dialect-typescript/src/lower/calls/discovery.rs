//! Tool candidates from the parsed tree, without evaluating or lowering it.
//!
//! Discovery visits every call, including code lowering will refuse or never
//! emit. Only a dotted callee with a free, non-builtin root is a candidate;
//! declaration scopes are the same ones lowering uses.

use std::collections::{BTreeSet, HashSet};

use lash_kernel_value::Name;

use crate::adapter as ast;
use crate::lower::walk;

pub(crate) fn tool_calls(program: &ast::Program, session: &BTreeSet<Name>) -> BTreeSet<String> {
    discover(program, session).calls
}

fn discover<'a>(program: &ast::Program, session: &'a BTreeSet<Name>) -> Discovery<'a> {
    let mut discovery = Discovery {
        session,
        scopes: Vec::new(),
        calls: BTreeSet::new(),
    };
    let mut names = Vec::new();
    walk::var_names(&program.statements, &mut names);
    discovery.scopes.push(names.into_iter().collect());
    discovery.block(&program.statements);
    discovery
}

struct Discovery<'a> {
    session: &'a BTreeSet<Name>,
    scopes: Vec<HashSet<String>>,
    calls: BTreeSet<String>,
}

impl Discovery<'_> {
    fn is_bound(&self, name: &str) -> bool {
        self.session.contains(&Name::new(name))
            || self.scopes.iter().any(|scope| scope.contains(name))
    }

    fn lexical_names(statements: &[ast::Stmt]) -> HashSet<String> {
        let mut names = Vec::new();
        for statement in statements {
            match statement.unspanned() {
                ast::Stmt::Var { kind, declarations } if *kind != ast::VarKind::Var => {
                    for declaration in declarations {
                        walk::pattern_names(&declaration.pattern, &mut names);
                    }
                }
                ast::Stmt::Function { name, .. } | ast::Stmt::Enum { name, .. } => {
                    names.push(name.clone());
                }
                _ => {}
            }
        }
        names.into_iter().collect()
    }

    fn block(&mut self, statements: &[ast::Stmt]) {
        self.scopes.push(Self::lexical_names(statements));
        self.statements(statements);
        self.scopes.pop();
    }

    fn statements(&mut self, statements: &[ast::Stmt]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &ast::Stmt) {
        match statement.unspanned() {
            ast::Stmt::Spanned(..) => unreachable!("unspanned statements have no wrapper"),
            ast::Stmt::Empty
            | ast::Stmt::TypeAlias { .. }
            | ast::Stmt::NamedBreak(_)
            | ast::Stmt::NamedContinue(_)
            | ast::Stmt::Break
            | ast::Stmt::Continue => {}
            ast::Stmt::Expr(value) | ast::Stmt::Throw(value) => self.expr(value),
            ast::Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            ast::Stmt::Labeled { body, .. } | ast::Stmt::ExitHandler { body, .. } => {
                self.statement(body)
            }
            ast::Stmt::Block(statements) => self.block(statements),
            ast::Stmt::Var { declarations, .. } => {
                for declaration in declarations {
                    self.pattern(&declaration.pattern);
                    if let Some(init) = &declaration.init {
                        self.expr(init);
                    }
                }
            }
            ast::Stmt::Enum { members, .. } => {
                for member in members {
                    self.expr(&member.value);
                }
            }
            ast::Stmt::Function { function, .. } => self.function(function),
            ast::Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test);
                self.statement(consequent);
                if let Some(alternate) = alternate {
                    self.statement(alternate);
                }
            }
            ast::Stmt::While { test, body } | ast::Stmt::DoWhile { test, body, .. } => {
                self.expr(test);
                self.statement(body);
            }
            ast::Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let scope = init.as_deref().map_or_else(HashSet::new, |init| {
                    Self::lexical_names(std::slice::from_ref(init))
                });
                self.scopes.push(scope);
                if let Some(init) = init {
                    self.statement(init);
                }
                if let Some(test) = test {
                    self.expr(test);
                }
                if let Some(update) = update {
                    self.expr(update);
                }
                self.statement(body);
                self.scopes.pop();
            }
            ast::Stmt::ForOf {
                pattern,
                kind,
                iterable: subject,
                body,
            }
            | ast::Stmt::ForIn {
                pattern,
                kind,
                object: subject,
                body,
            } => {
                let mut names = Vec::new();
                if kind.is_some() {
                    walk::pattern_names(pattern, &mut names);
                }
                self.scopes.push(names.into_iter().collect());
                self.expr(subject);
                self.pattern(pattern);
                self.statement(body);
                self.scopes.pop();
            }
            ast::Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expr(discriminant);
                let scope = cases
                    .iter()
                    .flat_map(|case| Self::lexical_names(&case.consequent))
                    .collect();
                self.scopes.push(scope);
                for case in cases {
                    if let Some(test) = &case.test {
                        self.expr(test);
                    }
                    self.statements(&case.consequent);
                }
                self.scopes.pop();
            }
            ast::Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.block(body);
                if let Some(catch) = catch {
                    let mut names = Vec::new();
                    if let Some(binding) = &catch.binding {
                        walk::pattern_names(binding, &mut names);
                    }
                    self.scopes.push(names.into_iter().collect());
                    if let Some(binding) = &catch.binding {
                        self.pattern(binding);
                    }
                    self.block(&catch.body);
                    self.scopes.pop();
                }
                if let Some(finally) = finally {
                    self.block(finally);
                }
            }
        }
    }

    fn function(&mut self, function: &ast::Function) {
        let mut names = Vec::new();
        // Named function expressions bind their own name; an arrow's inferred
        // display name is not a lexical binding.
        if !function.is_arrow {
            names.extend(function.name.iter().cloned());
            names.push("arguments".to_owned());
        }
        for param in &function.params {
            walk::pattern_names(param, &mut names);
        }
        self.scopes.push(names.into_iter().collect());
        for param in &function.params {
            self.pattern(param);
        }
        match &function.body {
            ast::FunctionBody::Block(statements) => {
                let mut names = Vec::new();
                walk::var_names(statements, &mut names);
                self.scopes.push(names.into_iter().collect());
                self.block(statements);
                self.scopes.pop();
            }
            ast::FunctionBody::Expression(value) => self.expr(value),
        }
        self.scopes.pop();
    }

    fn pattern(&mut self, pattern: &ast::Pattern) {
        match pattern {
            ast::Pattern::Ident(..) => {}
            ast::Pattern::Rest(inner) => self.pattern(inner),
            ast::Pattern::Member { object, property } => {
                self.expr(object);
                self.property(property);
            }
            ast::Pattern::Assign { target, default } => {
                self.pattern(target);
                self.expr(default);
            }
            ast::Pattern::Array { elements, rest, .. } => {
                for element in elements.iter().flatten() {
                    self.pattern(element);
                }
                if let Some(rest) = rest {
                    self.pattern(rest);
                }
            }
            ast::Pattern::Object {
                properties, rest, ..
            } => {
                for property in properties {
                    self.key(&property.key);
                    self.pattern(&property.value);
                }
                if let Some(rest) = rest {
                    self.pattern(rest);
                }
            }
        }
    }

    fn property(&mut self, property: &ast::MemberProperty) {
        if let ast::MemberProperty::Index(index) = property {
            self.expr(index);
        }
    }

    fn key(&mut self, key: &ast::PropertyKey) {
        if let ast::PropertyKey::Computed(key) = key {
            self.expr(key);
        }
    }

    fn target(&mut self, target: &ast::AssignTarget) {
        match target {
            ast::AssignTarget::Ident(_) | ast::AssignTarget::ParenIdent(_) => {}
            ast::AssignTarget::Member { object, property } => {
                self.expr(object);
                self.property(property);
            }
            ast::AssignTarget::Pattern(pattern) => self.pattern(pattern),
        }
    }

    fn args(&mut self, args: &[ast::CallArg]) {
        for arg in args {
            match arg {
                ast::CallArg::Value(value) | ast::CallArg::Spread(value) => self.expr(value),
            }
        }
    }

    fn expr(&mut self, expr: &ast::Expr) {
        match expr {
            ast::Expr::Null
            | ast::Expr::Bool(_)
            | ast::Expr::BigInt(_)
            | ast::Expr::Number(_)
            | ast::Expr::String(_)
            | ast::Expr::RegExp { .. }
            | ast::Expr::This
            | ast::Expr::TemplateObject { .. }
            | ast::Expr::LoneSurrogateString => {}
            ast::Expr::Ident(..) => {}
            ast::Expr::Array(elements) => {
                for element in elements {
                    match element {
                        ast::ArrayElement::Value(value) | ast::ArrayElement::Spread(value) => {
                            self.expr(value)
                        }
                        ast::ArrayElement::Hole => {}
                    }
                }
            }
            ast::Expr::Object(properties) => {
                for property in properties {
                    match property {
                        ast::ObjectProperty::KeyValue(key, value) => {
                            self.key(key);
                            self.expr(value);
                        }
                        ast::ObjectProperty::Spread(value) => self.expr(value),
                    }
                }
            }
            ast::Expr::Assign { target, value, .. } => {
                self.target(target);
                self.expr(value);
            }
            ast::Expr::Member {
                object, property, ..
            }
            | ast::Expr::Delete { object, property } => {
                self.expr(object);
                self.property(property);
            }
            ast::Expr::Unary { value, .. }
            | ast::Expr::Await { value, .. }
            | ast::Expr::As { value, .. } => self.expr(value),
            ast::Expr::Binary { left, right, .. } | ast::Expr::Logical { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            ast::Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test);
                self.expr(consequent);
                self.expr(alternate);
            }
            ast::Expr::Sequence(values) => {
                for value in values {
                    self.expr(value);
                }
            }
            ast::Expr::Template { expressions, .. } => {
                for (_, expr) in expressions {
                    self.expr(expr);
                }
            }
            ast::Expr::Function(function) => self.function(function),
            ast::Expr::Call { callee, args, .. } => {
                if let Some((root, path)) = super::static_path(callee)
                    && path.contains('.')
                    && !self.is_bound(root)
                    && !lash_ext_ts_helpers::builtins::is_global(root)
                    && lash_kernel_dialect::value_constructors::Constructor::named(&path).is_none()
                {
                    self.calls.insert(path);
                }
                self.expr(callee);
                self.args(args);
            }
            ast::Expr::New { constructor, args } => {
                self.expr(constructor);
                self.args(args)
            }
            ast::Expr::OptionalChain { base, operations }
            | ast::Expr::DeleteChain { base, operations } => {
                self.expr(base);
                for operation in operations {
                    match operation {
                        ast::OptionalOperation::Member { property, .. } => self.property(property),
                        ast::OptionalOperation::Call { args, .. } => self.args(args),
                    }
                }
            }
            ast::Expr::Update { target, .. } => self.target(target),
        }
    }
}
