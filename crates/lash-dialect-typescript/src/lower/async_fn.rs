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
//!   task, and `await sleep(ms)` one ordinary sleep operation with `then yield`.
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
use lash_kernel_doc::{ActKind, Expr, Literal, Rhs, Stmt};

use {
    lash_kernel_value::EffectName, lash_kernel_value::names::QualifiedName,
    lash_kernel_value::types::Signature, lash_kernel_value::types::Type,
};

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

/// Statically known slots stay intact; a deferred operation accepts a packed list.
enum WaitArguments {
    Slots(Vec<WaitArgument>),
    Packed(WaitArgument),
}

/// Keep the copy-out decision with its value through packing and child tasks.
struct WaitArgument {
    value: Operand,
    may_forward_builtin: bool,
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
        let body = self.closure(function, name)?;
        let Some((Rhs::Expr(Expr::Closure(mut source_body)), note)) = self.take_binding_of(&body)
        else {
            unreachable!("a source body is a fresh closure");
        };
        source_body.presentation = None;
        let Some(written) = note.written.clone() else {
            unreachable!("the source closure carries its Written facts");
        };
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
        let start = self.callable_presentation(function);
        let closure = self.emit_named_closure(params, block, start, name);
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
        let value = match value {
            // An awaited call: an asynchronous package member gives its
            // result where it stands.
            ast::Expr::Call {
                callee,
                args,
                span: call_span,
            } => self.expression_block(|this| this.call(callee, args, *call_span, false, true))?,
            value => self.lower_expr(value)?,
        };
        let policy = self.function_target("ts.await")?;
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
        let start = match wait {
            Wait::Sleep => "ts.promise.sleep",
            Wait::Effect(_) => "ts.promise.start",
        };
        let args = self.wait_arguments(&wait, args, span)?;
        // Construct the owned invocation list before the child starts.
        // Its body reads those slots instead of capturing live bindings.
        let this = self.fresh("this");
        let supplied = self.fresh("args");
        let slots = Operand::variable(supplied.clone(), Ty::Unknown);
        let (args, values) = match args {
            WaitArguments::Slots(args) => {
                let values = args
                    .iter()
                    .enumerate()
                    .map(|(index, argument)| WaitArgument {
                        value: self.inline(Self::element(&slots, index), argument.value.ty.clone()),
                        may_forward_builtin: argument.may_forward_builtin,
                    })
                    .collect();
                (
                    self.inline(
                        Expr::List(args.iter().map(|arg| arg.value.expr()).collect()),
                        Ty::Unknown,
                    ),
                    WaitArguments::Slots(values),
                )
            }
            WaitArguments::Packed(args) => {
                let value = WaitArgument {
                    value: self.inline(Self::element(&slots, 0), args.value.ty.clone()),
                    may_forward_builtin: args.may_forward_builtin,
                };
                (
                    self.inline(Expr::List(vec![args.value.expr()]), Ty::Unknown),
                    WaitArguments::Packed(value),
                )
            }
        };
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
        let promise = self.invoke(start, &[body, Operand::undefined(), args], Ty::Unknown)?;
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
        let expanded = self.imported.get(root).map(|prefix| {
            prefix
                .iter()
                .map(String::as_str)
                .chain(path.iter().skip(1).copied())
                .collect::<Vec<_>>()
        });
        let path = expanded.unwrap_or(path);
        if self.is_bound(root) || lash_ext_ts_helpers::builtins::is_global(root) {
            return None;
        }
        if path == ["sleep"] {
            return Some(Wait::Sleep);
        }
        match self.namespaces.resolve(&path, Spelling::TypeScript) {
            Resolution::Operation { operation, .. } | Resolution::Deferred { operation } => {
                Some(Wait::Effect(operation.effect_name()))
            }
            _ => None,
        }
    }

    fn signature_of(&self, effect: &EffectName) -> Signature {
        self.namespaces
            .operation(effect)
            .map(|(_, offered)| offered.signature.clone())
            .unwrap_or_else(lash_kernel_dialect::deferred_signature)
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
            if let Some(shadowing) = self.namespaces.shadowed_call(&path, Spelling::TypeScript) {
                return Err(Diagnostic::namespace_shadowed(shadowing, span));
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

    /// The arguments of a wait, nested in their left-to-right evaluation order.
    #[expect(
        clippy::cast_precision_loss,
        reason = "source argument count is bounded by source limits"
    )]
    fn wait_arguments(
        &mut self,
        wait: &Wait,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<WaitArguments> {
        let spread = args
            .iter()
            .any(|arg| matches!(arg, ast::CallArg::Spread(_)));
        let static_slots = |this: &mut Self| {
            let values: Vec<_> = args
                .iter()
                .map(|arg| match arg {
                    ast::CallArg::Value(value) => value,
                    ast::CallArg::Spread(_) => unreachable!("the argument list has no spread"),
                })
                .collect();
            let operands = this.operands(&values)?;
            Ok(WaitArguments::Slots(
                operands
                    .into_iter()
                    .zip(values)
                    .map(|(value, source)| WaitArgument {
                        value,
                        may_forward_builtin: this.facts.may_carry_builtin(source),
                    })
                    .collect(),
            ))
        };
        let may_forward_builtin = args.iter().any(|arg| {
            let (ast::CallArg::Value(value) | ast::CallArg::Spread(value)) = arg;
            self.facts.may_carry_builtin(value)
        });
        let (name, least, most) = match wait {
            Wait::Sleep => ("sleep".to_string(), 1, 1),
            Wait::Effect(effect) => {
                if self.namespaces.operation(effect).is_none() {
                    return if spread {
                        self.arguments(args).map(|value| {
                            WaitArguments::Packed(WaitArgument {
                                value,
                                may_forward_builtin,
                            })
                        })
                    } else {
                        static_slots(self)
                    };
                }
                let signature = self.signature_of(effect);
                let required = signature
                    .params
                    .iter()
                    .filter(|param| !param.optional)
                    .count();
                (effect.to_string(), required, signature.params.len())
            }
        };
        let arity = if least == most {
            least.to_string()
        } else {
            format!("{least} to {most}")
        };
        if !spread && (args.len() < least || args.len() > most) {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!(
                    "`{name}` takes {arity} argument(s) and is called with {}",
                    args.len()
                ),
                Some(span),
            ));
        }
        if !spread {
            return static_slots(self);
        }
        let values = self.arguments(args)?;
        let values = self.pin(values);
        if spread {
            let count = self.native("list.len", vec![values.expr()])?;
            let count = self.let_expr(count, Ty::Number);
            for (left, right) in [
                (Operand::number(least as f64).expr(), count.expr()),
                (count.expr(), Operand::number(most as f64).expr()),
            ] {
                let valid = self.native("num.le", vec![left, right])?;
                let invalid = self.native("bool.not", vec![valid])?;
                let error = self.native(
                    "error.new",
                    vec![
                        Operand::text("type_error").expr(),
                        Operand::text(format!("`{name}` takes {arity} argument(s)")).expr(),
                        Operand::null().expr(),
                    ],
                )?;
                let fail = self.block(|this| {
                    this.emit(Stmt::Throw { value: error });
                    Ok(())
                })?;
                self.emit_if(invalid, fail, super::Buf::default());
            }
        }
        let values = self.invoke(
            "ts.pad",
            &[values, Operand::number(most as f64)],
            Ty::Unknown,
        )?;
        let values = self.pin(values);
        Ok(WaitArguments::Slots(
            (0..most)
                .map(|index| WaitArgument {
                    value: self.inline(Self::element(&values, index), Ty::Unknown),
                    may_forward_builtin,
                })
                .collect(),
        ))
    }

    /// Rebuilds builtin callables in authored code before copy-out. Only path,
    /// display name and arity are captured; the library closure never leaves.
    fn builtin_forwarder(&mut self) -> Lowering<Operand> {
        let token = self.fresh("builtin");
        let body = self.block(|this| {
            let value = Operand::variable(token.clone(), Ty::Unknown);
            let path = this.let_expr(Self::element(&value, 1), Ty::Text);
            let name = this.let_expr(Self::element(&value, 2), Ty::Text);
            let arity = this.let_expr(Self::element(&value, 3), Ty::Float);
            let arities: std::collections::BTreeSet<_> = this
                .table
                .builtin_paths()
                .iter()
                .filter_map(|path| match this.table.builtin(path) {
                    Some(lash_ext_ts_helpers::builtins::Builtin::Function { length, .. }) => {
                        Some(length)
                    }
                    _ => None,
                })
                .collect();
            for length in arities {
                let matches = this.same(arity.expr(), Operand::number(f64::from(length)).expr())?;
                let matched = this.block(|this| {
                    let receiver = this.fresh("receiver");
                    let args = this.fresh("args");
                    let forwarding = this.block(|this| {
                        let output = this.invoke(
                            "ts.builtin_call",
                            &[
                                path.clone(),
                                Operand::variable(receiver.clone(), Ty::Unknown),
                                Operand::variable(args.clone(), Ty::Array),
                            ],
                            Ty::Unknown,
                        )?;
                        this.emit(Stmt::Return {
                            value: output.expr(),
                        });
                        Ok(())
                    })?;
                    let signature = Signature {
                        params: (0..length)
                            .map(|index| lash_kernel_value::Param {
                                name: lash_kernel_value::Name::new(format!("arg{index}")),
                                ty: Type::Any,
                                optional: true,
                            })
                            .collect(),
                        result: Type::Any,
                    };
                    let start = lash_kernel_doc::CallablePresentation {
                        signature,
                        representation: crate::function_values(),
                        convention: this.entry_convention(true)?,
                    };
                    let callable = this.emit_closure(vec![receiver, args], forwarding, Some(start));
                    this.emit(Stmt::Return {
                        value: Expr::Tuple(vec![
                            Operand::text(crate::FUNCTION_TAG).expr(),
                            Operand::text("").expr(),
                            name.expr(),
                            arity.expr(),
                            callable.expr(),
                            Expr::Record(vec![lash_kernel_doc::RecordEntry {
                                field: "native".into(),
                                value: Expr::Literal(Literal::Bool(true)),
                            }]),
                        ]),
                    });
                    Ok(())
                })?;
                this.emit_if(matches, matched, super::Buf::default());
            }
            // The immutable row table supplied the arity; a foreign token
            // cannot be constructed by guest source.
            this.emit(Stmt::Return {
                value: Operand::undefined().expr(),
            });
            Ok(())
        })?;
        let factory = self.emit_closure(vec![token], body, None);
        let Some((Rhs::Expr(factory), _)) = self.take_binding_of(&factory) else {
            unreachable!("the forwarding factory is a fresh closure");
        };
        // Copy-out owns the factory. It is not an authored statement or a
        // binding of the enclosing source scope.
        Ok(self.inline(factory, Ty::Unknown))
    }

    /// The wait as one statement, giving its result. Awaited where it
    /// stands, it takes one more turn once its outcome is in (see the
    /// module).
    fn wait_action(
        &mut self,
        wait: Wait,
        args: WaitArguments,
        then_yield: bool,
    ) -> Lowering<Operand> {
        match wait {
            Wait::Effect(effect) => {
                let signature = self.signature_of(&effect);
                let deferred = self.namespaces.operation(&effect).is_none();
                let operation = lash_kernel_value::names::QualifiedOperation::from_effect(&effect)
                    .map_err(|error| {
                        Diagnostic::new(DiagnosticCode::LinkError, error.to_string(), self.span)
                    })?;
                let slot = self.imports.operation(
                    QualifiedName {
                        package: operation.package,
                        member: operation.name,
                        type_member: None,
                    },
                    signature.clone(),
                );
                let result = signature.result.clone();
                let ty = Ty::decoded(&result);
                let may_forward = match &args {
                    WaitArguments::Packed(arg) => arg.may_forward_builtin,
                    WaitArguments::Slots(args) => args.iter().any(|arg| arg.may_forward_builtin),
                };
                let forward = if may_forward {
                    self.builtin_forwarder()?
                } else {
                    Operand::undefined()
                };
                let arguments = match args {
                    WaitArguments::Packed(args) => {
                        let boundary = self.invoke(
                            crate::constructs::conventions::ARGUMENT_COPY,
                            &[args.value, forward],
                            Ty::Unknown,
                        )?;
                        vec![boundary.expr()]
                    }
                    WaitArguments::Slots(args) => {
                        let mut copied = Vec::with_capacity(args.len());
                        for arg in args {
                            if matches!(arg.value.expr, Expr::Literal(_)) {
                                copied.push(arg.value.expr);
                                continue;
                            }
                            let factory = if arg.may_forward_builtin {
                                forward.clone()
                            } else {
                                Operand::undefined()
                            };
                            let value = self.invoke(
                                crate::constructs::conventions::ARGUMENT_COPY,
                                &[arg.value, factory],
                                Ty::Unknown,
                            )?;
                            copied.push(value.expr());
                        }
                        if deferred {
                            vec![Expr::List(copied)]
                        } else {
                            copied
                        }
                    }
                };
                Ok(self.action(
                    ActKind::Perform {
                        slot,
                        result,
                        then_yield,
                    },
                    arguments,
                    ty,
                ))
            }
            Wait::Sleep => {
                // Sleep returns undefined after its admitted duration is consumed.
                let WaitArguments::Slots(args) = args else {
                    unreachable!("sleep has an admitted slot")
                };
                let Some(duration) = args.into_iter().next() else {
                    unreachable!("sleep has one argument")
                };
                let duration = duration.value;
                let duration = self.invoke("ts.sleep_duration", &[duration], Ty::Number)?;
                let slot = self.imports.operation(
                    lash_kernel_dialect::service_name("typescript", "sleep"),
                    lash_kernel_dialect::sleep_signature(),
                );
                let sleep = self.action(
                    ActKind::Perform {
                        slot,
                        result: Type::Null,
                        then_yield,
                    },
                    vec![duration.expr()],
                    Ty::Unknown,
                );
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
