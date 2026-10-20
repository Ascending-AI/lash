//! Asynchronous source as kernel tasks.
//!
//! A promise is a record over the task that settles it (`helpers/
//! promise.kernel`). The lowerer writes four shapes:
//!
//! - **An `async` function** is a closure that starts a promise whose task
//!   runs the function's body. The body runs at once, up to its first wait
//!   (`K-TASK-002`), as JavaScript runs it up to its first `await`.
//! - **`await value`** is `await value via ts.await`: a `join` on a
//!   promise's task, after one `yield` when the promise has already
//!   settled, and one `yield` alone for a value that is no promise.
//! - **`await tool(args)`** is one `perform … then yield` in the awaiting
//!   task, and `await sleep(ms)` one `sleep … then yield`.
//! - **`tool(args)` and `sleep(ms)` not awaited where they stand** are
//!   promises whose task performs the effect or sleeps.
//!
//! The turn an awaited effect takes once its outcome is in (`then yield`,
//! `K-TASK-005`) keeps one order between the two ways of calling a tool.
//! An outcome delivered to a promise's task wakes that task, which ends and
//! only then wakes the tasks that await the promise: two turns of the
//! queue. A `perform` in the awaiting task would take one, so of two
//! outcomes delivered together the one awaited in place would always run
//! first, which JavaScript does not do. With the extra turn every delivery
//! takes two turns and the order of continuations is the order of arrival.
//! The turn is taken when the effect fails or is cancelled too, before the
//! raise reaches any `catch`.
//!
//! Async function adapters hand their inline source body to ts.promise.start.
//! Unawaited tool calls use the same adapter with an inline effect body. The
//! actual spawn site and its occurrence name the child (`K-TASK-020`).

