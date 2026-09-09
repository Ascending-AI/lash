//! Lowering for `Array.prototype.map`.
//!
//! Split from the general method lowering because it is the one instance
//! method that cannot go through the stdlib builtin: that builtin exports
//! every argument across the host boundary, and a function value cannot cross
//! it. This lowers to the VM's own map driver instead, which means it also
//! owns the arity reasoning that decides which callback shapes can run at all.

use lashlang::{
    AssignTarget, CatchClause, Expr as LashExpr, ExprFolder, FunctionExpr, ListComprehensionClause,
    TryExpr, fold_expr_children,
};

use super::{GENERATED_BINDING_PREFIX, Lowerer};
use crate::adapter::{Expr, Function, FunctionBody, Pattern, Stmt};
use crate::{Diagnostic, DiagnosticCode};

impl Lowerer {
    /// Lowers Promise aggregate maps that can share the runtime-sized list
    /// batch. Async callbacks with another await deliberately return `None`
    /// and stay on the sequential async-map path.
    pub(super) fn lower_promise_map(
        &mut self,
        expression: &Expr,
        settle: bool,
    ) -> Result<Option<LashExpr>, Diagnostic> {
        let Some((object, function)) = promise_map_parts(expression) else {
            return Ok(None);
        };
        if function.params.len() != 1 || !matches!(function.params[0], Pattern::Ident(_)) {
            return Ok(None);
        }

        if function.is_async {
            return self.lower_leading_await_map(object, function, settle);
        }
        if !function_returns_one_call(function) {
            return Ok(None);
        }

        let items = self.lower_expr(object)?;
        let callback = self.lower_eager_tool_map_function(function)?;
        let Some((parameter, call)) = direct_returned_tool_call(callback) else {
            return Ok(None);
        };
        let aggregate = runtime_list_batch(items, parameter, call, !settle);
        Ok(Some(if settle {
            super::all_settled_results(aggregate)
        } else {
            aggregate
        }))
    }

    /// Evaluates `const ps = xs.map(x => tool(x))` immediately through the
    /// existing sequential callback driver. A later Promise aggregate reads a
    /// list of settled values, matching the dialect's eager tool-call model.
    pub(super) fn lower_eager_tool_map(
        &mut self,
        expression: &Expr,
    ) -> Result<Option<LashExpr>, Diagnostic> {
        let Some((object, function)) = promise_map_parts(expression) else {
            return Ok(None);
        };
        if function.is_async
            || function.params.len() != 1
            || !matches!(function.params[0], Pattern::Ident(_))
            || !function_returns_one_call(function)
        {
            return Ok(None);
        }
        let items = self.lower_expr(object)?;
        let callback = self.lower_eager_tool_map_function(function)?;
        let Some(callback) = unwrap_direct_returned_tool_call(callback) else {
            return Ok(None);
        };
        Ok(Some(LashExpr::BuiltinCall {
            name: "__typescript_async_map".into(),
            args: vec![items, callback],
        }))
    }

