//! The scope stack's data: what a binding is, and what the lowerer knows
//! about it.
//!
//! Every per-binding fact belongs here rather than in a side table on the
//! lowerer. A table keyed by name outlives the scope that declared the
//! binding, and internal names are unique only where a binding is visibly
//! shadowed — so sibling scopes shared keys and a dead binding's facts
//! changed how a live one lowered.

use std::collections::{BTreeMap, BTreeSet};

use super::{Expr, MemberProperty, Pattern, PropertyKey, Stmt, TsAssignTarget};

/// Names assigned anywhere in one function owner's statement tree.
///
/// Nested lexical scopes deliberately share this name set: treating an
/// assignment to a shadowing binding as if it could target the outer binding
/// is conservative and prevents a false process-handle fact. Nested function
/// bodies have their own owner and are scanned when that function is lowered.
pub(super) fn assigned_identifiers_in_statements(statements: &[Stmt]) -> BTreeSet<String> {
    let mut assigned = BTreeSet::new();
    for statement in statements {
        collect_statement_assignments(statement, &mut assigned);
    }
    assigned
}

fn collect_statement_assignments(statement: &Stmt, assigned: &mut BTreeSet<String>) {
    match statement {
        Stmt::Empty | Stmt::Break | Stmt::Continue | Stmt::Function { .. } => {}
        Stmt::Expr(expression) | Stmt::Throw(expression) => {
            collect_expression_assignments(expression, assigned);
        }
        Stmt::Return(expression) => {
            if let Some(expression) = expression {
                collect_expression_assignments(expression, assigned);
            }
        }
        Stmt::Block(statements) => {
            for statement in statements {
                collect_statement_assignments(statement, assigned);
            }
        }
        Stmt::Var { declarations, .. } => {
            for declaration in declarations {
                if let Some(initializer) = &declaration.init {
                    collect_expression_assignments(initializer, assigned);
                }
                collect_pattern_expressions(&declaration.pattern, assigned);
            }
        }
        Stmt::Enum { members, .. } => {
            for member in members {
                collect_expression_assignments(&member.value, assigned);
            }
        }
        Stmt::If {
            test,
            consequent,
            alternate,
        } => {
            collect_expression_assignments(test, assigned);
            collect_statement_assignments(consequent, assigned);
            if let Some(alternate) = alternate {
                collect_statement_assignments(alternate, assigned);
            }
        }
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            collect_expression_assignments(test, assigned);
            collect_statement_assignments(body, assigned);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            if let Some(init) = init {
                collect_statement_assignments(init, assigned);
            }
            if let Some(test) = test {
                collect_expression_assignments(test, assigned);
            }
            if let Some(update) = update {
                collect_expression_assignments(update, assigned);
            }
            collect_statement_assignments(body, assigned);
        }
        Stmt::ForOf {
            pattern,
            kind,
            iterable,
            body,
        }
        | Stmt::ForIn {
            pattern,
            kind,
            object: iterable,
            body,
        } => {
            collect_expression_assignments(iterable, assigned);
            if kind.is_none() {
                collect_pattern_targets(pattern, assigned);
            } else {
                collect_pattern_expressions(pattern, assigned);
            }
            collect_statement_assignments(body, assigned);
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            collect_expression_assignments(discriminant, assigned);
            for case in cases {
                if let Some(test) = &case.test {
                    collect_expression_assignments(test, assigned);
                }
                for statement in &case.consequent {
                    collect_statement_assignments(statement, assigned);
                }
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for statement in body {
                collect_statement_assignments(statement, assigned);
            }
            if let Some(catch) = catch {
                if let Some(binding) = &catch.binding {
                    collect_pattern_expressions(binding, assigned);
                }
                for statement in &catch.body {
                    collect_statement_assignments(statement, assigned);
                }
            }
            if let Some(finally) = finally {
                for statement in finally {
                    collect_statement_assignments(statement, assigned);
                }
            }
        }
    }
}

