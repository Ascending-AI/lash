//! The early errors ECMA-262 attaches to declarations, checked once over the
//! whole normalized program.
//!
//! In strict code a name is lexically declared at most once per scope and is
//! never also var-declared there (the Static Semantics: Early Errors of
//! *Block*, *CaseBlock*, *Catch*, the `for` heads, *FunctionBody* and
//! *ScriptBody*); a function's parameters are distinct and are not lexically
//! redeclared in its body. Each is a `TS_SYNTAX_ERROR`.
//!
//! One redeclaration ECMA-262 allows is refused: a function declaration that
//! shares its var scope's name with another function, a `var`, or a
//! parameter. `tsc --strict` rejects each (TS2393, TS2300), so the dialect
//! refuses the shape by name, `TS_FUNCTION_REDECLARATION_UNSUPPORTED`, rather
//! than implement it (ADR 0064). A negative early error outranks it.

use std::collections::BTreeSet;

use super::early_errors::syntax_error;
use super::{
    ArrayElement, AssignTarget, CallArg, Catch, Expr, Function, FunctionBody, MemberProperty,
    ObjectProperty, OptionalOperation, Pattern, Program, PropertyKey, Stmt, SwitchCase, VarKind,
};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

pub(super) fn check(program: &Program) -> Result<(), Diagnostic> {
    let mut checker = Checker::default();
    checker.var_scope(&[], &program.statements, None)?;
    checker.refusal.map_or(Ok(()), Err)
}

/// A declared name and the statement that declares it.
struct Declared {
    name: String,
    span: Option<SourceSpan>,
}

#[derive(Default)]
struct Checker {
    /// The first refused redeclaration, reported only if no early error is.
    refusal: Option<Diagnostic>,
}

fn already_declared(declared: &Declared) -> Diagnostic {
    syntax_error(
        format!("identifier `{}` has already been declared", declared.name),
        declared.span,
    )
}

/// The first name `names` declares twice.
fn first_duplicate(names: &[Declared]) -> Option<&Declared> {
    let mut seen = BTreeSet::new();
    names
        .iter()
        .find(|declared| !seen.insert(declared.name.as_str()))
}

/// The first of `names` that `others` also declares.
fn first_shared<'a>(names: &'a [Declared], others: &[Declared]) -> Option<&'a Declared> {
    let others = others
        .iter()
        .map(|declared| declared.name.as_str())
        .collect::<BTreeSet<_>>();
    names
        .iter()
        .find(|declared| others.contains(declared.name.as_str()))
}

impl Checker {
    /// A function body or the cell itself: its top-level function
    /// declarations are var-scoped, its `let`/`const` lexical.
    fn var_scope(
        &mut self,
        params: &[Pattern],
        body: &[Stmt],
        span: Option<SourceSpan>,
    ) -> Result<(), Diagnostic> {
        let parameters = pattern_list_names(params, span);
        if let Some(duplicate) = first_duplicate(&parameters) {
            return Err(syntax_error(
                format!(
                    "duplicate parameter name `{}` is not allowed in strict mode",
                    duplicate.name
                ),
                span,
            ));
        }
        let lexical = lexically_declared(body, false);
        let functions = top_level_functions(body);
        let mut vars = Vec::new();
        var_declared(body, None, &mut vars);
        if let Some(duplicate) = first_duplicate(&lexical) {
            return Err(already_declared(duplicate));
        }
        if let Some(shared) = first_shared(&lexical, &vars)
            .or_else(|| first_shared(&lexical, &functions))
            .or_else(|| first_shared(&lexical, &parameters))
        {
            return Err(already_declared(shared));
        }
        if self.refusal.is_none()
            && let Some(redeclared) = first_duplicate(&functions)
                .or_else(|| first_shared(&functions, &vars))
                .or_else(|| first_shared(&functions, &parameters))
        {
            self.refusal = Some(Diagnostic::new(
                DiagnosticCode::FunctionRedeclarationUnsupported,
                format!(
                    "function `{}` redeclares a name its scope already declares, which `tsc --strict` refuses (TS2393, TS2300)",
                    redeclared.name
                ),
                redeclared.span,
            ));
        }
        params.iter().try_for_each(|param| self.pattern(param))?;
        self.statements(body)
    }

    /// A block, or a case block taken whole: every declaration in it is
    /// lexical, function declarations included.
    fn block_scope(&mut self, body: &[&Stmt]) -> Result<(), Diagnostic> {
        let lexical = lexically_declared(body.iter().copied(), true);
        let mut vars = Vec::new();
        var_declared(body.iter().copied(), None, &mut vars);
        if let Some(duplicate) = first_duplicate(&lexical) {
            return Err(already_declared(duplicate));
        }
        if let Some(shared) = first_shared(&lexical, &vars) {
            return Err(already_declared(shared));
        }
        body.iter().try_for_each(|stmt| self.statement(stmt, None))
    }

