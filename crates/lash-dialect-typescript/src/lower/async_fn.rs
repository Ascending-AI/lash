//! Asynchronous source as kernel tasks.
//!
//! A promise is a record over the task that settles it (`helpers/
//! promise.kernel`). The lowerer writes four shapes:
//!
//! - **An `async` function** is a closure that starts a promise whose task
//!   runs the function's body. The body runs at once, up to its first wait
//!   (`K-TASK-002`), as JavaScript runs it up to its first `await`.
//! - **`await value`** is `ts.await(value)`: a `join` on a promise's task,
//!   after one `yield` when the promise has already settled, and one
//!   `yield` alone for a value that is no promise.
//! - **`await tool(args)`** is one `perform` in the awaiting task, and
//!   `await sleep(ms)` one `sleep`; each is followed by one `yield`.
//! - **`tool(args)` and `sleep(ms)` not awaited where they stand** are
//!   promises whose task performs the effect or sleeps.
//!
//! The `yield` after an awaited effect keeps one order between the two ways
//! of calling a tool. An outcome delivered to a promise's task wakes that
//! task, which ends and only then wakes the tasks that await the promise:
//! two turns of the queue. A `perform` in the awaiting task would take one,
//! so of two outcomes delivered together the one awaited in place would
//! always run first, which JavaScript does not do. With the `yield` every
//! delivery takes two turns and the order of continuations is the order of
//! arrival.
//!
//! The spawn is written in the document, not inside a helper, so a task's
//! identity (`K-TASK-020`) names the async function or the tool call that
//! started it.

use lash_kernel_doc::{Action, Atom, Callee, EffectName, Expr, Literal, Member, Place, Rhs, Stmt};