    fn lower_leading_await_map(
        &mut self,
        object: &Expr,
        function: &Function,
        settle: bool,
    ) -> Result<Option<LashExpr>, Diagnostic> {
        let items = self.lower_expr(object)?;
        let callback = self.lower_expr(&Expr::Function(function.clone()))?;
        let LashExpr::Function(mut projection) = callback else {
            return Ok(None);
        };
        let [parameter] = projection.params.as_slice() else {
            return Ok(None);
        };
        let parameter = parameter.to_string();
        let Some(call) = single_direct_tool_await(&projection.body) else {
            return Ok(None);
        };

        let items_name = self.temporary("promise_map_items");
        let results_name = self.temporary("promise_map_results");
        let pair_name = self.temporary("promise_map_pair");
        let pair_value = LashExpr::Index {
            target: Box::new(LashExpr::Variable(pair_name.as_str().into())),
            index: Box::new(LashExpr::Number(0.0)),
        };
        let replacement = if settle {
            LashExpr::ResultUnwrap(Box::new(pair_value))
        } else {
            pair_value
        };
        let mut rewrite = HoistedAwaitProjection {
            parameter: parameter.clone(),
            items: items_name.as_str().into(),
            pair: pair_name.as_str().into(),
            replacement,
            replaced: false,
        };
        projection.body = Box::new(rewrite.fold_expr(*projection.body));
        debug_assert!(rewrite.replaced, "the selected direct await is replaced");
        projection.params = vec![pair_name.as_str().into()];
        if !projection
            .captures
            .iter()
            .any(|capture| capture.as_str() == items_name)
        {
            projection.captures.push(items_name.as_str().into());
        }
        let mut projection = LashExpr::Function(projection);
        if settle {
            projection = settle_async_callback(projection, self.temporary("settled_reason"));
        }

        let aggregate = runtime_list_batch(
            LashExpr::Variable(items_name.as_str().into()),
            parameter,
            call,
            !settle,
        );
        let enumerated = LashExpr::BuiltinCall {
            name: "__typescript_stdlib".into(),
            args: vec![
                LashExpr::String("__enumerate".into()),
                LashExpr::Variable(results_name.as_str().into()),
            ],
        };
        Ok(Some(LashExpr::Block(vec![
            LashExpr::Assign {
                target: AssignTarget::variable(items_name.as_str().into()),
                expr: Box::new(items),
            },
            LashExpr::Assign {
                target: AssignTarget::variable(results_name.as_str().into()),
                expr: Box::new(aggregate),
            },
            LashExpr::Map {
                items: Box::new(enumerated),
                function: Box::new(projection),
            },
        ])))
    }

    /// `xs.map(callback)` as an in-VM map over the VM's own callback driver.
    ///
    /// ECMA calls the callback with `(value, index, array)`. The VM checks
    /// callback arity exactly, so the shape of the callback decides the
    /// lowering: a one-parameter callback maps directly, and a two-parameter
    /// one maps over `(value, index)` pairs through a generated wrapper. The
    /// third `array` argument and callbacks whose arity is not statically
    /// known reject by name rather than lowering something that cannot run.
    pub(super) fn lower_array_map(
        &mut self,
        object: &Expr,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        self.lower_array_map_with_settlement(object, args, false)
    }