use lash_kernel_dialect::{Resolution, Spelling};
use lash_kernel_doc::{ActKind, EffectName, Expr, Literal, Rhs, Signature, Stmt};

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
        name: &str,
        _span: Option<SourceSpan>,
    ) -> Lowering<Operand> {
        let body = self.closure(function)?;
        let Some((Rhs::Expr(Expr::Closure(mut source_body)), note)) = self.take_binding_of(&body)
        else {
            unreachable!("a source body is a fresh closure");
        };
        let convention = source_body.start.take().map(|start| start.convention);
        let written = self.written(function);
        let this = self.fresh("this");
        let args = self.fresh("args");
        let params = vec![this.clone(), args.clone()];
        let block = self.block(|lowerer| {
            // Own the packed invocation slots before the child starts. The
            // source body stays inline; ts.promise.start owns its task setup.
            let owned = lowerer.native("list.copy", vec![Expr::Variable(args)])?;
            let owned = lowerer.inline(owned, Ty::Unknown);
            let body = lowerer.inline(Expr::Closure(source_body), Ty::Unknown);
            let promise = lowerer.invoke(
                "ts.promise.start",
                &[body, Operand::variable(this, Ty::Unknown), owned],
                Ty::Unknown,
            )?;
            lowerer.push(
                Stmt::Return {
                    value: promise.expr(),
                },
                note.blocks,
            );
            Ok(())
        })?;
        self.written = Some(written);
        let start = self.start_adapter(function);
        let closure = self.emit_closure(params, block, start);
        if let Some(Stmt::Let {
            value: Rhs::Expr(Expr::Closure(closure)),
            ..
        }) = self.buf.last_stmt_mut()
            && let Some(start) = &mut closure.start
            && let Some(convention) = convention
        {
            start.convention = convention;
        }
        Ok(self.function_token(function, name, closure))
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
            let result = self.wait_action(wait, args, true)?;
            if control {
                // The call settled, so the turn is over: nothing after it
                // runs (`K-FORM-019`). The host reads the control from the
                // settled call, never from this value.
                let name = self.temp();
                self.emit(Stmt::Finish {
                    value: Expr::Let {
                        name,
                        value: Box::new(result.expr()),
                        body: Box::new(Expr::Literal(Literal::Null)),
                    },
                });
                return Ok(Operand::undefined());
            }
            return Ok(result);
        }
        let value = self.lower_expr(value)?;
        let policy = self.function("ts.await")?;
        let awaited = self.action(ActKind::Await(policy), vec![value.expr()], Ty::Unknown);
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
        // Construct the owned invocation list before the child starts.
        // Its body reads those slots instead of capturing live bindings.
        let this = self.fresh("this");
        let supplied = self.fresh("args");
        let slots = Operand::variable(supplied.clone(), Ty::Unknown);
        let values = (0..args.len())
            .map(|index| Self::element(&slots, index))
            .collect();
        let block = self.block(|lowerer| {
            let result = lowerer.wait_action(wait, values, false)?;
            lowerer.emit(Stmt::Return {
                value: result.expr(),
            });
            Ok(())
        })?;
        let body = self.emit_closure(vec![this, supplied], block, None);
        let Some((Rhs::Expr(body), note)) = self.take_binding_of(&body) else {
            unreachable!("a wait body is a fresh closure");
        };
        let body = self.inline(body, Ty::Unknown);
        let args = self.inline(Expr::List(args), Ty::Unknown);
        let promise = self.invoke(
            "ts.promise.start",
            &[body, Operand::undefined(), args],
            Ty::Unknown,
        )?;
        let promise = self.let_expr(promise.expr(), Ty::Unknown);
        if let Some(last) = self.buf.last_note_mut() {
            last.blocks = note.blocks;
        }
        Ok(promise)
    }

    /// Whether the wait is a call of an effect that ends the turn.
    fn control_call(&self, wait: &Wait) -> bool {
        matches!(wait, Wait::Effect(effect) if self.namespaces.controls(effect).is_some())
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
        let (root, path) = super::calls::path_segments(callee)?;
        if self.is_bound(root) || crate::builtins::is_global(root) {
            return None;
        }
        if path == ["sleep"] {
            return Some(Wait::Sleep);
        }
        match self.namespaces.resolve(&path, Spelling::TypeScript) {
            Resolution::Operation { operation, .. } => Some(Wait::Effect(operation.effect_name())),
            _ => None,
        }
    }

    /// The signature `effect`, an offered operation, is performed under.
    fn signature_of(&self, effect: &EffectName) -> &Signature {
        match self.namespaces.operation(effect) {
            Some((_, offered)) => &offered.requirement.signature,
            None => unreachable!("a wait's effect is an offered operation"),
        }
    }

    /// The refusal of a call whose dotted path names an operation the
    /// program cannot reach as written: its root is a binding, which
    /// shadows the package root lexically, or it spells the operation in
    /// another dialect's spelling (`inbox.send_item` for `inbox.sendItem`).
    pub(super) fn operation_refusal(&self, callee: &ast::Expr, span: SourceSpan) -> Lowering<()> {
        let Some((root, path)) = super::calls::path_segments(callee) else {
            return Ok(());
        };
        if self.is_bound(root) {
            if let Resolution::Operation { operation, .. } =
                self.namespaces.resolve(&path, Spelling::TypeScript)
            {
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::OperationShadowed,
                    format!(
                        "`{root}` is a binding here, so `{}` calls a value, while the session offers the operation `{}`",
                        path.join("."),
                        operation.effect_name()
                    ),
                    format!("rename `{root}` and update its references"),
                    Some(span),
                ));
            }
            return Ok(());
        }
        if let Some(misspelled) = self.namespaces.misspelled(&path, Spelling::TypeScript) {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::OperationSpelling,
                format!(
                    "`{}` names the operation `{}` in another dialect's spelling",
                    path.join("."),
                    misspelled.operation.effect_name()
                ),
                format!("write `{}`", misspelled.repair),
                Some(span),
            ));
        }
        Ok(())
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

    /// The arguments of a wait, nested in their left-to-right evaluation order.
    fn wait_arguments(
        &mut self,
        wait: &Wait,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<Vec<Expr>> {
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
                let params = &self.signature_of(effect).params;
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
        Ok(operands.iter().map(Operand::expr).collect())
    }

    /// The wait as one statement, giving its result. Awaited where it
    /// stands, it takes one more turn once its outcome is in (see the
    /// module).
    fn wait_action(&mut self, wait: Wait, args: Vec<Expr>, then_yield: bool) -> Lowering<Operand> {
        match wait {
            Wait::Effect(effect) => {
                let signature = self.signature_of(&effect).clone();
                // The perform states the result type the tool declares, so
                // an `Int` or a `Float` is decoded as declared, and what
                // it gives is known to be that type.
                let result = signature.result.clone();
                let ty = Ty::decoded(&result);
                self.performed.insert(
                    effect.clone(),
                    lash_kernel_doc::OperationRequirement::of_signature(signature),
                );
                let mut copied = Vec::with_capacity(args.len());
                for expr in args {
                    // The boundary gives a literal back as it is: it holds
                    // no array slot to fill and no function token.
                    if matches!(expr, Expr::Literal(_)) {
                        copied.push(expr);
                        continue;
                    }
                    let value = self.inline(expr, Ty::Unknown);
                    let value = self.invoke("ts.boundary", &[value], Ty::Unknown)?;
                    copied.push(value.expr());
                }
                Ok(self.action(
                    ActKind::Perform {
                        effect,
                        result,
                        then_yield,
                    },
                    copied,
                    ty,
                ))
            }
            Wait::Sleep => {
                let Some(duration) = args.into_iter().next() else {
                    unreachable!("`sleep` is checked to take one argument");
                };
                // Sleep returns Null in the kernel, whereas JavaScript sleep
                // resolves to undefined. A scoped role preserves that value.
                let sleep = self.action(ActKind::Sleep { then_yield }, vec![duration], Ty::Unknown);
                let name = self.temp();
                Ok(self.inline(
                    Expr::Let {
                        name,
                        value: Box::new(sleep.expr()),
                        body: Box::new(Operand::undefined().expr()),
                    },
                    Ty::Undefined,
                ))
            }
        }
    }
}