    fn block(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        self.block_scope(&body.iter().collect::<Vec<_>>())
    }

    fn statements(&mut self, body: &[Stmt]) -> Result<(), Diagnostic> {
        body.iter().try_for_each(|stmt| self.statement(stmt, None))
    }

    fn statement(&mut self, stmt: &Stmt, span: Option<SourceSpan>) -> Result<(), Diagnostic> {
        match stmt {
            Stmt::Spanned(span, stmt) => self.statement(stmt, Some(*span)),
            Stmt::Labeled { stmt, .. } => self.statement(stmt, span),
            Stmt::Empty | Stmt::Break | Stmt::Continue => Ok(()),
            Stmt::Expr(expr) | Stmt::Throw(expr) => self.expr(expr),
            Stmt::Return(expr) => expr.iter().try_for_each(|expr| self.expr(expr)),
            Stmt::Block(body) => self.block(body),
            Stmt::Var { declarations, .. } => declarations.iter().try_for_each(|declaration| {
                self.pattern(&declaration.pattern)?;
                declaration.init.iter().try_for_each(|init| self.expr(init))
            }),
            Stmt::Enum { members, .. } => members
                .iter()
                .try_for_each(|member| self.expr(&member.value)),
            Stmt::Function { function, .. } => self.function(function, span),
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test)?;
                self.statement(consequent, span)?;
                alternate
                    .iter()
                    .try_for_each(|alternate| self.statement(alternate, span))
            }
            Stmt::While { test, body } | Stmt::DoWhile { body, test, .. } => {
                self.expr(test)?;
                self.statement(body, span)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                if let Some(init) = init {
                    if let Stmt::Var { kind, declarations } = init.unlabeled()
                        && *kind != VarKind::Var
                    {
                        let mut names = Vec::new();
                        for declaration in declarations {
                            pattern_names(&declaration.pattern, span, &mut names);
                        }
                        self.lexical_head(&names, body)?;
                    }
                    self.statement(init, span)?;
                }
                test.iter().try_for_each(|test| self.expr(test))?;
                update.iter().try_for_each(|update| self.expr(update))?;
                self.statement(body, span)
            }
            Stmt::ForOf {
                pattern,
                kind,
                iterable: object,
                body,
            }
            | Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => {
                if matches!(kind, Some(VarKind::Let | VarKind::Const)) {
                    let mut names = Vec::new();
                    pattern_names(pattern, span, &mut names);
                    self.lexical_head(&names, body)?;
                }
                self.pattern(pattern)?;
                self.expr(object)?;
                self.statement(body, span)
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.expr(discriminant)?;
                self.case_block(cases)
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                self.block(body)?;
                if let Some(catch) = catch {
                    self.catch(catch, span)?;
                }
                finally.iter().try_for_each(|finally| self.block(finally))
            }
        }
    }

    /// A `for` head's lexical declaration: its names are distinct and not
    /// var-declared in the loop body.
    fn lexical_head(&mut self, names: &[Declared], body: &Stmt) -> Result<(), Diagnostic> {
        if let Some(duplicate) = first_duplicate(names) {
            return Err(already_declared(duplicate));
        }
        let mut vars = Vec::new();
        var_declared([body], None, &mut vars);
        match first_shared(&vars, names) {
            Some(shared) => Err(already_declared(shared)),
            None => Ok(()),
        }
    }

    fn case_block(&mut self, cases: &[SwitchCase]) -> Result<(), Diagnostic> {
        for case in cases {
            case.test.iter().try_for_each(|test| self.expr(test))?;
        }
        self.block_scope(
            &cases
                .iter()
                .flat_map(|case| case.consequent.iter())
                .collect::<Vec<_>>(),
        )
    }

    /// A catch parameter is not redeclared lexically in its block, nor by a
    /// `var` when it is a pattern. A simple parameter may share a `var`'s
    /// name, as Annex B.3.4 allows and Node and `tsc` accept.
    fn catch(&mut self, catch: &Catch, span: Option<SourceSpan>) -> Result<(), Diagnostic> {
        if let Some(binding) = &catch.binding {
            let mut parameter = Vec::new();
            pattern_names(binding, span, &mut parameter);
            if let Some(duplicate) = first_duplicate(&parameter) {
                return Err(already_declared(duplicate));
            }
            let lexical = lexically_declared(&catch.body, true);
            if let Some(shared) = first_shared(&lexical, &parameter) {
                return Err(already_declared(shared));
            }
            if !matches!(binding, Pattern::Ident(..)) {
                let mut vars = Vec::new();
                var_declared(&catch.body, None, &mut vars);
                if let Some(shared) = first_shared(&vars, &parameter) {
                    return Err(already_declared(shared));
                }
            }
            self.pattern(binding)?;
        }
        self.block(&catch.body)
    }

    fn function(
        &mut self,
        function: &Function,
        span: Option<SourceSpan>,
    ) -> Result<(), Diagnostic> {
        match &function.body {
            FunctionBody::Block(body) => self.var_scope(&function.params, body, span),
            FunctionBody::Expression(body) => {
                self.var_scope(&function.params, &[], span)?;
                self.expr(body)
            }
        }
    }

    fn pattern(&mut self, pattern: &Pattern) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Ident(..) => Ok(()),
            Pattern::Rest(pattern) => self.pattern(pattern),
            Pattern::Member { object, property } => {
                self.expr(object)?;
                self.member_property(property)
            }
            Pattern::Assign { target, default } => {
                self.pattern(target)?;
                self.expr(default)
            }
            Pattern::Array { elements, rest } => {
                elements
                    .iter()
                    .flatten()
                    .try_for_each(|element| self.pattern(element))?;
                rest.iter().try_for_each(|rest| self.pattern(rest))
            }
            Pattern::Object { properties, rest } => {
                for property in properties {
                    self.property_key(&property.key)?;
                    self.pattern(&property.value)?;
                }
                rest.iter().try_for_each(|rest| self.pattern(rest))
            }
        }
    }

    fn property_key(&mut self, key: &PropertyKey) -> Result<(), Diagnostic> {
        match key {
            PropertyKey::Static(_) => Ok(()),
            PropertyKey::Computed(expr) => self.expr(expr),
        }
    }

    fn member_property(&mut self, property: &MemberProperty) -> Result<(), Diagnostic> {
        match property {
            MemberProperty::Field(_) => Ok(()),
            MemberProperty::Index(expr) => self.expr(expr),
        }
    }

    fn call_args(&mut self, args: &[CallArg]) -> Result<(), Diagnostic> {
        args.iter().try_for_each(|arg| match arg {
            CallArg::Value(expr) | CallArg::Spread(expr) => self.expr(expr),
        })
    }

    fn assign_target(&mut self, target: &AssignTarget) -> Result<(), Diagnostic> {
        match target {
            AssignTarget::Ident(_) => Ok(()),
            AssignTarget::Member { object, property } => {
                self.expr(object)?;
                self.member_property(property)
            }
            AssignTarget::Pattern(pattern) => self.pattern(pattern),
        }
    }

    fn expr(&mut self, expr: &Expr) -> Result<(), Diagnostic> {
        match expr {
            Expr::Undefined
            | Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::RegExp { .. }
            | Expr::Ident(..)
            | Expr::This
            | Expr::LoneSurrogateString => Ok(()),
            Expr::Array(elements) => elements.iter().try_for_each(|element| match element {
                ArrayElement::Value(expr) | ArrayElement::Spread(expr) => self.expr(expr),
            }),
            Expr::Object(properties) => properties.iter().try_for_each(|property| match property {
                ObjectProperty::KeyValue(key, value) => {
                    self.property_key(key)?;
                    self.expr(value)
                }
                ObjectProperty::Spread(expr) => self.expr(expr),
            }),
            Expr::Assign { target, value, .. } => {
                self.assign_target(target)?;
                self.expr(value)
            }
            Expr::Update { target, .. } => self.assign_target(target),
            Expr::Member {
                object, property, ..
            }
            | Expr::Delete { object, property } => {
                self.expr(object)?;
                self.member_property(property)
            }
            Expr::Unary { value, .. } | Expr::Await { value, .. } => self.expr(value),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.expr(test)?;
                self.expr(consequent)?;
                self.expr(alternate)
            }
            Expr::Template { expressions, .. } => {
                expressions.iter().try_for_each(|expr| self.expr(expr))
            }
            Expr::Function(function) => self.function(function, None),
            Expr::Call { callee, args, .. } => {
                self.expr(callee)?;
                self.call_args(args)
            }
            Expr::New { args, .. } => self.call_args(args),
            Expr::OptionalChain { base, operations } => {
                self.expr(base)?;
                operations.iter().try_for_each(|operation| match operation {
                    OptionalOperation::Member { property, .. } => self.member_property(property),
                    OptionalOperation::Call { args, .. } => self.call_args(args),
                })
            }
        }
    }
}