fn collect_expression_assignments(expression: &Expr, assigned: &mut BTreeSet<String>) {
    match expression {
        Expr::Function(_) => {}
        Expr::Assign { target, value, .. } => {
            collect_assignment_target(target, assigned);
            collect_expression_assignments(value, assigned);
        }
        Expr::Update { target, .. } => collect_assignment_target(target, assigned),
        expression => {
            for child in expression.children() {
                collect_expression_assignments(child, assigned);
            }
        }
    }
}

fn collect_assignment_target(target: &TsAssignTarget, assigned: &mut BTreeSet<String>) {
    match target {
        TsAssignTarget::Ident(name) => {
            assigned.insert(name.clone());
        }
        TsAssignTarget::Member { object, property } => {
            collect_expression_assignments(object, assigned);
            if let MemberProperty::Index(index) = property {
                collect_expression_assignments(index, assigned);
            }
        }
        TsAssignTarget::Pattern(pattern) => collect_pattern_targets(pattern, assigned),
    }
}

fn collect_pattern_targets(pattern: &Pattern, assigned: &mut BTreeSet<String>) {
    match pattern {
        Pattern::Ident(name) => {
            assigned.insert(name.clone());
        }
        Pattern::Rest(target) => collect_pattern_targets(target, assigned),
        Pattern::Member { object, property } => {
            collect_expression_assignments(object, assigned);
            if let MemberProperty::Index(index) = property {
                collect_expression_assignments(index, assigned);
            }
        }
        Pattern::Assign { target, default } => {
            collect_pattern_targets(target, assigned);
            collect_expression_assignments(default, assigned);
        }
        Pattern::Array { elements, rest } => {
            for element in elements.iter().flatten() {
                collect_pattern_targets(element, assigned);
            }
            if let Some(rest) = rest {
                collect_pattern_targets(rest, assigned);
            }
        }
        Pattern::Object { properties, rest } => {
            for property in properties {
                if let PropertyKey::Computed(key) = &property.key {
                    collect_expression_assignments(key, assigned);
                }
                collect_pattern_targets(&property.value, assigned);
            }
            if let Some(rest) = rest {
                collect_pattern_targets(rest, assigned);
            }
        }
    }
}

fn collect_pattern_expressions(pattern: &Pattern, assigned: &mut BTreeSet<String>) {
    for expression in pattern.child_expressions() {
        collect_expression_assignments(expression, assigned);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BindingKind {
    Const,
    Let,
    Var,
    Function,
    Parameter,
    Catch,
}

/// The exotic iterables whose `forEach` and `entries`/`keys`/`values` are the
/// collection's own, not the array methods of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IterableKind {
    Map,
    Set,
    UrlSearchParams,
}

impl IterableKind {
    pub(super) fn from_constructor(constructor: &str) -> Option<Self> {
        match constructor {
            "Map" => Some(Self::Map),
            "Set" => Some(Self::Set),
            "URLSearchParams" => Some(Self::UrlSearchParams),
            _ => None,
        }
    }
}

/// What a binding *is*, beyond the name it holds.
///
/// Every one of these facts is per-binding, so it lives on the binding and
/// dies with the scope that declared it. Held in a side table keyed by name
/// they outlived their binding and leaked across sibling scopes, which is
/// exactly the shadowing case the internal names are only unique under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BindingRole {
    /// An ordinary value. Everything the lowerer has no extra fact about.
    Plain,
    /// An async function bound to a name, which must be awaited at its call.
    AsyncHelper,
    /// A handle returned by `start`, which `await` resolves to the process
    /// result.
    ProcessHandle,
    /// A `defineProcess` binding, carrying the declared process name that
    /// `start` targets.
    ProcessDefinition(String),
    /// A collection whose iteration protocol is its own, not an array's.
    ExoticIterable(IterableKind),
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    pub(super) internal: String,
    pub(super) kind: BindingKind,
    pub(super) initialized: bool,
    pub(super) owner_function: usize,
    pub(super) role: BindingRole,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Scope {
    pub(super) bindings: BTreeMap<String, Binding>,
}
