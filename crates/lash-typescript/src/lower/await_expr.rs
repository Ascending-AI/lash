use super::*;

impl Lowerer {
    pub(super) fn lower_await(&mut self, inner: &Expr) -> Result<LashExpr, Diagnostic> {
        let async_helper = match inner {
            Expr::Call { callee, .. } => match callee.as_ref() {
                Expr::Ident(name) => self
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(name))
                    .is_some_and(|binding| self.async_bindings.contains(&binding.internal)),
                Expr::Function(function) => function.is_async,
                _ => false,
            },
            _ => false,
        };
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
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("Promise.{method} expects one iterable"),
                        None,
                    ));
                };
                let async_map = matches!(
                    value,
                    Expr::Call { callee, args }
                        if matches!(callee.as_ref(), Expr::Member { property: MemberProperty::Field(map), .. } if map == "map")
                            && matches!(args.as_slice(), [CallArg::Value(Expr::Function(function))] if function.is_async)
                );
                if !matches!(value, Expr::Array(_)) && !async_map {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AwaitUnsupported,
                        format!("Promise.{method} currently requires an array iterable"),
                        None,
                    ));
                }
                Some((method.as_str(), value))
            }
            _ => None,
        };
        self.await_depth += 1;
        let (mode, lowered) = if let Some((mode, value)) = promise_kind {
            let lowered = if mode == "allSettled" && is_async_map(value) {
                self.lower_all_settled_async_map(value)
            } else {
                self.lower_expr(value)
            };
            (Some(mode), lowered)
        } else {
            (None, self.lower_expr(inner))
        };
        self.await_depth -= 1;
        let lowered = lowered?;
        if async_helper {
            return Ok(lowered);
        }
        if matches!(mode, Some("all" | "allSettled"))
            && matches!(&lowered, LashExpr::BuiltinCall { name, .. } if name.as_str() == "__typescript_async_map")
        {
            return Ok(lowered);
        }
        if mode.is_some() && has_unsupported_aggregate_effect(&lowered) {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled currently aggregate tool promises and resolved values; process and timer promises require separate await expressions",
                None,
            ));
        }
        if mode.is_some() && has_nested_aggregate_effect(&lowered) {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled tool promises must be top-level array elements",
                None,
            ));
        }
        if mode.is_some()
            && has_aggregate_effect_leaf(&lowered)
            && has_unbatchable_aggregate_value(&lowered)
        {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled cannot mix tool promises with computed function or assignment values in v1",
                None,
            ));
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
        if mode == Some("allSettled") {
            let has_effect = has_aggregate_effect_leaf(&lowered);
            let settled = settle_aggregate_leaves(lowered);
            let values = if has_effect {
                LashExpr::Await(Box::new(settled))
            } else {
                settled
            };
            return Ok(all_settled_results(values));
        }
        if mode == Some("all") {
            if has_aggregate_effect_leaf(&lowered) {
                return Ok(LashExpr::Await(Box::new(unwrap_aggregate_leaves(lowered))));
            }
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
        if matches!(
            &lowered,
            LashExpr::Variable(name) if self.process_handle_bindings.contains(name.as_str())
        ) {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        Err(Diagnostic::new(
            DiagnosticCode::AwaitUnsupported,
            "await supports tools, process handles, sleep, waitSignal, and Promise.all/allSettled",
            None,
        ))
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
