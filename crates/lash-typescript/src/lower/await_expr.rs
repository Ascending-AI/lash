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
                // An allow-list, not a deny-list: nothing checks at runtime
                // that the aggregate's argument is an array, so a shape this
                // pass cannot see an array in has to be refused here. These
                // are exactly the three the dialect grants.
                let array_valued = matches!(value, Expr::Array(_))
                    || is_map_call(value)
                    || matches!(value, Expr::Ident(name)
                    if self.binding(name).is_ok_and(|binding| {
                        matches!(binding.role, BindingRole::ArrayValue(_))
                    }));
                if !array_valued {
                    return Err(Diagnostic::with_repair(
                        DiagnosticCode::AwaitUnsupported,
                        format!("Promise.{method} requires an array-valued expression"),
                        "pass an array literal, an inline `.map(...)`, or an identifier that holds an array",
                        None,
                    ));
                }
                Some((method.as_str(), value))
            }
            _ => None,
        };
        if let Some((mode, value)) = promise_kind
            && let Some(mapped) =
                self.with_await(|lowerer| lowerer.lower_promise_map(value, mode == "allSettled"))?
        {
            return Ok(mapped);
        }
        let aggregate_process_handle = promise_kind
            .as_ref()
            .is_some_and(|(_, value)| self.aggregate_contains_process_handle(value));
        let (mode, lowered) = self.with_await(|lowerer| {
            if let Some((mode, value)) = promise_kind {
                let lowered = if mode == "allSettled" && is_async_map(value) {
                    lowerer.lower_all_settled_async_map(value)
                } else {
                    lowerer.lower_expr(value)
                }?;
                Ok((Some(mode), lowered))
            } else {
                Ok((None, lowerer.lower_expr(inner)?))
            }
        })?;
        if async_helper {
            return Ok(lowered);
        }
        if matches!(mode, Some("all" | "allSettled"))
            && matches!(&lowered, LashExpr::BuiltinCall { name, .. } if name.as_str() == "__typescript_async_map")
        {
            return Ok(lowered);
        }
        if mode.is_some()
            && (aggregate_process_handle || has_unsupported_aggregate_effect(&lowered))
        {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled currently aggregate tool promises and resolved values; process and timer promises require separate await expressions",
                "await the process or timer promise on its own line, before the aggregate",
                None,
            ));
        }
        if mode.is_some()
            && has_aggregate_effect_leaf(&lowered)
            && has_nested_aggregate_effect(&lowered)
        {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled tool promises must be top-level array elements",
                "lift each tool call to its own element of the array literal",
                None,
            ));
        }
        if mode.is_some()
            && has_aggregate_effect_leaf(&lowered)
            && has_unbatchable_aggregate_value(&lowered)
        {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::AwaitUnsupported,
                "Promise.all/allSettled cannot mix tool promises with computed function or assignment values in v1",
                "bind the computed values first, then aggregate only the tool promises",
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
            if !has_effect {
                return Ok(all_settled_resolved_values(lowered));
            }
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
        if process_handle {
            return Ok(LashExpr::ResultUnwrap(Box::new(LashExpr::Await(Box::new(
                lowered,
            )))));
        }
        Err(Diagnostic::with_repair(
            DiagnosticCode::AwaitUnsupported,
            "await supports tools, process handles, sleep, waitSignal, and Promise.all/allSettled",
            "drop the `await`: this value is already settled",
            None,
        ))
    }

    /// Whether the aggregate's argument may carry a process handle.
    ///
    /// The inline and identifier-bound spellings of the same program have to
    /// agree, so a name whose binding recorded process handles is refused
    /// exactly like the array literal it was bound from.
    fn aggregate_contains_process_handle(&self, value: &Expr) -> bool {
        match value {
            Expr::Array(items) => items.iter().any(|item| match item {
                ArrayElement::Value(value) | ArrayElement::Spread(value) => {
                    self.expr_may_be_process_handle(value)
                }
            }),
            Expr::Ident(name) => self.binding(name).is_ok_and(|binding| {
                binding.role == BindingRole::ArrayValue(ArrayContents::ProcessHandles)
            }),
            _ => false,
        }
    }

    pub(super) fn expr_may_be_process_handle(&self, expr: &Expr) -> bool {
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

/// An inline `xs.map(callback)` with a function literal, which the aggregate
/// accepts whether or not the callback is async.
fn is_map_call(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Call { callee, args }
            if matches!(callee.as_ref(), Expr::Member { property: MemberProperty::Field(map), .. } if map == "map")
                && matches!(args.as_slice(), [CallArg::Value(Expr::Function(_))])
    )
}

fn is_async_map(value: &Expr) -> bool {
    matches!(
        value,
        Expr::Call { callee, args }
            if matches!(callee.as_ref(), Expr::Member { property: MemberProperty::Field(map), .. } if map == "map")
                && matches!(args.as_slice(), [CallArg::Value(Expr::Function(function))] if function.is_async)
    )
}