use super::{Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// A call the kernel runs as a wait of its own.
pub(super) enum Wait {
    /// A call of an effect the host supplies.
    Effect(EffectName),
    /// `sleep(ms)`.
    Sleep,
}

impl Lowerer<'_> {
    /// An `async` function or arrow. Gives the operand of its value, the
    /// token of a closure by the dialect's calling convention: called, it
    /// starts the body as a task and returns that task's promise.
    pub(super) fn lower_async_function(
        &mut self,
        function: &ast::Function,
        _span: Option<SourceSpan>,
    ) -> Lowering<Operand> {
        let body = self.closure(function)?;
        // The function the source wrote is the one that starts the task,
        // not the closure of its body.
        if let Some(note) = self.buf.notes.last_mut() {
            note.written = None;
        }
        let written = self.in_cell_code().then(|| self.written(function));
        let this = self.fresh("this");
        let args = self.fresh("args");
        let params = vec![this.clone(), args.clone()];
        let block = self.block(|lowerer| {
            let promise =
                lowerer.start_promise(&body, Atom::Variable(this), Atom::Variable(args))?;
            lowerer.emit(Stmt::Return {
                value: promise.expr(),
            });
            Ok(())
        })?;
        self.written = written;
        let closure = self.emit_closure(params, block);
        Ok(self.function_token(function, closure))
    }

    /// `await value`. The statements of everything the source evaluated
    /// before it are already emitted.
    pub(super) fn lower_await(&mut self, value: &ast::Expr, span: SourceSpan) -> Lowering<Operand> {
        if let ast::Expr::Call { callee, args, .. } = value
            && let Some(wait) = self.wait_of(callee)
        {
            let control = self.control_call(&wait);
            if control && !self.in_main() {
                return Err(self.control_placement(&wait, span));
            }
            let args = self.wait_arguments(&wait, args, span)?;
            let result = self.wait_in_place(wait, args)?;
            if control {
                // The call settled, so the turn is over: nothing after it
                // runs (`K-FORM-019`). The host reads the control from the
                // settled call, never from this value.
                self.emit(Stmt::Finish {
                    value: Expr::Literal(Literal::Null),
                });
            }
            return Ok(result);
        }
        let value = self.lower_expr(value)?;
        let awaited = self.invoke("ts.await", &[value], Ty::Unknown)?;
        self.await_process(awaited, span)
    }

    /// A tool call or a `sleep` that is not awaited where it stands: a
    /// promise whose task does the waiting.
    pub(super) fn lower_wait_call(
        &mut self,
        wait: Wait,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<Operand> {
        if self.control_call(&wait) {
            return Err(self.control_placement(&wait, span));
        }
        let args = self.wait_arguments(&wait, args, span)?;
        let this = self.fresh("this");
        let ignored = self.fresh("args");
        let block = self.block(|lowerer| {
            let result = lowerer.wait_action(wait, args)?;
            lowerer.emit(Stmt::Return {
                value: result.expr(),
            });
            Ok(())
        })?;
        let body = self.emit_closure(vec![this, ignored], block);
        self.start_promise(
            &body,
            Atom::Literal(Literal::Absent),
            Atom::Literal(Literal::Absent),
        )
    }

    /// Whether the wait is a call of an effect that ends the turn.
    fn control_call(&self, wait: &Wait) -> bool {
        matches!(wait, Wait::Effect(effect) if self.controls.contains_key(effect))
    }

    /// The refusal of a control call written where the turn cannot end on
    /// it: not awaited where it stands, or inside a function. A promise or
    /// a function's body runs as a task beside `main`, and only `main` ends
    /// the turn.
    fn control_placement(&self, wait: &Wait, span: SourceSpan) -> Diagnostic {
        let name = match wait {
            Wait::Effect(effect) => effect.to_string(),
            Wait::Sleep => "sleep".to_string(),
        };
        Diagnostic::with_repair(
            DiagnosticCode::ControlCallPlacement,
            format!(
                "`{name}` ends the turn, so it is awaited directly at the top level of the cell: not passed to `Promise.all`, kept as a promise, or called inside a function"
            ),
            format!(
                "await everything else first, then write `await {name}(...)` as the last top-level statement; a function returns its value for the top level to pass on"
            ),
            Some(span),
        )
    }

    /// Which wait a call of `callee` is, if the source binds none of the
    /// names it is written with.
    pub(super) fn wait_of(&self, callee: &ast::Expr) -> Option<Wait> {
        let path = self.unbound_path(callee)?;
        if path == "sleep" {
            return Some(Wait::Sleep);
        }
        let effect = EffectName::new(path.as_str()).ok()?;
        self.effects
            .contains_key(&effect)
            .then_some(Wait::Effect(effect))
    }

    /// Whether a statement is a tool call whose promise nothing can read:
    /// `tool(x);` or `void tool(x);`.
    pub(super) fn discards_effect(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Call { callee, .. } => matches!(self.wait_of(callee), Some(Wait::Effect(_))),
            ast::Expr::Unary {
                op: ast::UnaryOp::Void,
                value,
            } => self.discards_effect(value),
            _ => false,
        }
    }

    /// `a.b.c` as a dotted path, when the source does not bind `a`.
    fn unbound_path(&self, expr: &ast::Expr) -> Option<String> {
        match expr {
            ast::Expr::Ident(name, _) => (!self.is_bound(name)).then(|| name.clone()),
            ast::Expr::Member {
                object,
                property: ast::MemberProperty::Field(field),
                ..
            } => Some(format!("{}.{field}", self.unbound_path(object)?)),
            _ => None,
        }
    }

    /// The arguments of a wait, evaluated left to right, as the atoms its
    /// action takes.
    fn wait_arguments(
        &mut self,
        wait: &Wait,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<Vec<Atom>> {
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                ast::CallArg::Value(value) => values.push(value),
                ast::CallArg::Spread(_) => {
                    return Err(Diagnostic::refusal(
                        DiagnosticCode::UnsupportedExpression,
                        "Unsupported: a spread in a tool call's arguments. Pass each argument.",
                        Some(span),
                    ));
                }
            }
        }
        let (name, least, most) = match wait {
            Wait::Sleep => ("sleep".to_string(), 1, 1),
            Wait::Effect(effect) => {
                let params = &self.effects[effect].params;
                let required = params.iter().filter(|param| !param.optional).count();
                (effect.to_string(), required, params.len())
            }
        };
        if values.len() < least || values.len() > most {
            let takes = if least == most {
                format!("{most}")
            } else {
                format!("{least} to {most}")
            };
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!(
                    "`{name}` takes {takes} argument(s) and is called with {}",
                    values.len()
                ),
                Some(span),
            ));
        }
        let operands = self.operands(&values)?;
        Ok(operands.into_iter().map(|operand| operand.atom).collect())
    }

    /// The wait as one statement, giving its result.
    fn wait_action(&mut self, wait: Wait, args: Vec<Atom>) -> Lowering<Operand> {
        match wait {
            Wait::Effect(effect) => {
                let signature = self.effects[&effect].clone();
                // The perform states the result type the tool declares, so
                // an `Int` or a `Float` is decoded as declared, and what
                // it gives is known to be that type.
                let result = signature.result.clone();
                let ty = Ty::decoded(&result);
                self.performed.insert(effect.clone(), signature);
                let mut copied = Vec::with_capacity(args.len());
                for atom in args {
                    let value = Operand {
                        atom,
                        ty: Ty::Unknown,
                    };
                    let value = self.invoke("ts.boundary", &[value], Ty::Unknown)?;
                    copied.push(value.atom);
                }
                Ok(self.emit_action(
                    Action::Perform {
                        effect,
                        args: copied,
                        result,
                    },
                    ty,
                ))
            }
            Wait::Sleep => {
                let Some(duration) = args.into_iter().next() else {
                    unreachable!("`sleep` is checked to take one argument");
                };
                self.emit(Stmt::Do {
                    action: Action::Sleep { duration },
                });
                Ok(Operand::undefined())
            }
        }
    }

    /// The wait in the awaiting task, and then the one `yield` that keeps
    /// its continuation in arrival order with promises' (see the module).
    /// The `yield` runs when the effect fails too.
    fn wait_in_place(&mut self, wait: Wait, args: Vec<Atom>) -> Lowering<Operand> {
        let pause = Stmt::Do {
            action: Action::Yield,
        };
        if matches!(wait, Wait::Sleep) {
            let result = self.wait_action(wait, args)?;
            self.emit(pause);
            return Ok(result);
        }
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let name = super::statements::variable_of(&result);
        let place = Place::Variable(name.clone());
        // Code after the `try` runs only when the effect gave its result.
        let mut ty = Ty::Unknown;
        let body = self.block(|lowerer| {
            let outcome = lowerer.wait_action(wait, args)?;
            ty = outcome.ty.clone();
            lowerer.store(place, outcome);
            Ok(())
        })?;
        let finally = self.block(|lowerer| {
            lowerer.emit(pause);
            Ok(())
        })?;
        self.emit_try(body, None, Some(finally));
        Ok(Operand::variable(name, ty))
    }

    /// Starts `body(this, args)` as a task and gives its promise:
    ///
    /// ```text
    /// let p = invoke ts.promise.pending()
    /// set p.task = spawn invoke ts.promise.run(p, body, this, args)
    /// ```
    fn start_promise(&mut self, body: &Operand, this: Atom, args: Atom) -> Lowering<Operand> {
        let promise = self.invoke("ts.promise.pending", &[], Ty::Unknown)?;
        let run = self.function("ts.promise.run")?;
        self.emit(Stmt::Assign {
            place: Place::Member(Member::Field {
                target: promise.expr(),
                field: "task".to_string(),
            }),
            value: Rhs::Action(Action::Spawn {
                callee: Callee::Library(run),
                args: vec![promise.atom.clone(), body.atom.clone(), this, args],
            }),
        });
        Ok(promise)
    }
}
