//! Lowering for `Array.prototype.map`.
//!
//! Split from the general method lowering because it is the one instance
//! method that cannot go through the stdlib builtin: that builtin exports
//! every argument across the host boundary, and a function value cannot cross
//! it. This lowers to the VM's own map driver instead, which means it also
//! owns the arity reasoning that decides which callback shapes can run at all.

use lashlang::{
    AssignTarget, CatchClause, Expr as LashExpr, ExprFolder, FunctionExpr, TryExpr,
    fold_expr_children,
};

use super::{GENERATED_BINDING_PREFIX, Lowerer};
use crate::adapter::Expr;
use crate::{Diagnostic, DiagnosticCode};

impl Lowerer {
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
                crate::adapter::CallArg::Spread(_) => Err(Diagnostic::new(
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
            return Err(Diagnostic::new(
                DiagnosticCode::MethodUnsupported,
                "map takes exactly one callback argument in v1",
                None,
            ));
        };
        let Expr::Function(function) = callback else {
            return Err(Diagnostic::new(
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
            other => Err(Diagnostic::new(
                DiagnosticCode::MethodUnsupported,
                format!(
                    "map callbacks take the value and optionally its index in v1; this one takes {other} parameters"
                ),
                None,
            )),
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