    pub(super) fn lower_all_settled_async_map(
        &mut self,
        expression: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        let Expr::Call { callee, args } = expression else {
            unreachable!("caller identifies an async array map")
        };
        let Expr::Member {
            object,
            property: crate::adapter::MemberProperty::Field(method),
        } = callee.as_ref()
        else {
            unreachable!("caller identifies an async array map")
        };
        debug_assert_eq!(method, "map");
        let args = args
            .iter()
            .map(|arg| match arg {
                crate::adapter::CallArg::Value(value) => Ok(value.clone()),
                crate::adapter::CallArg::Spread(_) => Err(Diagnostic::defect(
                    DiagnosticCode::MethodUnsupported,
                    "map does not accept spread callback arguments",
                    None,
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.lower_array_map_with_settlement(object, &args, true)
    }

    fn lower_array_map_with_settlement(
        &mut self,
        object: &Expr,
        args: &[Expr],
        settle: bool,
    ) -> Result<LashExpr, Diagnostic> {
        let [callback] = args else {
            return Err(Diagnostic::defect(
                DiagnosticCode::MethodUnsupported,
                "map takes exactly one callback argument in v1",
                None,
            ));
        };
        let Expr::Function(function) = callback else {
            return Err(Diagnostic::defect(
                DiagnosticCode::MethodUnsupported,
                "map requires a function literal in v1 so its parameter count is known before it runs",
                None,
            ));
        };
        let items = self.lower_expr(object)?;
        if function.is_async {
            let callback = self.lower_expr(callback)?;
            let callback = if settle {
                settle_async_callback(callback, self.temporary("settled_reason"))
            } else {
                callback
            };
            return Ok(LashExpr::BuiltinCall {
                name: "__typescript_async_map".into(),
                args: vec![items, callback],
            });
        }
        match function.params.len() {
            1 => Ok(LashExpr::Map {
                items: Box::new(items),
                function: Box::new(self.lower_expr(callback)?),
            }),
            2 => {
                // Pair each item with its index, then unpack in a generated
                // one-parameter wrapper so the driver's arity still matches.
                let pairs = LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![LashExpr::String("__enumerate".into()), items],
                };
                let pair = format!("{GENERATED_BINDING_PREFIX}{}_pair", self.next_binding);
                self.next_binding += 1;
                let lowered_callback = self.lower_expr(callback)?;
                let wrapper = format!("{GENERATED_BINDING_PREFIX}{}_map", self.next_binding);
                self.next_binding += 1;
                let index_of = |index: usize| LashExpr::Index {
                    target: Box::new(LashExpr::Variable(pair.as_str().into())),
                    index: Box::new(LashExpr::Number(index as f64)),
                };
                Ok(LashExpr::Block(vec![
                    LashExpr::Assign {
                        target: AssignTarget::variable(wrapper.as_str().into()),
                        expr: Box::new(lowered_callback),
                    },
                    LashExpr::Map {
                        items: Box::new(pairs),
                        function: Box::new(LashExpr::Function(Box::new(FunctionExpr {
                            name: None,
                            params: vec![pair.as_str().into()],
                            captures: vec![wrapper.as_str().into()],
                            body: Box::new(LashExpr::Return(Box::new(LashExpr::Call {
                                function: Box::new(LashExpr::Variable(wrapper.as_str().into())),
                                args: vec![index_of(0), index_of(1)],
                            }))),
                        }))),
                    },
                ]))
            }
            other => Err(Diagnostic::defect(
                DiagnosticCode::MethodUnsupported,
                format!(
                    "map callbacks take the value and optionally its index in v1; this one takes {other} parameters"
                ),
                None,
            )),
        }
    }
}

fn promise_map_parts(expression: &Expr) -> Option<(&Expr, &Function)> {
    let Expr::Call { callee, args } = expression else {
        return None;
    };
    let Expr::Member {
        object,
        property: crate::adapter::MemberProperty::Field(method),
    } = callee.as_ref()
    else {
        return None;
    };
    let [crate::adapter::CallArg::Value(Expr::Function(function))] = args.as_slice() else {
        return None;
    };
    (method == "map").then_some((object.as_ref(), function))
}

fn function_returns_one_call(function: &Function) -> bool {
    match &function.body {
        FunctionBody::Expression(value) => matches!(value.as_ref(), Expr::Call { .. }),
        FunctionBody::Block(statements) => {
            let mut statements = statements
                .iter()
                .filter(|statement| !matches!(statement, Stmt::Empty));
            matches!(
                statements.next(),
                Some(Stmt::Return(Some(Expr::Call { .. })))
            ) && statements.next().is_none()
        }
    }
}

fn direct_returned_tool_call(callback: LashExpr) -> Option<(String, LashExpr)> {
    let LashExpr::Function(function) = callback else {
        return None;
    };
    let [parameter] = function.params.as_slice() else {
        return None;
    };
    let call = direct_return_value(&function.body)?;
    matches!(call, LashExpr::ReceiverCall { .. }).then(|| (parameter.to_string(), call.clone()))
}

fn unwrap_direct_returned_tool_call(mut callback: LashExpr) -> Option<LashExpr> {
    let LashExpr::Function(function) = &mut callback else {
        return None;
    };
    let value = direct_return_value_mut(&mut function.body)?;
    if !matches!(value, LashExpr::ReceiverCall { .. }) {
        return None;
    }
    let call = std::mem::replace(value, LashExpr::Undefined);
    *value = LashExpr::ResultUnwrap(Box::new(call));
    Some(callback)
}

fn direct_return_value(body: &LashExpr) -> Option<&LashExpr> {
    let LashExpr::Block(expressions) = body else {
        return None;
    };
    match expressions.as_slice() {
        [LashExpr::Return(value)] | [LashExpr::Return(value), LashExpr::Undefined] => Some(value),
        [nested @ LashExpr::Block(_)] => direct_return_value(nested),
        _ => None,
    }
}

fn direct_return_value_mut(body: &mut LashExpr) -> Option<&mut LashExpr> {
    let LashExpr::Block(expressions) = body else {
        return None;
    };
    match expressions.as_mut_slice() {
        [LashExpr::Return(value)] | [LashExpr::Return(value), LashExpr::Undefined] => Some(value),
        [nested @ LashExpr::Block(_)] => direct_return_value_mut(nested),
        _ => None,
    }
}

fn runtime_list_batch(items: LashExpr, binding: String, call: LashExpr, unwrap: bool) -> LashExpr {
    let element = if unwrap {
        LashExpr::ResultUnwrap(Box::new(call))
    } else {
        call
    };
    LashExpr::Await(Box::new(LashExpr::ListComprehension {
        element: Box::new(element),
        clauses: vec![ListComprehensionClause::For {
            binding: binding.into(),
            iterable: items,
        }],
    }))
}

fn single_direct_tool_await(body: &LashExpr) -> Option<LashExpr> {
    fn collect(expr: &LashExpr, awaits: &mut Vec<Option<LashExpr>>) {
        match expr {
            LashExpr::Function(_) => {}
            LashExpr::Await(inner) => {
                let call = match inner.as_ref() {
                    LashExpr::ResultUnwrap(call)
                        if matches!(call.as_ref(), LashExpr::ReceiverCall { .. }) =>
                    {
                        Some(call.as_ref().clone())
                    }
                    _ => None,
                };
                awaits.push(call);
            }
            _ => expr.children().for_each(|child| collect(child, awaits)),
        }
    }
    let mut awaits = Vec::new();
    collect(body, &mut awaits);
    match awaits.as_slice() {
        [Some(call)] => Some(call.clone()),
        _ => None,
    }
}

struct HoistedAwaitProjection {
    parameter: String,
    items: String,
    pair: String,
    replacement: LashExpr,
    replaced: bool,
}

impl ExprFolder for HoistedAwaitProjection {
    fn fold_expr(&mut self, expr: LashExpr) -> LashExpr {
        match expr {
            LashExpr::Function(_) => expr,
            LashExpr::Await(inner)
                if !self.replaced
                    && matches!(inner.as_ref(), LashExpr::ResultUnwrap(call) if matches!(call.as_ref(), LashExpr::ReceiverCall { .. })) =>
            {
                self.replaced = true;
                self.replacement.clone()
            }
            LashExpr::Variable(name) if name.as_str() == self.parameter => LashExpr::Index {
                target: Box::new(LashExpr::Variable(self.items.as_str().into())),
                index: Box::new(LashExpr::Index {
                    target: Box::new(LashExpr::Variable(self.pair.as_str().into())),
                    index: Box::new(LashExpr::Number(1.0)),
                }),
            },
            other => fold_expr_children(self, other),
        }
    }
}

fn settled_fulfilled(value: LashExpr) -> LashExpr {
    LashExpr::Record(vec![
        ("status".into(), LashExpr::String("fulfilled".into())),
        ("value".into(), value),
    ])
}

fn settled_rejected(reason: LashExpr) -> LashExpr {
    LashExpr::Record(vec![
        ("status".into(), LashExpr::String("rejected".into())),
        ("reason".into(), reason),
    ])
}

struct SettleReturns;

impl ExprFolder for SettleReturns {
    fn fold_expr(&mut self, expr: LashExpr) -> LashExpr {
        match expr {
            // Returns in a nested function belong to that function, not the
            // async-map callback being settlement-wrapped.
            LashExpr::Function(_) => expr,
            LashExpr::Return(value) => {
                LashExpr::Return(Box::new(settled_fulfilled(self.fold_expr(*value))))
            }
            other => fold_expr_children(self, other),
        }
    }
}

fn settle_async_callback(mut callback: LashExpr, reason: String) -> LashExpr {
    let function = match &mut callback {
        LashExpr::Function(function) => function.as_mut(),
        LashExpr::BuiltinCall { name, args } if name.as_str() == "__typescript_closure" => {
            let Some(LashExpr::Function(function)) = args.first_mut() else {
                unreachable!("closure intrinsic starts with a function")
            };
            function.as_mut()
        }
        _ => unreachable!("async callback lowers to a function or closure intrinsic"),
    };
    let body = SettleReturns.fold_expr(std::mem::replace(
        function.body.as_mut(),
        LashExpr::Undefined,
    ));
    *function.body = LashExpr::Try(Box::new(TryExpr {
        body: Box::new(LashExpr::Block(vec![
            body,
            LashExpr::Return(Box::new(settled_fulfilled(LashExpr::Undefined))),
        ])),
        catch: Some(CatchClause {
            binding: reason.as_str().into(),
            body: Box::new(LashExpr::Return(Box::new(settled_rejected(
                LashExpr::Variable(reason.into()),
            )))),
        }),
        finally: None,
    }));
    callback
}
