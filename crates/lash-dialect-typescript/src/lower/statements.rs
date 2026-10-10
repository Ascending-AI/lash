//! Statements and control flow.

use lash_kernel_doc::{Expr, Literal, Member, Name, Place, Stmt};

use super::patterns::Mode;
use super::{BindingKind, Buf, Control, Lowerer, Lowering, Note, Operand, Ty, walk};
use crate::adapter::{self as ast, VarKind};
use crate::{Diagnostic, DiagnosticCode};

fn kind_of(kind: VarKind) -> BindingKind {
    match kind {
        VarKind::Var => BindingKind::Var,
        VarKind::Let => BindingKind::Let,
        VarKind::Const => BindingKind::Const,
    }
}

/// How a `for...of` or `for...in` head binds what each pass gives.
fn mode_of(kind: Option<VarKind>) -> Mode {
    match kind {
        None => Mode::Assign,
        Some(VarKind::Var) => Mode::Declared,
        Some(_) => Mode::Local,
    }
}

fn truth() -> Expr {
    Expr::Literal(Literal::Bool(true))
}

fn one(stmt: Stmt) -> Buf {
    Buf {
        stmts: vec![stmt],
        notes: vec![Note::default()],
    }
}

impl Lowerer<'_> {
    /// Declares what a block's own statements declare lexically: `let`,
    /// `const`, functions and enums. The bindings exist for the whole
    /// block and are not live until their declarations run.
    pub(super) fn declare_block(&mut self, statements: &[ast::Stmt]) -> Lowering<()> {
        for statement in statements {
            match statement.unlabeled() {
                ast::Stmt::Var { kind, declarations } if *kind != VarKind::Var => {
                    let mut names = Vec::new();
                    for declaration in declarations {
                        walk::pattern_names(&declaration.pattern, &mut names);
                    }
                    for name in names {
                        self.declare(&name, kind_of(*kind));
                    }
                }
                ast::Stmt::Function { name, .. } => {
                    self.declare(name, BindingKind::Function);
                }
                ast::Stmt::Enum { name, .. } => {
                    self.declare(name, BindingKind::Let);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Declares every `var` of a function's body (or of `main`) in the
    /// function's scope.
    pub(super) fn declare_vars(&mut self, statements: &[ast::Stmt]) {
        let mut names = Vec::new();
        walk::var_names(statements, &mut names);
        for name in names {
            self.declare(&name, BindingKind::Var);
        }
    }

    /// Lowers a block's statements. Function declarations are hoisted: each
    /// exists before any statement of the block that can run code.
    ///
    /// The declarations that open the block with constants run first. No
    /// function can be called before they have, so a function that reads
    /// one needs no check that it is initialised.
    pub(super) fn lower_statements(&mut self, statements: &[ast::Stmt]) -> Lowering<()> {
        let constants = statements
            .iter()
            .take_while(|statement| walk::declares_constants(statement))
            .count();
        let (constants, rest) = statements.split_at(constants);
        for statement in constants {
            self.lower_statement(statement)?;
        }
        for statement in statements {
            if let ast::Stmt::Function { name, function } = statement.unlabeled() {
                let closure = self.lower_function(function)?;
                self.initialise(name, closure);
            }
        }
        // What an `if` that always leaves showed false holds of every
        // statement after it in the block.
        let outer = self.narrowed.len();
        let result = rest.iter().try_for_each(|statement| {
            self.lower_statement(statement)?;
            if let ast::Stmt::If {
                test,
                consequent,
                alternate: None,
            } = statement.unlabeled()
                && walk::always_leaves(consequent)
            {
                let shown = self.narrowing(test).when_false;
                self.narrowed.extend(shown);
            }
            Ok(())
        });
        self.narrowed.truncate(outer);
        result
    }

    /// A statement as a block of its own, with its own scope.
    fn body(&mut self, statement: &ast::Stmt) -> Lowering<Buf> {
        self.scoped_block(|this| this.lower_statement(statement))
    }

    /// `if condition {} else { break }`.
    fn exit_unless(&mut self, condition: Expr) {
        if condition == truth() {
            return;
        }
        self.emit_if(condition, Buf::default(), one(Stmt::Break));
    }

    pub(super) fn lower_statement(&mut self, statement: &ast::Stmt) -> Lowering<()> {
        match statement {
            ast::Stmt::Spanned(span, inner) => {
                let outer = self.span.replace(*span);
                let result = self.lower_statement(inner);
                self.span = outer;
                result
            }
            ast::Stmt::Labeled { label, stmt } => {
                let emitted = self.buf.notes.len();
                self.lower_statement(stmt)?;
                // The label names the statement that does the work, which
                // is the last one the source statement lowered to.
                if self.buf.notes.len() > emitted
                    && let Some(note) = self.buf.notes.last_mut()
                {
                    note.label = Some(label.clone());
                }
                Ok(())
            }
            ast::Stmt::Empty | ast::Stmt::TypeAlias { .. } | ast::Stmt::Function { .. } => Ok(()),
            ast::Stmt::Expr(expr) => {
                if self.discards_effect(expr) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnawaitedTool,
                        "a discarded tool call's promise can never be awaited",
                        self.span,
                    ));
                }
                let value = self.lower_expr(expr)?;
                self.discard(value);
                Ok(())
            }
            ast::Stmt::Block(statements) => {
                self.push_scope();
                let result = self
                    .declare_block(statements)
                    .and_then(|()| self.lower_statements(statements));
                self.pop_scope();
                result
            }
            ast::Stmt::Var { kind, declarations } => {
                for declaration in declarations {
                    self.lower_declaration(*kind, declaration)?;
                }
                Ok(())
            }
            ast::Stmt::Enum { name, members } => self.lower_enum(name, members),
            ast::Stmt::Return(value) => {
                if self.in_main() {
                    return Err(Diagnostic::new(
                        DiagnosticCode::ReturnOutsideFunction,
                        "`return` is used outside a function",
                        self.span,
                    ));
                }
                let value = match value {
                    Some(value) => self.lower_expr(value)?,
                    None => Operand::undefined(),
                };
                self.emit(Stmt::Return {
                    value: value.expr(),
                });
                Ok(())
            }
            ast::Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                let shown = self.narrowing(test);
                let test = self.lower_expr(test)?;
                let condition = self.condition(test)?;
                let then_block = self.narrowed(shown.when_true, |this| this.body(consequent))?;
                let else_block = match alternate {
                    Some(alternate) => {
                        self.narrowed(shown.when_false, |this| this.body(alternate))?
                    }
                    None => Buf::default(),
                };
                self.emit_if(condition, then_block, else_block);
                Ok(())
            }
            ast::Stmt::While { test, body } => {
                let body = self.loop_block(Vec::new(), |this| {
                    let test = this.lower_expr(test)?;
                    let condition = this.condition(test)?;
                    this.exit_unless(condition);
                    this.lower_statement(body)
                })?;
                self.emit_while(truth(), body);
                Ok(())
            }
            ast::Stmt::DoWhile { body, test, .. } => {
                let first = self.let_expr(truth(), Ty::Bool);
                let body = self.loop_block(Vec::new(), |this| {
                    this.unless_first(&first, |this| {
                        let test = this.lower_expr(test)?;
                        let condition = this.condition(test)?;
                        this.exit_unless(condition);
                        Ok(())
                    })?;
                    this.lower_statement(body)
                })?;
                self.emit_while(truth(), body);
                Ok(())
            }
            ast::Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.push_scope();
                let result = self.lower_for(
                    init.as_deref(),
                    test.as_ref(),
                    update.as_ref(),
                    body,
                    statement,
                );
                self.pop_scope();
                result
            }
            ast::Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => self.lower_for_each("ts.iterate", pattern, *kind, iterable, body),
            ast::Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => self.lower_for_each("ts.keys", pattern, *kind, object, body),
            ast::Stmt::Switch {
                discriminant,
                cases,
            } => self.lower_switch(discriminant, cases),
            ast::Stmt::Break => {
                if self.frame().controls.is_empty() {
                    return Err(self.outside_loop("break"));
                }
                self.emit(Stmt::Break);
                Ok(())
            }
            ast::Stmt::Continue => {
                let depth = self.frame().controls.len();
                self.lower_continue(depth)
            }
            ast::Stmt::Throw(value) => {
                let value = self.lower_expr(value)?;
                self.emit(Stmt::Throw {
                    value: value.expr(),
                });
                Ok(())
            }
            ast::Stmt::Try {
                body,
                catch,
                finally,
            } => self.lower_try(body, catch.as_ref(), finally.as_deref()),
        }
    }

    fn outside_loop(&self, keyword: &str) -> Diagnostic {
        Diagnostic::new(
            DiagnosticCode::LoopControlOutsideLoop,
            format!("`{keyword}` is used outside a loop"),
            self.span,
        )
    }

    fn lower_declaration(&mut self, kind: VarKind, declaration: &ast::Var) -> Lowering<()> {
        let value = match &declaration.init {
            // A `const` of the cell bound to an `async` arrow names a process.
            Some(init)
                if kind == VarKind::Const
                    && self.in_cell_code()
                    && matches!(declaration.pattern, ast::Pattern::Ident(..))
                    && self.process_arrow(init).is_some() =>
            {
                let (ast::Pattern::Ident(name, _), Some(function)) =
                    (&declaration.pattern, self.process_arrow(init))
                else {
                    unreachable!("the guard matched a named process arrow");
                };
                self.lower_process(Some(name), function)?
            }
            Some(init) => match &declaration.pattern {
                ast::Pattern::Ident(name, _) => self.named_expression(init, name)?,
                _ => self.lower_expr(init)?,
            },
            None if kind == VarKind::Var => {
                // `var x;` makes `x` exist and assigns nothing.
                let mut names = Vec::new();
                walk::pattern_names(&declaration.pattern, &mut names);
                for name in names {
                    self.resolve(&name, self.span)?;
                }
                return Ok(());
            }
            None => Operand::undefined(),
        };
        self.destructure(&declaration.pattern, value, Mode::Declared)
    }

    fn lower_enum(&mut self, name: &str, members: &[ast::EnumMember]) -> Lowering<()> {
        let object = self.let_expr(Expr::Record(Vec::new()), Ty::Unknown);
        self.initialise(name, object);
        let Some(object) = self.resolve(name, self.span)? else {
            unreachable!("the enum was just declared");
        };
        let object = Operand::variable(object, Ty::Unknown);
        for member in members {
            let value = self.lower_expr(&member.value)?;
            let value = self.pin(value);
            self.store(
                Place::Member(Member::Field {
                    target: object.expr(),
                    field: member.name.clone(),
                }),
                value.clone(),
            );
            if member.reverse {
                let written = self.invoke(
                    "ts.set",
                    &[object.clone(), value, Operand::text(member.name.clone())],
                    Ty::Unknown,
                )?;
                self.discard(written);
            }
        }
        Ok(())
    }

    /// A loop's body: a block and scope of its own, inside which `break`
    /// and `continue` mean the kernel's.
    fn loop_block(
        &mut self,
        before_continue: Vec<Stmt>,
        lower: impl FnOnce(&mut Self) -> Lowering<()>,
    ) -> Lowering<Buf> {
        self.frame_mut()
            .controls
            .push(Control::Loop { before_continue });
        let body = self.scoped_block(lower);
        self.frame_mut().controls.pop();
        body
    }

    /// Runs `lower` on every pass of a loop but the first.
    fn unless_first(
        &mut self,
        first: &Operand,
        lower: impl FnOnce(&mut Self) -> Lowering<()>,
    ) -> Lowering<()> {
        let lower_flag = one(Stmt::Assign {
            place: Place::Variable(variable_of(first)),
            value: lash_kernel_doc::Rhs::Expr(Expr::Literal(Literal::Bool(false))),
        });
        let later = self.block(lower)?;
        self.emit_if(first.expr(), lower_flag, later);
        Ok(())
    }

    /// `for (init; test; update) body`. Each pass of a loop whose head
    /// declares with `let` and whose body makes a function has its own
    /// copy of the head's bindings, as the language gives it.
    fn lower_for(
        &mut self,
        init: Option<&ast::Stmt>,
        test: Option<&ast::Expr>,
        update: Option<&ast::Expr>,
        body: &ast::Stmt,
        whole: &ast::Stmt,
    ) -> Lowering<()> {
        let mut per_pass = Vec::new();
        if let Some(init) = init {
            if let ast::Stmt::Var { kind, declarations } = init.unlabeled()
                && *kind != VarKind::Var
            {
                let mut names = Vec::new();
                for declaration in declarations {
                    walk::pattern_names(&declaration.pattern, &mut names);
                }
                for name in &names {
                    self.declare(name, kind_of(*kind));
                }
                if *kind == VarKind::Let && walk::statement_makes_function(whole) {
                    per_pass = names;
                }
            }
            self.lower_statement(init)?;
        }
        let first = update.map(|_| self.let_expr(truth(), Ty::Bool));
        // The head's variable and the pass's copy of it, by source name.
        let mut copies: Vec<(String, Name, Name)> = Vec::new();
        let outer_names: Vec<(String, Name)> = per_pass
            .iter()
            .filter_map(|name| {
                let kernel = self.binding_mut(name)?.1.kernel.clone();
                Some((name.clone(), kernel))
            })
            .collect();
        let copy_back = |copies: &[(String, Name, Name)]| -> Vec<Stmt> {
            copies
                .iter()
                .map(|(_, outer, inner)| Stmt::Assign {
                    place: Place::Variable(outer.clone()),
                    value: lash_kernel_doc::Rhs::Expr(Expr::Variable(inner.clone())),
                })
                .collect()
        };
        self.frame_mut().controls.push(Control::Loop {
            before_continue: Vec::new(),
        });
        let body = self.scoped_block(|this| {
            for (name, outer) in outer_names {
                let inner = this.declare(&name, BindingKind::Local);
                this.emit(Stmt::Let {
                    name: inner.clone(),
                    value: lash_kernel_doc::Rhs::Expr(Expr::Variable(outer.clone())),
                });
                copies.push((name, outer, inner));
            }
            if let Some(Control::Loop { before_continue }) = this.frame_mut().controls.last_mut() {
                *before_continue = copy_back(&copies);
            }
            if let (Some(first), Some(update)) = (&first, update) {
                this.unless_first(first, |this| {
                    let value = this.lower_expr(update)?;
                    this.discard(value);
                    Ok(())
                })?;
            }
            if let Some(test) = test {
                let test = this.lower_expr(test)?;
                let condition = this.condition(test)?;
                this.exit_unless(condition);
            }
            this.lower_statement(body)?;
            for stmt in copy_back(&copies) {
                this.emit(stmt);
            }
            Ok(())
        });
        self.frame_mut().controls.pop();
        self.emit_while(truth(), body?);
        Ok(())
    }

    /// The element a loop over a list binds, as JavaScript reads it: a hole
    /// is `undefined`.
    pub(super) fn element_value(&mut self, raw: &Operand) -> Lowering<Operand> {
        let element = self.temp();
        self.bind(element.clone(), raw.clone());
        self.absent_if_hole(&element, raw)?;
        Ok(Operand::variable(element, raw.ty.clone()))
    }

    /// `if same(raw, ()) { set element = absent }`: the empty tuple is a
    /// hole, which no JavaScript value is.
    pub(super) fn absent_if_hole(&mut self, element: &Name, raw: &Operand) -> Lowering<()> {
        let hole = self.same(raw.expr(), Expr::Tuple(Vec::new()))?;
        let absent = one(Stmt::Assign {
            place: Place::Variable(element.clone()),
            value: lash_kernel_doc::Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
        self.emit_if(hole, absent, Buf::default());
        Ok(())
    }

    /// `for (pattern of iterable)` and `for (pattern in object)`: a kernel
    /// loop over the list `helper` gives.
    fn lower_for_each(
        &mut self,
        helper: &str,
        pattern: &ast::Pattern,
        kind: Option<VarKind>,
        subject: &ast::Expr,
        body: &ast::Stmt,
    ) -> Lowering<()> {
        let subject = self.lower_expr(subject)?;
        let subject = self.pin(subject);
        if helper == "ts.iterate" {
            if let Ty::List(element) = &subject.ty {
                // `TS_TYPED_ARRAY_ITERATION`: a declared array is the
                // kernel's live loop over a list (`K-ITER-002`), which reads
                // length and element anew on each pass as an array iterator
                // does. A value of another kind raises `type_error` here.
                let checked = self.native("list.check", vec![subject.expr()])?;
                let checked = self.let_expr(checked, Ty::Unknown);
                self.discard(checked);
                let item = self.temp();
                let raw = Operand::variable(item.clone(), (**element).clone());
                let body = self.loop_block(Vec::new(), |this| {
                    let value = this.element_value(&raw)?;
                    this.destructure(pattern, value, mode_of(kind))?;
                    this.lower_statement(body)
                })?;
                self.emit_for(item, subject.expr(), body);
                return Ok(());
            }
            // Kernel collection loops retain insertion-sequence cursors. A
            // snapshot list would lose additions and visit deleted entries.
            // A list is the same live loop, as an array iterator reads it.
            let receiver = self.invoke("ts.receiver", std::slice::from_ref(&subject), Ty::Text)?;
            let list = self.same(receiver.expr(), Operand::text("list").expr())?;
            let map = self.same(receiver.expr(), Operand::text("map").expr())?;
            let set = self.same(receiver.expr(), Operand::text("set").expr())?;
            let list = self.let_expr(list, Ty::Bool);
            let collection = self.short_circuit(ast::LogicalOp::Or, list, |this| {
                let map = this.let_expr(map, Ty::Bool);
                this.short_circuit(ast::LogicalOp::Or, map, |this| {
                    Ok(this.let_expr(set, Ty::Bool))
                })
            })?;
            let live = self.block(|this| {
                let key = this.temp();
                let raw = Operand::variable(key.clone(), Ty::Unknown);
                let body = this.loop_block(Vec::new(), |this| {
                    let list = this.same(receiver.expr(), Operand::text("list").expr())?;
                    let element = this.temp();
                    let value = Operand::variable(element.clone(), Ty::Unknown);
                    this.bind(element.clone(), raw.clone());
                    let hole = this.block(|this| this.absent_if_hole(&element, &raw))?;
                    let keyed = this.block(|this| {
                        let key_value =
                            this.invoke("ts.map.value", std::slice::from_ref(&raw), Ty::Unknown)?;
                        this.store(Place::Variable(element.clone()), key_value);
                        let entry = this.block(|this| {
                            let read = Expr::Member(Box::new(Member::Index {
                                target: subject.expr(),
                                index: raw.expr(),
                            }));
                            let read = this.let_expr(read, Ty::Unknown);
                            let pair = this.let_expr(
                                Expr::List(vec![Expr::Variable(element.clone()), read.expr()]),
                                Ty::Unknown,
                            );
                            this.store(Place::Variable(element.clone()), pair);
                            Ok(())
                        })?;
                        let map = this.same(receiver.expr(), Operand::text("map").expr())?;
                        this.emit_if(map, entry, Buf::default());
                        Ok(())
                    })?;
                    this.emit_if(list, hole, keyed);
                    this.destructure(pattern, value, mode_of(kind))?;
                    this.lower_statement(body)
                })?;
                this.emit_for(key, subject.expr(), body);
                Ok(())
            })?;
            let array =
                self.block(|this| this.lower_for_of_iterator(pattern, kind, subject, body))?;
            self.emit_if(collection.expr(), live, array);
            return Ok(());
        }
        let items = self.invoke(helper, std::slice::from_ref(&subject), Ty::Unknown)?;
        let item = self.temp();
        let element = Operand::variable(item.clone(), Ty::Unknown);
        let body = self.loop_block(Vec::new(), |this| {
            let present = this.invoke("ts.has", &[subject, element.clone()], Ty::Bool)?;
            let visits = this.block(|this| {
                this.destructure(pattern, element, mode_of(kind))?;
                this.lower_statement(body)
            })?;
            this.emit_if(present.expr(), visits, Buf::default());
            Ok(())
        })?;
        self.emit_for(item, items.expr(), body);
        Ok(())
    }

    fn lower_for_of_iterator(
        &mut self,
        pattern: &ast::Pattern,
        kind: Option<VarKind>,
        subject: Operand,
        body: &ast::Stmt,
    ) -> Lowering<()> {
        let iterator = self.invoke("ts.array.iterator", &[subject], Ty::Unknown)?;
        let body = self.loop_block(Vec::new(), |this| {
            let empty = this.let_expr(Expr::List(Vec::new()), Ty::Unknown);
            let step = this.invoke(
                "ts.call_member",
                &[iterator.clone(), Operand::text("next"), empty],
                Ty::Unknown,
            )?;
            let done = this.invoke(
                "ts.get",
                &[step.clone(), Operand::text("done")],
                Ty::Unknown,
            )?;
            let onward = this.invoke("ts.not", &[done], Ty::Bool)?;
            this.exit_unless(onward.expr());
            let element = this.invoke("ts.get", &[step, Operand::text("value")], Ty::Unknown)?;
            this.destructure(pattern, element, mode_of(kind))?;
            this.lower_statement(body)
        })?;
        self.emit_while(truth(), body);
        Ok(())
    }

    /// `switch`: the clause to start at is chosen first, by testing each
    /// `case` in order until one matches; then the clauses run from there,
    /// inside a loop of one pass so that `break` leaves the switch.
    fn lower_switch(
        &mut self,
        discriminant: &ast::Expr,
        cases: &[ast::SwitchCase],
    ) -> Lowering<()> {
        let discriminant = self.lower_expr(discriminant)?;
        let discriminant = self.pin(discriminant);
        let unchosen = Operand::number(-1.0);
        let chosen = self.let_expr(unchosen.expr(), Ty::Float);
        let chosen_name = variable_of(&chosen);
        let choose = |index: usize| Stmt::Assign {
            place: Place::Variable(chosen_name.clone()),
            value: lash_kernel_doc::Rhs::Expr(position(index)),
        };
        for (index, case) in cases.iter().enumerate() {
            let Some(test) = &case.test else { continue };
            let undecided = self.same(chosen.expr(), unchosen.expr())?;
            let attempt = self.block(|this| {
                let test = this.lower_expr(test)?;
                let equal =
                    this.invoke("ts.strict_equals", &[discriminant.clone(), test], Ty::Bool)?;
                this.emit_if(equal.expr(), one(choose(index)), Buf::default());
                Ok(())
            })?;
            self.emit_if(undecided, attempt, Buf::default());
        }
        if let Some(default) = cases.iter().position(|case| case.test.is_none()) {
            let undecided = self.same(chosen.expr(), unchosen.expr())?;
            self.emit_if(undecided, one(choose(default)), Buf::default());
        }
        let running = self.let_expr(Expr::Literal(Literal::Bool(false)), Ty::Bool);
        let running_name = variable_of(&running);
        self.frame_mut().controls.push(Control::Switch {
            continue_flag: None,
        });
        let body = self.scoped_block(|this| {
            for case in cases {
                this.declare_block(&case.consequent)?;
            }
            for (index, case) in cases.iter().enumerate() {
                let here = this.same(chosen.expr(), position(index))?;
                this.emit_if(
                    here,
                    one(Stmt::Assign {
                        place: Place::Variable(running_name.clone()),
                        value: lash_kernel_doc::Rhs::Expr(truth()),
                    }),
                    Buf::default(),
                );
                let clause = this.block(|this| this.lower_statements(&case.consequent))?;
                this.emit_if(running.expr(), clause, Buf::default());
            }
            this.emit(Stmt::Break);
            Ok(())
        });
        let depth = self.frame().controls.len() - 1;
        let Some(Control::Switch { continue_flag }) = self.frame_mut().controls.pop() else {
            unreachable!("the switch pushed its own control");
        };
        let body = body?;
        match continue_flag {
            None => self.emit_while(truth(), body),
            Some(flag) => {
                self.emit(Stmt::Let {
                    name: flag.clone(),
                    value: lash_kernel_doc::Rhs::Expr(Expr::Literal(Literal::Bool(false))),
                });
                self.emit_while(truth(), body);
                let onward = self.block(|this| this.lower_continue(depth))?;
                self.emit_if(Expr::Variable(flag), onward, Buf::default());
            }
        }
        Ok(())
    }

    /// `continue`, seen from inside the innermost `depth` controls. A
    /// switch between here and the loop is left first, with a flag that
    /// makes the code after it continue.
    fn lower_continue(&mut self, depth: usize) -> Lowering<()> {
        let Some(index) = depth.checked_sub(1) else {
            return Err(self.outside_loop("continue"));
        };
        if !self.frame().controls[..depth]
            .iter()
            .any(|control| matches!(control, Control::Loop { .. }))
        {
            return Err(self.outside_loop("continue"));
        }
        let before = match &self.frame().controls[index] {
            Control::Loop { before_continue } => Some(before_continue.clone()),
            Control::Switch { .. } => None,
        };
        match before {
            Some(before) => {
                for stmt in before {
                    self.emit(stmt);
                }
                self.emit(Stmt::Continue);
            }
            None => {
                let existing = match &self.frame().controls[index] {
                    Control::Switch { continue_flag } => continue_flag.clone(),
                    Control::Loop { .. } => None,
                };
                let flag = match existing {
                    Some(flag) => flag,
                    None => {
                        let flag = self.temp();
                        if let Control::Switch { continue_flag } =
                            &mut self.frame_mut().controls[index]
                        {
                            *continue_flag = Some(flag.clone());
                        }
                        flag
                    }
                };
                self.emit(Stmt::Assign {
                    place: Place::Variable(flag),
                    value: lash_kernel_doc::Rhs::Expr(truth()),
                });
                self.emit(Stmt::Break);
            }
        }
        Ok(())
    }

    fn lower_try(
        &mut self,
        body: &[ast::Stmt],
        catch: Option<&ast::Catch>,
        finally: Option<&[ast::Stmt]>,
    ) -> Lowering<()> {
        let statements = |this: &mut Self, statements: &[ast::Stmt]| {
            this.declare_block(statements)?;
            this.lower_statements(statements)
        };
        let body = self.scoped_block(|this| statements(this, body))?;
        let catch = match catch {
            None => None,
            Some(catch) => {
                let mut binding = None;
                let handler = self.scoped_block(|this| {
                    match &catch.binding {
                        Some(ast::Pattern::Ident(name, _)) => {
                            binding = Some(this.declare(name, BindingKind::Local));
                        }
                        Some(pattern) => {
                            let caught = this.temp();
                            binding = Some(caught.clone());
                            this.destructure(
                                pattern,
                                Operand::variable(caught, Ty::Unknown),
                                Mode::Local,
                            )?;
                        }
                        None => binding = Some(this.temp()),
                    }
                    statements(this, &catch.body)
                })?;
                let Some(binding) = binding else {
                    unreachable!("the handler names its binding");
                };
                Some((binding, handler))
            }
        };
        let finally = match finally {
            Some(finally) => Some(self.scoped_block(|this| statements(this, finally))?),
            None => None,
        };
        self.emit_try(body, catch, finally);
        Ok(())
    }
}

pub(super) fn variable_of(operand: &Operand) -> Name {
    match &operand.atom {
        lash_kernel_doc::Atom::Variable(name) => name.clone(),
        lash_kernel_doc::Atom::Literal(_) => unreachable!("the operand was bound to a temporary"),
    }
}

/// A clause's position as the number the switch compares.
fn position(index: usize) -> Expr {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a clause's position in its switch"
    )]
    let index = index as f64;
    Operand::number(index).expr()
}