/// The names a statement list declares lexically at its own level:
/// `let` and `const`, and in a block (`functions_are_lexical`) its function
/// declarations too.
fn lexically_declared<'a>(
    body: impl IntoIterator<Item = &'a Stmt>,
    functions_are_lexical: bool,
) -> Vec<Declared> {
    let mut names = Vec::new();
    for stmt in body {
        let span = statement_span(stmt);
        match stmt.unlabeled() {
            Stmt::Var { kind, declarations } if *kind != VarKind::Var => {
                for declaration in declarations {
                    pattern_names(&declaration.pattern, span, &mut names);
                }
            }
            Stmt::Function { name, .. } if functions_are_lexical => names.push(Declared {
                name: name.clone(),
                span,
            }),
            _ => {}
        }
    }
    names
}

/// A var scope's own function declarations, which are var-scoped there.
fn top_level_functions(body: &[Stmt]) -> Vec<Declared> {
    body.iter()
        .filter_map(|stmt| match stmt.unlabeled() {
            Stmt::Function { name, .. } => Some(Declared {
                name: name.clone(),
                span: statement_span(stmt),
            }),
            _ => None,
        })
        .collect()
}

/// The names `var` declares anywhere in `body` short of a nested function.
fn var_declared<'a>(
    body: impl IntoIterator<Item = &'a Stmt>,
    span: Option<SourceSpan>,
    names: &mut Vec<Declared>,
) {
    for stmt in body {
        var_declared_in(stmt, statement_span(stmt).or(span), names);
    }
}

