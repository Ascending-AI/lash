//! Statements.

use std::collections::BTreeSet;

use lash_kernel_doc::{
    AssignOrder, Closure, Expr, Literal, Member, Name, Place, Protocol, Rhs, Stmt,
};
use ruff_python_ast::{self as ast, Expr as PyExpr, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextRange};

use super::captions::Arm;
use super::{Active, Buf, Lowerer, Lowering, Note, Operand, Scope, ScopeKind};
use crate::diagnostics::{self, Code};
use crate::exceptions::Catches;
use crate::scope::{self, Ty};

/// What a function's kernel block runs.
pub(super) enum Body<'a> {
    Suite(&'a [PyStmt]),
    /// A lambda: the block returns the expression.
    Value(&'a PyExpr),
}

/// What a `for` walks, and the protocol it walks it through.
pub(super) type ForHeader = (Operand, Protocol);

impl Lowerer<'_> {
    pub(super) fn statements(&mut self, body: &[PyStmt]) -> Lowering<()> {
        for statement in body {
            self.statement(statement)?;
        }
        Ok(())
    }

    /// A Python suite as a kernel block.
    fn suite(&mut self, body: &[PyStmt]) -> Lowering<Buf> {
        let span = self.span;
        let block = self.block(|this| this.statements(body));
        self.span = span;
        block
    }

    pub(super) fn single(&self, stmt: Stmt) -> Buf {
        Buf {
            stmts: vec![stmt],
            notes: vec![Note {
                span: self.span,
                ..Note::default()
            }],
        }
    }

    fn statement(&mut self, statement: &PyStmt) -> Lowering<()> {
        self.span_of(statement);
        let span = self.span;
        let first = self.buf.stmts.len();
        self.nested(statement.range(), |this| {
            this.statement_inner(statement, span)
        })?;
        self.own_caption(statement, first);
        Ok(())
    }

    fn statement_inner(
        &mut self,
        statement: &PyStmt,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        match statement {
            PyStmt::Expr(expression) => {
                // A docstring and any other bare constant does nothing.
                if expression.value.is_literal_expr() {
                    return Ok(());
                }
                let value = self.expr(&expression.value)?;
                self.discard(value);
                Ok(())
            }
            PyStmt::Assign(assign) => {
                for target in &assign.targets {
                    self.check_assignment_repair(target, &assign.value, "=")?;
                }
                let value = self.expr(&assign.value)?;
                match assign.targets.as_slice() {
                    [target] => self.assign(target, value),
                    targets => {
                        let value = self.pin(value);
                        for target in targets {
                            self.assign(target, value.clone())?;
                        }
                        Ok(())
                    }
                }
            }
            PyStmt::AnnAssign(assign) => match &assign.value {
                Some(value) => {
                    self.check_assignment_repair(&assign.target, value, "=")?;
                    let value = self.expr(value)?;
                    self.assign(&assign.target, value)
                }
                None => Ok(()),
            },
            PyStmt::AugAssign(assign) => {
                self.check_assignment_repair(
                    &assign.target,
                    &assign.value,
                    &format!("{}=", assign.op.as_str()),
                )?;
                self.augmented(assign)
            }
            PyStmt::Return(ret) => {
                if self.in_module() {
                    return Err(diagnostics::diagnostic(
                        Code::ReturnOutsideFunction,
                        "`return` outside a function",
                        ret.range,
                    ));
                }
                let value = match &ret.value {
                    Some(value) => self.expr(value)?,
                    None => Operand::none(),
                };
                self.return_value(value);
                Ok(())
            }
            PyStmt::Pass(_) => Ok(()),
            PyStmt::Break(node) => {
                if self.scope().loops == 0 {
                    return Err(diagnostics::diagnostic(
                        Code::Syntax,
                        "`break` outside a loop",
                        node.range,
                    ));
                }
                self.emit(Stmt::Break);
                Ok(())
            }
            PyStmt::Continue(node) => {
                if self.scope().loops == 0 {
                    return Err(diagnostics::diagnostic(
                        Code::Syntax,
                        "`continue` outside a loop",
                        node.range,
                    ));
                }
                self.emit(Stmt::Continue);
                Ok(())
            }
            PyStmt::Global(global) => {
                for name in &global.names {
                    let id = name.id.as_str();
                    let between = &self.scopes[1..self.scopes.len().saturating_sub(1).max(1)];
                    if between
                        .iter()
                        .any(|scope| scope.bindings.locals.contains(id))
                    {
                        return Err(diagnostics::with_repair(
                            Code::GlobalShadowed,
                            format!(
                                "`global {id}` inside a function that an enclosing function's `{id}` hides"
                            ),
                            name.range,
                            format!("rename one of the two `{id}` variables"),
                        ));
                    }
                }
                Ok(())
            }
            PyStmt::Nonlocal(_) => Ok(()),
            PyStmt::Import(import) => {
                for alias in &import.names {
                    let module = alias.name.id.as_str();
                    let renamed = alias
                        .asname
                        .as_ref()
                        .is_some_and(|asname| asname.id.as_str() != module);
                    if module != "asyncio" || renamed {
                        return Err(unsupported_import(module, alias.range));
                    }
                }
                Ok(())
            }
            PyStmt::ImportFrom(import) => {
                let module = import
                    .module
                    .as_ref()
                    .map_or("", |module| module.id.as_str());
                Err(unsupported_import(module, import.range))
            }
            PyStmt::If(branch) => {
                let test = self.expr(&branch.test)?;
                let condition = self.truth(test)?;
                let then_block = self.suite(&branch.body)?;
                let else_block = self.block(|this| this.clauses(&branch.elif_else_clauses))?;
                self.span = span;
                self.emit_if(condition, then_block, else_block);
                let otherwise = self.else_caption(&branch.elif_else_clauses);
                self.caption_arm(Arm::Last, otherwise);
                Ok(())
            }
            PyStmt::While(while_loop) => self.while_loop(while_loop, span),
            PyStmt::For(for_loop) => self.for_loop(for_loop, span),
            PyStmt::Try(attempt) => self.attempt(attempt, span),
            PyStmt::Raise(raise) => self.raise(raise),
            PyStmt::Assert(assert) => {
                let test = self.expr(&assert.test)?;
                let holds = self.truth(test)?;
                let fails = self.not(holds)?;
                let then_block = self.block(|this| {
                    let args = match &assert.msg {
                        Some(message) => vec![this.expr(message)?.expr],
                        None => Vec::new(),
                    };
                    let args = Operand::inline(Expr::Tuple(args), Ty::Tuple);
                    let error = this.raised("AssertionError", args, Operand::none())?;
                    this.emit(Stmt::Throw { value: error.expr });
                    Ok(())
                })?;
                self.span = span;
                self.emit_if(fails, then_block, Buf::default());
                Ok(())
            }
            PyStmt::Delete(delete) => {
                for target in &delete.targets {
                    match target {
                        PyExpr::Subscript(subscript)
                            if !matches!(subscript.slice.as_ref(), PyExpr::Slice(_)) =>
                        {
                            let operands = self.operands(&[&subscript.value, &subscript.slice])?;
                            self.invoke_do("py.delitem", &operands)?;
                        }
                        other => {
                            return Err(diagnostics::with_repair(
                                Code::DeleteUnsupported,
                                "`del` removes one item of a list or a dict here, nothing else",
                                other.range(),
                                "write `del container[key]`; a variable cannot be unbound",
                            ));
                        }
                    }
                }
                Ok(())
            }
            PyStmt::FunctionDef(def) => self.function_def(def),
            PyStmt::ClassDef(class) => {
                let name = class.name.id.as_str();
                if self.in_module()
                    && (self.classes.contains(name) || self.typed_dicts.contains_key(name))
                {
                    return Ok(());
                }
                Err(class_refusal(class.range, class.name.id.as_str()))
            }
            PyStmt::With(with) => {
                for item in &with.items {
                    if let PyExpr::Call(call) = &item.context_expr
                        && let PyExpr::Attribute(attribute) = call.func.as_ref()
                        && let Some(refusal) = self.printed_receiver(&attribute.value)
                    {
                        return Err(refusal);
                    }
                }
                Err(diagnostics::with_repair(
                    Code::WithUnsupported,
                    "`with` is not in the dialect",
                    with.range,
                    "write `try:` and `finally:` around the block",
                ))
            }
            PyStmt::Match(node) => Err(diagnostics::with_repair(
                Code::MatchUnsupported,
                "`match` is not in the dialect",
                node.range,
                "write `if` and `elif` tests",
            )),
            PyStmt::TypeAlias(node) => Err(diagnostics::with_repair(
                Code::SyntaxUnsupported,
                "a `type` statement is not in the dialect",
                node.range,
                "remove it; annotations may name the type directly",
            )),
            PyStmt::IpyEscapeCommand(node) => Err(diagnostics::diagnostic(
                Code::Syntax,
                "an IPython escape is not Python",
                node.range,
            )),
        }
    }

    /// The `elif` and `else` clauses of an `if`, as its else block.
    fn clauses(&mut self, clauses: &[ast::ElifElseClause]) -> Lowering<()> {
        let Some((clause, rest)) = clauses.split_first() else {
            return Ok(());
        };
        self.span_of(clause);
        let span = self.span;
        match &clause.test {
            None => self.statements(&clause.body),
            Some(test) => {
                let test = self.expr(test)?;
                let condition = self.truth(test)?;
                let then_block = self.suite(&clause.body)?;
                let else_block = self.block(|this| this.clauses(rest))?;
                self.span = span;
                self.emit_if(condition, then_block, else_block);
                // An `elif`'s caption names its own check, the nested `if`.
                if let Some(caption) = self.caption_at(clause.start())
                    && let Some(note) = self.buf.notes.last_mut()
                {
                    note.caption = Some(caption);
                }
                let otherwise = self.else_caption(rest);
                self.caption_arm(Arm::Last, otherwise);
                Ok(())
            }
        }
    }

    /// The caption of the `else:` that `clauses` start with, when they do.
    fn else_caption(&self, clauses: &[ast::ElifElseClause]) -> Option<lash_kernel_doc::Caption> {
        let clause = clauses.first().filter(|clause| clause.test.is_none())?;
        self.caption_at(clause.start())
    }

    fn while_loop(
        &mut self,
        while_loop: &ast::StmtWhile,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        self.python_while(
            |this| {
                let test = this.expr(&while_loop.test)?;
                this.truth(test)
            },
            |this| this.statements(&while_loop.body),
            &while_loop.orelse,
            self.loop_else_caption(&while_loop.body, &while_loop.orelse),
            span,
        )
    }

    /// A Python `while`, whose test `test` lowers and whose suite `body`
    /// lowers, as a kernel `while` with its `else`. A test that needs
    /// statements of its own runs at the top of each pass, inside the
    /// loop: it leaves by `break` when the loop has no `else`, and otherwise
    /// records its answer for the loop's own test, so that the `else` runs.
    pub(super) fn python_while(
        &mut self,
        test: impl FnOnce(&mut Self) -> Lowering<Expr>,
        body: impl FnOnce(&mut Self) -> Lowering<()>,
        orelse: &[PyStmt],
        else_caption: Option<lash_kernel_doc::Caption>,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        let mut simple = None;
        let mut going = None;
        let block = self.block(|this| {
            // Actions stay nested in the condition (`K-EVAL-008`).
            let condition = test(this)?;
            if this.buf.is_empty() {
                simple = Some(condition);
                return this.loop_body(body);
            }
            if orelse.is_empty() {
                let done = this.not(condition)?;
                let leave = this.single(Stmt::Break);
                this.emit_if(done, leave, Buf::default());
                return this.loop_body(body);
            }
            let go = this.fresh("go");
            this.emit(Stmt::Assign {
                place: Place::Variable(go.clone()),
                value: Rhs::Expr(condition),
                order: AssignOrder::ValueFirst,
            });
            let pass = this.block(|this| this.loop_body(body))?;
            this.emit_if(Expr::Variable(go.clone()), pass, Buf::default());
            going = Some(go);
            Ok(())
        })?;
        let orelse = self.block(|this| this.statements(orelse))?;
        self.span = span;
        let condition = match (simple, going) {
            (Some(condition), _) => condition,
            (None, Some(go)) => {
                self.emit(Stmt::Let {
                    name: go.clone(),
                    value: Rhs::Expr(Expr::Literal(Literal::Bool(true))),
                });
                Expr::Variable(go)
            }
            (None, None) => Expr::Literal(Literal::Bool(true)),
        };
        let otherwise = (!orelse.is_empty()).then_some(else_caption).flatten();
        self.emit_while(condition, block, orelse);
        self.caption_arm(Arm::Last, otherwise);
        Ok(())
    }

    /// The suite of a loop, where `break` and `continue` are this loop's.
    fn loop_body(&mut self, body: impl FnOnce(&mut Self) -> Lowering<()>) -> Lowering<()> {
        self.scope_mut().loops += 1;
        let result = body(self);
        self.scope_mut().loops -= 1;
        result
    }

    /// What a `for` walks, and the protocol it walks it through
    /// (`K-ITER-005`): a value through `py.for_in`, which checks a dict or
    /// a set for changes as CPython's iterator does (FIG-6406), and a dict
    /// view through its own protocol over the dict.
    pub(super) fn for_header(&mut self, iter: &PyExpr) -> Lowering<ForHeader> {
        if let PyExpr::Call(call) = iter {
            match call.func.as_ref() {
                // `for k, v in d.items()`: the dict itself is walked, and
                // each pass binds what the view would give.
                PyExpr::Attribute(attribute)
                    if call.arguments.is_empty()
                        && matches!(attribute.attr.id.as_str(), "items" | "keys" | "values")
                        && !self.is_asyncio(&attribute.value) =>
                {
                    let receiver = self.expr(&attribute.value)?;
                    let view = attribute.attr.id.as_str();
                    let item = match view {
                        "items" => Some(self.function("py.for_in.item")?),
                        "values" => Some(self.function("py.for_in.value")?),
                        _ => None,
                    };
                    let via = Protocol {
                        open: self.function(&format!("py.for_in.{view}"))?,
                        next: None,
                        item,
                        check: Some(self.function("py.for_in.check")?),
                    };
                    return Ok((receiver, via));
                }
                _ => {}
            }
        }
        let value = self.expr(iter)?;
        let via = Protocol {
            open: self.function("py.for_in.open")?,
            next: None,
            item: None,
            check: Some(self.function("py.for_in.check")?),
        };
        Ok((value, via))
    }

    fn for_loop(
        &mut self,
        for_loop: &ast::StmtFor,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        if for_loop.is_async {
            return Err(diagnostics::with_repair(
                Code::AsyncUnsupported,
                "`async for` is not in the dialect",
                for_loop.range,
                "await inside an ordinary `for`",
            ));
        }
        if let PyExpr::Call(call) = for_loop.iter.as_ref()
            && matches!(call.func.as_ref(), PyExpr::Name(name) if name.id.as_str() == "range")
            && self.variable("range").is_none()
            && self.effect(call).is_none()
        {
            return self.range_loop(for_loop, call, span);
        }
        let (walked, via) = self.for_header(&for_loop.iter)?;
        let binding = self.temp();
        let body = self.block(|this| {
            this.assign(
                &for_loop.target,
                Operand::temp(binding.clone(), Ty::Unknown),
            )?;
            this.loop_body(|this| this.statements(&for_loop.body))
        })?;
        let orelse = self.block(|this| this.statements(&for_loop.orelse))?;
        self.span = span;
        let otherwise = (!orelse.is_empty())
            .then(|| self.loop_else_caption(&for_loop.body, &for_loop.orelse))
            .flatten();
        self.emit_for(binding, walked.expr, Some(via), body, orelse);
        self.caption_arm(Arm::Last, otherwise);
        Ok(())
    }

    /// Writes `value` to an assignment target.
    pub(super) fn assign(&mut self, target: &PyExpr, value: Operand) -> Lowering<()> {
        match target {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                let variable = self.written(id, name.range)?;
                let annotated = self
                    .variable(id)
                    .and_then(|found| found.scope)
                    .and_then(|scope| self.scopes[scope].bindings.types.get(id).copied());
                let value = if annotated.is_some_and(|ty| {
                    ty != Ty::Unknown && value.ty != Ty::Unknown && ty != value.ty
                }) {
                    // A trusted annotation chooses an operation, but does not
                    // make the actual value satisfy its type before execution.
                    self.invoke(
                        "py.name_check",
                        &[
                            value,
                            Operand::text(id),
                            Operand::literal(Literal::Bool(!self.in_module()), Ty::Bool),
                        ],
                        Ty::Unknown,
                    )?
                } else {
                    value
                };
                self.store(Place::Variable(variable), value);
                self.mark_bound(id);
                Ok(())
            }
            PyExpr::Subscript(subscript) => {
                if matches!(subscript.slice.as_ref(), PyExpr::Slice(_)) {
                    return Err(diagnostics::with_repair(
                        Code::TargetUnsupported,
                        "assignment to a slice is not in the dialect",
                        subscript.range,
                        format!(
                            "build the replacement list and assign it to `{}` whole",
                            self.text(subscript.value.range())
                        ),
                    ));
                }
                // The value is evaluated before the target's parts.
                let value = self.pin(value);
                let mut operands = self.operands(&[&subscript.value, &subscript.slice])?;
                operands.push(value);
                self.invoke_do("py.setitem", &operands)
            }
            PyExpr::Tuple(ast::ExprTuple { elts, .. })
            | PyExpr::List(ast::ExprList { elts, .. }) => {
                if let Some(starred) = elts.iter().find(|elt| elt.is_starred_expr()) {
                    return Err(diagnostics::with_repair(
                        Code::StarUnsupported,
                        "a starred assignment target is not in the dialect",
                        starred.range(),
                        format!(
                            "take the values assigned to `{}` with indices and slices of the assigned sequence",
                            self.text(target.range())
                        ),
                    ));
                }
                let count = Operand::literal(
                    Literal::Int(i64::try_from(elts.len()).unwrap_or(i64::MAX).into()),
                    Ty::Int,
                );
                let items = self.invoke("py.unpack", &[value, count], Ty::Unknown)?;
                let items = self.pin(items);
                for (index, elt) in elts.iter().enumerate() {
                    let item = Expr::Member(Box::new(Member::Index {
                        target: items.expr.clone(),
                        index: Expr::Literal(Literal::Int(
                            i64::try_from(index).unwrap_or(i64::MAX).into(),
                        )),
                    }));
                    self.assign(elt, Operand::inline(item, Ty::Unknown))?;
                }
                Ok(())
            }
            other => Err(diagnostics::with_repair(
                Code::TargetUnsupported,
                "an assignment writes a variable, an item, or a tuple of those",
                other.range(),
                self.target_repair(other),
            )),
        }
    }

    fn augmented(&mut self, assign: &ast::StmtAugAssign) -> Lowering<()> {
        match assign.target.as_ref() {
            PyExpr::Name(name) => {
                let operands = self.operands(&[&assign.target, &assign.value])?;
                let result = self.in_place(assign, operands[0].clone(), operands[1].clone())?;
                let variable = self.written(name.id.as_str(), name.range)?;
                self.store(Place::Variable(variable), result);
                Ok(())
            }
            PyExpr::Subscript(subscript)
                if !matches!(subscript.slice.as_ref(), PyExpr::Slice(_)) =>
            {
                let place = self.operands(&[&subscript.value, &subscript.slice])?;
                let container = self.pin(place[0].clone());
                let index = self.pin(place[1].clone());
                let current = self.invoke(
                    "py.getitem",
                    &[container.clone(), index.clone()],
                    Ty::Unknown,
                )?;
                let (aside, operand) = self.aside(|this| this.expr(&assign.value))?;
                let current = if aside.is_empty() {
                    current
                } else {
                    self.pin(current)
                };
                self.append(aside);
                let result = self.in_place(assign, current, operand)?;
                self.invoke_do("py.setitem", &[container, index, result])
            }
            other => Err(diagnostics::with_repair(
                Code::TargetUnsupported,
                "an augmented assignment writes a variable or an item",
                other.range(),
                self.target_repair(other),
            )),
        }
    }

    /// `left op= right`. `+=` extends a list in place.
    fn in_place(
        &mut self,
        assign: &ast::StmtAugAssign,
        left: Operand,
        right: Operand,
    ) -> Lowering<Operand> {
        let typed = (left.ty.is_number() && right.ty.is_number())
            || (left.ty == Ty::Str && right.ty == Ty::Str);
        if assign.op == ast::Operator::Add && !typed {
            return self.invoke("py.iadd", &[left, right], Ty::Unknown);
        }
        self.binary(
            assign.op,
            left,
            right,
            assign.range,
            [assign.target.range(), assign.value.range()],
        )
    }

    fn attempt(
        &mut self,
        attempt: &ast::StmtTry,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        if attempt.is_star {
            return Err(diagnostics::with_repair(
                Code::SyntaxUnsupported,
                "`except*` is not in the dialect",
                attempt.range,
                "catch the exception with `except`",
            ));
        }
        let has_else = !attempt.orelse.is_empty();
        if !attempt.finalbody.is_empty() && scope::reraises(&attempt.finalbody, &self.reraisers()) {
            return self.finally_reraising(attempt, span);
        }
        if attempt.handlers.is_empty() {
            // `try` and `finally` alone.
            let body = self.suite(&attempt.body)?;
            let finally = self.suite(&attempt.finalbody)?;
            self.span = span;
            self.emit_try(body, None, Some(finally));
            self.caption_arm(Arm::Last, self.finally_caption(attempt));
            return Ok(());
        }
        let caught = self.temp();
        if attempt.finalbody.is_empty() {
            return self.handled(attempt, &caught, None, span);
        }
        if !has_else {
            let finally = self.suite(&attempt.finalbody)?;
            return self.handled(attempt, &caught, Some(finally), span);
        }
        // The `else` suite is outside the handlers and inside the
        // `finally`.
        let body = self.block(|this| this.handled(attempt, &caught, None, span))?;
        let finally = self.suite(&attempt.finalbody)?;
        self.span = span;
        self.emit_try(body, None, Some(finally));
        self.caption_arm(Arm::Last, self.finally_caption(attempt));
        Ok(())
    }

    /// A `try` whose `finally` may raise again the exception being handled
    /// when it runs: the one unwinding through it, or else the one handled
    /// around the `try`.
    fn finally_reraising(
        &mut self,
        attempt: &ast::StmtTry,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        let pending = self.fresh("pending");
        self.emit(Stmt::Let {
            name: pending.clone(),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
        let body = self.block(|this| {
            if attempt.handlers.is_empty() {
                return this.statements(&attempt.body);
            }
            let caught = this.temp();
            this.handled(attempt, &caught, None, span)
        })?;
        let raised = self.temp();
        let record = self.block(|this| {
            this.emit(Stmt::Assign {
                place: Place::Variable(pending.clone()),
                value: Rhs::Expr(Expr::Variable(raised.clone())),
                order: AssignOrder::ValueFirst,
            });
            this.emit(Stmt::Throw {
                value: Expr::Variable(raised.clone()),
            });
            Ok(())
        })?;
        let finally = self.block(|this| {
            let active = this.fresh("active");
            this.emit(Stmt::Let {
                name: active.clone(),
                value: Rhs::Expr(Expr::Variable(pending.clone())),
            });
            let outer = this.active();
            if !matches!(outer.expr, Expr::Literal(Literal::Absent)) {
                let none = this.native(
                    "same",
                    vec![
                        Expr::Variable(active.clone()),
                        Expr::Literal(Literal::Absent),
                    ],
                )?;
                let inherit = this.single(Stmt::Assign {
                    place: Place::Variable(active.clone()),
                    value: Rhs::Expr(outer.expr),
                    order: AssignOrder::ValueFirst,
                });
                this.emit_if(none, inherit, Buf::default());
            }
            this.scope_mut().handlers.push(Active::Passed(active));
            let result = this.statements(&attempt.finalbody);
            this.scope_mut().handlers.pop();
            result
        })?;
        self.span = span;
        self.emit_try(body, Some((raised, record)), Some(finally));
        self.caption_arm(Arm::Last, self.finally_caption(attempt));
        Ok(())
    }

    /// `try`, its handlers and its `else`, with `finally` when the caller
    /// has one that can sit on the same kernel `try`.
    fn handled(
        &mut self,
        attempt: &ast::StmtTry,
        caught: &Name,
        finally: Option<Buf>,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        let has_else = !attempt.orelse.is_empty();
        let passed = has_else.then(|| {
            let flag = self.fresh("passed");
            self.emit(Stmt::Let {
                name: flag.clone(),
                value: Rhs::Expr(Expr::Literal(Literal::Bool(false))),
            });
            flag
        });
        let body = self.block(|this| {
            this.statements(&attempt.body)?;
            if let Some(flag) = &passed {
                this.emit(Stmt::Assign {
                    place: Place::Variable(flag.clone()),
                    value: Rhs::Expr(Expr::Literal(Literal::Bool(true))),
                    order: AssignOrder::ValueFirst,
                });
            }
            Ok(())
        })?;
        let handlers = self.block(|this| this.handlers(&attempt.handlers, caught))?;
        self.span = span;
        let finally_caption = finally.as_ref().and_then(|_| self.finally_caption(attempt));
        self.emit_try(body, Some((caught.clone(), handlers)), finally);
        self.caption_arm(Arm::Catch, self.except_caption(attempt));
        self.caption_arm(Arm::Last, finally_caption);
        if let Some(flag) = passed {
            let orelse = self.suite(&attempt.orelse)?;
            self.span = span;
            self.emit_if(Expr::Variable(flag), orelse, Buf::default());
        }
        Ok(())
    }

    /// The `except` clauses, tried in order against the raised value; one
    /// that no clause takes is raised again.
    fn handlers(&mut self, handlers: &[ast::ExceptHandler], caught: &Name) -> Lowering<()> {
        let Some((ast::ExceptHandler::ExceptHandler(handler), rest)) = handlers.split_first()
        else {
            self.emit(Stmt::Throw {
                value: Expr::Variable(caught.clone()),
            });
            return Ok(());
        };
        self.span_of(handler);
        let span = self.span;
        let catches = match &handler.type_ {
            None => Catches::Everything,
            Some(classes) => {
                let names = self.class_names(classes)?;
                self.classes.catches(&names)
            }
        };
        let raised = Operand::temp(caught.clone(), Ty::Unknown);
        let body = |this: &mut Self| -> Lowering<()> {
            this.scope_mut()
                .handlers
                .push(Active::Caught(caught.clone()));
            let result = match &handler.name {
                None => this.statements(&handler.body),
                Some(name) => this.named_handler(name, caught, &handler.body),
            };
            this.scope_mut().handlers.pop();
            result
        };
        let classes = |names: Vec<String>| {
            let names = names
                .into_iter()
                .map(|name| Expr::Literal(Literal::Text(name)))
                .collect();
            Operand::inline(Expr::Tuple(names), Ty::Tuple)
        };
        let matched = match catches {
            Catches::Everything => return body(self),
            Catches::Exceptions(also) => {
                self.invoke("py.exc.is_exception", &[raised, classes(also)], Ty::Bool)?
            }
            Catches::Named(names) => {
                self.invoke("py.exc.matches", &[raised, classes(names)], Ty::Bool)?
            }
        };
        let then_block = self.block(body)?;
        let else_block = self.block(|this| this.handlers(rest, caught))?;
        self.span = span;
        self.emit_if(matched.expr, then_block, else_block);
        Ok(())
    }

    /// The body of `except ... as name`. The name holds the exception in the
    /// body and is unbound however the body is left, as Python deletes it.
    fn named_handler(
        &mut self,
        name: &ast::Identifier,
        caught: &Name,
        body: &[PyStmt],
    ) -> Lowering<()> {
        let id = name.id.as_str();
        let variable = self.written(id, name.range)?;
        self.emit(Stmt::Assign {
            place: Place::Variable(variable.clone()),
            value: Rhs::Expr(Expr::Variable(caught.clone())),
            order: AssignOrder::ValueFirst,
        });
        self.mark_bound(id);
        let block = self.suite(body)?;
        let unbind = self.single(Stmt::Assign {
            place: Place::Variable(variable),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
            order: AssignOrder::ValueFirst,
        });
        self.emit_try(block, None, Some(unbind));
        self.mark_unbound(id);
        Ok(())
    }

    /// A new exception of `class` made with the tuple `args`.
    pub(super) fn raised(
        &mut self,
        class: &str,
        args: Operand,
        cause: Operand,
    ) -> Lowering<Operand> {
        // `str` of a KeyError quotes its argument.
        let quoted = Operand::literal(
            Literal::Bool(self.classes.descends(class, "KeyError")),
            Ty::Bool,
        );
        self.invoke(
            "py.exc.new",
            &[Operand::text(class), args, cause, quoted],
            Ty::Unknown,
        )
    }

    /// The exception class an expression names, when it names one.
    pub(super) fn exception_class(&self, expr: &PyExpr) -> Option<String> {
        match expr {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                (self.classes.contains(id) && self.variable(id).is_none()).then(|| id.to_string())
            }
            PyExpr::Attribute(attribute) if self.is_asyncio(&attribute.value) => {
                let id = attribute.attr.id.as_str();
                matches!(id, "CancelledError" | "TimeoutError").then(|| id.to_string())
            }
            _ => None,
        }
    }

    /// The classes an `except` clause names.
    fn class_names(&self, classes: &PyExpr) -> Lowering<Vec<String>> {
        let listed: Vec<&PyExpr> = match classes {
            PyExpr::Tuple(tuple) => tuple.elts.iter().collect(),
            single => vec![single],
        };
        listed
            .into_iter()
            .map(|class| {
                self.exception_class(class).ok_or_else(|| {
                    diagnostics::with_repair(
                        Code::ExceptionClass,
                        "`except` names exception classes the front end can see",
                        class.range(),
                        "write the class name, a built-in exception or a `class Name(Exception): pass` of this cell",
                    )
                })
            })
            .collect()
    }

    fn raise(&mut self, raise: &ast::StmtRaise) -> Lowering<()> {
        let Some(exception) = &raise.exc else {
            match self.scope().handlers.last().cloned() {
                Some(Active::Caught(caught)) => self.emit(Stmt::Throw {
                    value: Expr::Variable(caught),
                }),
                Some(Active::Passed(active)) => {
                    self.invoke_do("py.exc.reraise", &[Operand::temp(active, Ty::Unknown)])?;
                }
                None => self.invoke_do(
                    "py.fail",
                    &[
                        Operand::text("RuntimeError"),
                        Operand::text("No active exception to reraise"),
                    ],
                )?,
            }
            return Ok(());
        };
        // `raise Class(...)` and `raise Class` make the exception here.
        let made: Option<(String, &[PyExpr])> = match exception.as_ref() {
            PyExpr::Call(call) => match self.exception_class(&call.func) {
                Some(class) => {
                    self.plain_arguments(&call.arguments, "an exception")?;
                    Some((class, &call.arguments.args))
                }
                None => None,
            },
            other => self.exception_class(other).map(|class| (class, &[][..])),
        };
        let error = match made {
            Some((class, args)) => {
                let mut exprs: Vec<&PyExpr> = args.iter().collect();
                if let Some(cause) = &raise.cause {
                    exprs.push(cause);
                }
                let mut operands = self.operands(&exprs)?;
                let cause = match &raise.cause {
                    Some(_) => operands.pop().unwrap_or_else(Operand::none),
                    None => Operand::none(),
                };
                let args = Operand::inline(
                    Expr::Tuple(operands.into_iter().map(|operand| operand.expr).collect()),
                    Ty::Tuple,
                );
                self.raised(&class, args, cause)?
            }
            None => match &raise.cause {
                Some(cause) => {
                    let operands = self.operands(&[exception, cause])?;
                    self.invoke("py.exc.with_cause", &operands, Ty::Unknown)?
                }
                None => {
                    let value = self.expr(exception)?;
                    return self.invoke_do("py.exc.throw", &[value]);
                }
            },
        };
        self.emit(Stmt::Throw { value: error.expr });
        Ok(())
    }

    /// Records the exception classes the module declares, and a workflow
    /// module's `TypedDict`s, so a use may stand before its `class`
    /// statement.
    pub(super) fn declare_classes(&mut self, body: &[PyStmt]) -> Lowering<()> {
        for statement in body {
            let PyStmt::ClassDef(class) = statement else {
                continue;
            };
            let name = class.name.id.as_str();
            if self.module.is_some()
                && self.variable(name).is_none()
                && let Some(typed) = super::typed_dicts::declared(class)
            {
                self.typed_dicts.insert(name.to_owned(), typed);
                continue;
            }
            let trivial = class.body.iter().all(|statement| match statement {
                PyStmt::Pass(_) => true,
                PyStmt::Expr(expression) => expression.value.is_literal_expr(),
                _ => false,
            });
            let base = match class.arguments.as_deref() {
                Some(arguments) if arguments.keywords.is_empty() => match &*arguments.args {
                    [base] => self.exception_class(base),
                    _ => None,
                },
                _ => None,
            };
            let plain = class.decorator_list.is_empty() && class.type_params.is_none();
            match base {
                Some(base)
                    if trivial
                        && plain
                        && self.classes.is_exception(&base)
                        && self.variable(name).is_none() =>
                {
                    self.classes.declare(name, &base);
                }
                _ => return Err(class_refusal(class.range, class.name.id.as_str())),
            }
        }
        Ok(())
    }

    fn function_def(&mut self, def: &ast::StmtFunctionDef) -> Lowering<()> {
        if let Some(decorator) = def.decorator_list.first() {
            return Err(diagnostics::with_repair(
                Code::DecoratorUnsupported,
                "decorators are not in the dialect",
                decorator.range,
                format!(
                    "remove the decorator, then wrap the defined function: `{0} = ({1})({0})`",
                    def.name.id,
                    self.text(decorator.expression.range())
                ),
            ));
        }
        if def.type_params.is_some() {
            return Err(diagnostics::with_repair(
                Code::SyntaxUnsupported,
                "type parameters are not in the dialect",
                def.range,
                "remove them; the function takes any value",
            ));
        }
        if self.entry_defs.contains(&def.range) {
            return self.module_entry(def);
        }
        let name = def.name.id.as_str();
        let defaults = self.defaults(&def.parameters)?;
        // The body runs only once the name is bound.
        self.mark_bound(name);
        let active = self.takes_active(name).then(|| self.fresh("active"));
        let checked = !self.scope().bindings.filled.contains(name);
        let (params, body) = self.function_body(
            Some(&def.parameters),
            Body::Suite(&def.body),
            def.is_async,
            &defaults,
            checked.then_some(name),
            active,
        )?;
        let written =
            self.function_written(Some(&def.parameters), def.is_async, def.returns.as_deref());
        let start = super::saved::start_adapter(
            Some(&def.parameters),
            def.returns.as_deref(),
            params.len(),
        );
        let variable = self.written(name, def.name.range)?;
        self.span_of(def);
        self.push(
            Stmt::Assign {
                place: Place::Variable(variable),
                value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                    params,
                    body: body.stmts,
                    start,
                }))),
                order: AssignOrder::ValueFirst,
            },
            vec![body.notes],
        );
        if let Some(note) = self.buf.notes.last_mut() {
            note.written = Some(written);
        }
        Ok(())
    }

    /// Evaluates the parameters' defaults, once, where the function is
    /// defined.
    pub(super) fn defaults(
        &mut self,
        parameters: &ast::Parameters,
    ) -> Lowering<Vec<Option<Operand>>> {
        let unsupported = parameters
            .posonlyargs
            .first()
            .map(Ranged::range)
            .or_else(|| parameters.kwonlyargs.first().map(Ranged::range))
            .or_else(|| parameters.vararg.as_ref().map(|param| param.range))
            .or_else(|| parameters.kwarg.as_ref().map(|param| param.range));
        if let Some(range) = unsupported {
            return Err(diagnostics::with_repair(
                Code::StarUnsupported,
                "`*args`, `**kwargs`, keyword-only and positional-only parameters are not in the dialect",
                range,
                "name each parameter; pass a list or a dict for a variable number of values",
            ));
        }
        let mut defaults = Vec::with_capacity(parameters.args.len());
        for param in &parameters.args {
            defaults.push(match &param.default {
                Some(default) => {
                    let value = self.expr(default)?;
                    Some(self.pin(value))
                }
                None => None,
            });
        }
        Ok(defaults)
    }

    /// The kernel parameters and block of a function or a lambda. `checked`
    /// names the function a call that leaves a parameter without a default
    /// unfilled raises for, and is None where no call can (`always_filled`).
    /// `active` is the last parameter of a function that takes the
    /// exception its caller handles.
    pub(super) fn function_body(
        &mut self,
        parameters: Option<&ast::Parameters>,
        body: Body<'_>,
        is_async: bool,
        defaults: &[Option<Operand>],
        checked: Option<&str>,
        active: Option<Name>,
    ) -> Lowering<(Vec<Name>, Buf)> {
        let suite: &[PyStmt] = match body {
            Body::Suite(suite) => suite,
            Body::Value(_) => &[],
        };
        let mut bindings = scope::bindings(suite, parameters, &self.rebound, &self.reraisers());
        bindings.filled = scope::always_filled(suite, &bindings.defs);
        let params: Vec<String> = parameters
            .map(|parameters| {
                parameters
                    .args
                    .iter()
                    .map(|param| param.parameter.name.id.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let declared: Vec<String> = bindings
            .locals
            .iter()
            .filter(|local| !params.contains(local))
            .cloned()
            .collect();
        // A local hides a comprehension variable of the same name around
        // the function.
        let hidden: Vec<(String, Name)> = self
            .renames
            .iter()
            .filter(|(renamed, _)| bindings.locals.contains(renamed))
            .cloned()
            .collect();
        let kept = std::mem::take(&mut self.renames);
        self.renames = kept
            .iter()
            .filter(|rename| !hidden.contains(rename))
            .cloned()
            .collect();
        let mut excluded = self.rebound.clone();
        excluded.extend(params.iter().cloned());
        let range_builtin = self.variable("range").is_none();
        let integer_locals = scope::integer_locals(suite, &bindings, &excluded, range_builtin);
        let sequence_locals =
            scope::sequence_locals(suite, &bindings, &excluded, self.variable("str").is_none());
        self.scopes.push(Scope {
            kind: ScopeKind::Function { is_async },
            bindings,
            integer_locals,
            sequence_locals,
            bound: vec![params.iter().cloned().collect::<BTreeSet<String>>()],
            handlers: active.iter().cloned().map(Active::Passed).collect(),
            loops: 0,
        });
        let span = self.span;
        let block = self.block(|this| {
            for local in &declared {
                this.emit(Stmt::Let {
                    name: Name::new(local.as_str()),
                    value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                });
            }
            // Arguments fill parameters in order, so the last parameter
            // without a default tells whether any is missing.
            let required = params
                .iter()
                .zip(defaults)
                .rfind(|(_, default)| default.is_none());
            let argument_check = if let (Some((param, _)), Some(name)) = (required, checked) {
                let missing = this.omitted(param)?;
                let failure = this.invoke("py.arg_missing", &[Operand::text(name)], Ty::Unknown)?;
                Some(Expr::If {
                    cond: Box::new(missing),
                    then: Box::new(failure.expr),
                    r#else: Box::new(Expr::Literal(Literal::Null)),
                })
            } else {
                None
            };
            let prologue_end = this.buf.stmts.len();
            for (param, default) in params.iter().zip(defaults) {
                let Some(default) = default else {
                    continue;
                };
                let missing = this.omitted(param)?;
                let assign = this.single(Stmt::Assign {
                    place: Place::Variable(Name::new(param.as_str())),
                    value: Rhs::Expr(default.expr.clone()),
                    order: AssignOrder::ValueFirst,
                });
                this.emit_if(missing, assign, Buf::default());
            }
            match body {
                Body::Suite(suite) => this.statements(suite),
                Body::Value(value) => {
                    let value = this.expr(value)?;
                    this.return_value(value);
                    Ok(())
                }
            }?;
            if let Some(check) = argument_check {
                let binding = this.temp();
                if let Some(stmt) = this.buf.stmts.get_mut(prologue_end) {
                    let expr = match stmt {
                        Stmt::Let {
                            value: Rhs::Expr(expr),
                            ..
                        }
                        | Stmt::Assign {
                            value: Rhs::Expr(expr),
                            ..
                        }
                        | Stmt::Return { value: expr }
                        | Stmt::Do { value: expr }
                        | Stmt::Print { value: expr }
                        | Stmt::Throw { value: expr }
                        | Stmt::If {
                            condition: expr, ..
                        }
                        | Stmt::While {
                            condition: expr, ..
                        }
                        | Stmt::For { iterable: expr, .. } => Some(expr),
                        _ => None,
                    };
                    if let Some(expr) = expr {
                        *expr = Expr::Let {
                            name: binding,
                            value: Box::new(check),
                            body: Box::new(expr.clone()),
                        };
                        for (path, _) in &mut this.buf.notes[prologue_end].nested_written {
                            if !path.is_empty() {
                                path.insert(1, 1);
                            }
                        }
                        return Ok(());
                    }
                }
                let Expr::If { cond, then, .. } = check else {
                    unreachable!("a parameter check is conditional")
                };
                this.buf.stmts.insert(
                    prologue_end,
                    Stmt::If {
                        condition: *cond,
                        then_block: vec![Stmt::Do { value: *then }],
                        else_block: Vec::new(),
                    },
                );
                this.buf.notes.insert(
                    prologue_end,
                    Note {
                        span,
                        blocks: vec![
                            vec![Note {
                                span,
                                ..Note::default()
                            }],
                            Vec::new(),
                        ],
                        ..Note::default()
                    },
                );
            }
            Ok(())
        });
        self.span = span;
        self.scopes.pop();
        self.renames = kept;
        let params = params
            .iter()
            .map(|param| Name::new(param.as_str()))
            .chain(active)
            .collect();
        Ok((params, self.finish_declarations(block?, None)))
    }

    /// Whether the call gave no argument for `param`.
    fn omitted(&mut self, param: &str) -> Lowering<Expr> {
        self.native(
            "same",
            vec![
                Expr::Variable(Name::new(param)),
                Expr::Literal(Literal::Absent),
            ],
        )
    }
}

fn unsupported_import(module: &str, range: TextRange) -> lash_kernel_dialect::Diagnostic {
    if module == "re" || module.starts_with("re.") {
        return diagnostics::with_repair(
            Code::RegexUnsupported,
            "regular expressions are not in the dialect: it ships no `re` extension",
            range,
            "use `str` methods (`find`, `split`, `startswith`, `replace`)",
        );
    }
    diagnostics::with_repair(
        Code::ImportUnsupported,
        format!("`{module}` cannot be imported: the dialect has `import asyncio` and nothing else"),
        range,
        "use the built-in functions and the tools the host provides; write `asyncio.name`, not `from asyncio import name`",
    )
}

fn class_refusal(range: TextRange, name: &str) -> lash_kernel_dialect::Diagnostic {
    diagnostics::with_repair(
        Code::ClassUnsupported,
        "classes are not in the dialect, except an exception class with an empty body",
        range,
        format!(
            "keep `{name}` state in dicts and behaviour in functions; `class {name}(Exception): pass` at the top level declares an exception"
        ),
    )
}
