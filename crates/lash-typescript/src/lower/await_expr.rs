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
            if is_async_map(value) {
                return self.with_await(|lowerer| {
                    if mode == "allSettled" {
                        lowerer.lower_all_settled_async_map(value)
                    } else {
                        lowerer.lower_expr(value)
                    }
                });
            }
            // The operand is lowered as top-level code whatever surrounds the
            // aggregate. Tool calls become pending handles only at await depth
            // zero (`calls.rs`), and an aggregate written inside another
            // awaited call's arguments inherited that call's depth: its leaves
            // lowered as plain calls whose `{ok:false,error}` envelopes the
            // aggregate then reported as fulfilled values, so `try/catch`
            // never fired. Process handles in the array are values here; the
            // runtime awaits them after the tool batch settles (ADR 0086).
            let array = self.at_top_level_await_depth(|lowerer| lowerer.lower_expr(value))?;
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
}

fn is_async_map(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Call { callee, args }
            if matches!(callee.as_ref(), Expr::Member { property: MemberProperty::Field(map), .. } if map == "map")
                && matches!(args.as_slice(), [CallArg::Value(Expr::Function(function))] if function.is_async)
    )
}