fn var_declared_in(stmt: &Stmt, span: Option<SourceSpan>, names: &mut Vec<Declared>) {
    match stmt {
        Stmt::Spanned(span, stmt) => var_declared_in(stmt, Some(*span), names),
        Stmt::Labeled { stmt, .. } => var_declared_in(stmt, span, names),
        Stmt::Var {
            kind: VarKind::Var,
            declarations,
        } => {
            for declaration in declarations {
                pattern_names(&declaration.pattern, span, names);
            }
        }
        Stmt::Block(body) => var_declared(body, span, names),
        Stmt::If {
            consequent,
            alternate,
            ..
        } => {
            var_declared_in(consequent, span, names);
            if let Some(alternate) = alternate {
                var_declared_in(alternate, span, names);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            var_declared_in(body, span, names);
        }
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                var_declared_in(init, span, names);
            }
            var_declared_in(body, span, names);
        }
        Stmt::ForOf {
            pattern,
            kind,
            body,
            ..
        }
        | Stmt::ForIn {
            pattern,
            kind,
            body,
            ..
        } => {
            if *kind == Some(VarKind::Var) {
                pattern_names(pattern, span, names);
            }
            var_declared_in(body, span, names);
        }
        Stmt::Switch { cases, .. } => {
            for case in cases {
                var_declared(&case.consequent, span, names);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            var_declared(body, span, names);
            if let Some(catch) = catch {
                var_declared(&catch.body, span, names);
            }
            if let Some(finally) = finally {
                var_declared(finally, span, names);
            }
        }
        _ => {}
    }
}

fn statement_span(stmt: &Stmt) -> Option<SourceSpan> {
    match stmt {
        Stmt::Spanned(span, _) => Some(*span),
        Stmt::Labeled { stmt, .. } => statement_span(stmt),
        _ => None,
    }
}

fn pattern_list_names(patterns: &[Pattern], span: Option<SourceSpan>) -> Vec<Declared> {
    let mut names = Vec::new();
    for pattern in patterns {
        pattern_names(pattern, span, &mut names);
    }
    names
}

/// The BoundNames of a binding pattern.
fn pattern_names(pattern: &Pattern, span: Option<SourceSpan>, names: &mut Vec<Declared>) {
    match pattern {
        Pattern::Ident(name, _) => names.push(Declared {
            name: name.clone(),
            span,
        }),
        Pattern::Rest(pattern)
        | Pattern::Assign {
            target: pattern, ..
        } => pattern_names(pattern, span, names),
        Pattern::Member { .. } => {}
        Pattern::Array { elements, rest } => {
            for element in elements.iter().flatten() {
                pattern_names(element, span, names);
            }
            if let Some(rest) = rest {
                pattern_names(rest, span, names);
            }
        }
        Pattern::Object { properties, rest } => {
            for property in properties {
                pattern_names(&property.value, span, names);
            }
            if let Some(rest) = rest {
                pattern_names(rest, span, names);
            }
        }
    }
}
