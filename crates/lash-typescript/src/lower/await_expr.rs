use super::*;

impl Lowerer {
    pub(super) fn lower_await(&mut self, inner: &Expr) -> Result<LashExpr, Diagnostic> {
        let async_helper = match inner {
            Expr::Call { callee, .. } => match callee.as_ref() {
                Expr::Ident(name) => self
                    .binding(name)
                    .is_ok_and(|binding| binding.role == BindingRole::AsyncHelper),
                Expr::Function(function) => function.is_async,
                _ => false,
            },
            _ => false,
        };
        // Resolved before lowering, against the scope stack: the awaited name
        // is a process handle only if the binding it reads is one.
        let process_handle = matches!(
            inner,
            Expr::Ident(name)
                if self
                    .binding(name)
                    .is_ok_and(|binding| binding.role == BindingRole::ProcessHandle)
        );
        let promise_kind = match inner {
            Expr::Call { callee, args }
                if matches!(
                    callee.as_ref(),
                    Expr::Member {
                        object,
                        property: MemberProperty::Field(method),
                    } if matches!(object.as_ref(), Expr::Ident(name) if name == "Promise" && !self.has_binding(name))
                        && matches!(method.as_str(), "all" | "allSettled")
                ) =>
            {
                let Expr::Member {
                    property: MemberProperty::Field(method),
                    ..
                } = callee.as_ref()
                else {
                    unreachable!()
                };
                let [CallArg::Value(value)] = args.as_slice() else {
                    return Err(Diagnostic::defect(
                        DiagnosticCode::UnsupportedExpression,
                        format!("Promise.{method} expects one iterable"),
                        None,
                    ));
                };
                Some((method.as_str(), value))
            }
            _ => None,
        };
        if let Some((mode, value)) = promise_kind {
            if self.aggregate_contains_process_handle(value) {
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::AwaitUnsupported,
                    "Promise.all/allSettled process promises require separate await expressions",
                    "await the process promise on its own line, before the aggregate",
                    None,
                ));
            }
            if is_async_map(value) {
                return self.with_await(|lowerer| {
                    if mode == "allSettled" {
                        lowerer.lower_all_settled_async_map(value)
                    } else {
                        lowerer.lower_expr(value)
                    }
                });
            }
            let array = self.lower_expr(value)?;
            let aggregate = LashExpr::BuiltinCall {
                name: "__typescript_await_array".into(),
                args: vec![array, LashExpr::Bool(mode == "allSettled")],
            };
            return Ok(if mode == "allSettled" {
                all_settled_results(aggregate)
            } else {
                aggregate
            });
        }
        let lowered = self.with_await(|lowerer| lowerer.lower_expr(inner))?;
        if async_helper {
            return Ok(lowered);
        }
        if matches!(
            lowered,
            LashExpr::SleepFor(_)
                | LashExpr::SleepUntil(_)
                | LashExpr::WaitSignal { .. }
                | LashExpr::SignalRun { .. }
                | LashExpr::Wake(_)
                | LashExpr::Finish(_)
                | LashExpr::Fail(_)
        ) {
            return Ok(lowered);
        }
        if matches!(lowered, LashExpr::ReceiverCall { .. }) {
            return Ok(LashExpr::Await(Box::new(LashExpr::ResultUnwrap(Box::new(
                lowered,
            )))));
        }
        if matches!(lowered, LashExpr::StartProcess(_)) {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        if process_handle {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_await_pending".into(),
            args: vec![lowered],
        })
    }
    fn aggregate_contains_process_handle(&self, value: &Expr) -> bool {
        let Expr::Array(items) = value else {
            return false;
        };
        items.iter().any(|item| match item {
            ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                self.expr_may_be_process_handle(value)
            }
        })
    }

    fn expr_may_be_process_handle(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Ident(name) => self
                .binding(name)
                .is_ok_and(|binding| binding.role == BindingRole::ProcessHandle),
            Expr::Assign { value, .. } => self.expr_may_be_process_handle(value),
            Expr::Logical { left, right, .. } => {
                self.expr_may_be_process_handle(left) || self.expr_may_be_process_handle(right)
            }
            Expr::Conditional {
                consequent,
                alternate,
                ..
            } => {
                self.expr_may_be_process_handle(consequent)
                    || self.expr_may_be_process_handle(alternate)
            }
            _ => false,
        }
    }
}

fn is_async_map(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Call { callee, args }
            if matches!(callee.as_ref(), Expr::Member { property: MemberProperty::Field(map), .. } if map == "map")
                && matches!(args.as_slice(), [CallArg::Value(Expr::Function(function))] if function.is_async)
    )
}
