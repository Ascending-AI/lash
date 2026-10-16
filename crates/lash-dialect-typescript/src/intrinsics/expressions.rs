use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

use super::{Reader, invalid, string};
use crate::Diagnostic;

impl Reader {
    pub(super) fn function(&mut self, expr: &s::Expr) -> Result<k::Function, Diagnostic> {
        let s::Expr::Fn(function) = expr else {
            return Err(invalid(
                "expected a kernel function body",
                Some(expr.span()),
            ));
        };
        if function.ident.is_some() {
            return Err(invalid("closure has no expression name", Some(expr.span())));
        }
        self.read_function(&function.function)
    }
    pub(super) fn read_function(
        &mut self,
        function: &s::Function,
    ) -> Result<k::Function, Diagnostic> {
        if function.is_generator
            || function.type_params.is_some()
            || function.return_type.is_some()
            || !function.decorators.is_empty()
        {
            return Err(invalid(
                "generated functions have no generator marker or annotations",
                Some(function.span),
            ));
        }
        let params = function
            .params
            .iter()
            .map(|param| match &param.pat {
                s::Pat::Ident(binding)
                    if binding.type_ann.is_none()
                        && !binding.id.optional
                        && param.decorators.is_empty() =>
                {
                    self.name(&binding.id)
                }
                _ => Err(invalid("kernel parameters are names", Some(param.span()))),
            })
            .collect::<Result<_, _>>()?;
        let body = function
            .body
            .as_ref()
            .ok_or_else(|| invalid("kernel functions have a body", Some(function.span)))?;
        let parent = std::mem::take(&mut self.types);
        let body = self.block(&body.stmts)?;
        self.types = parent;
        Ok(k::Function {
            params,
            body,
            start: None,
        })
    }
    pub(super) fn expression(&mut self, expr: &s::Expr) -> Result<k::Expr, Diagnostic> {
        if let s::Expr::Await(awaited) = expr
            && async_scope(&awaited.arg)
        {
            return self.expression(&awaited.arg);
        }
        if let Some(action) = self.native_action(expr) {
            return action.map(super::native::NativeAction::expression);
        }
        if let Some(native) = self.native_expression(expr) {
            return native;
        }
        if let Some(readable) = self.readable_expression(expr) {
            return readable;
        }
        match expr {
            s::Expr::Cond(value) => Ok(k::Expr::If {
                cond: Box::new(self.expression(&value.test)?),
                then: Box::new(self.expression(&value.cons)?),
                r#else: Box::new(self.expression(&value.alt)?),
            }),
            s::Expr::Call(call) if call.args.len() == 1 && call.args[0].spread.is_none() => {
                let s::Callee::Expr(callee) = &call.callee else {
                    return Err(invalid("expected scoped expression", Some(expr.span())));
                };
                let mut callee = callee.as_ref();
                while let s::Expr::Paren(paren) = callee {
                    callee = &paren.expr;
                }
                let s::Expr::Arrow(arrow) = callee else {
                    return Err(invalid("expected scoped expression", Some(expr.span())));
                };
                let [s::Pat::Ident(binding)] = arrow.params.as_slice() else {
                    return Err(invalid(
                        "a scoped expression binds one name",
                        Some(expr.span()),
                    ));
                };
                let s::BlockStmtOrExpr::Expr(body) = arrow.body.as_ref() else {
                    return Err(invalid(
                        "a scoped expression has an expression body",
                        Some(expr.span()),
                    ));
                };
                Ok(k::Expr::Let {
                    name: self.name(&binding.id)?,
                    value: Box::new(self.expression(&call.args[0].expr)?),
                    body: Box::new(self.expression(body)?),
                })
            }
            s::Expr::Paren(value) => self.expression(&value.expr),
            s::Expr::Ident(ident) => Ok(k::Expr::Variable(self.name(ident)?)),
            s::Expr::Fn(_) => {
                let function = self.function(expr)?;
                Ok(k::Expr::Closure(Box::new(k::Closure {
                    params: function.params,
                    body: function.body,
                    start: None,
                })))
            }
            s::Expr::Lit(s::Lit::Null(_)) => Ok(k::Expr::Literal(k::Literal::Null)),
            s::Expr::Lit(s::Lit::Bool(value)) => {
                Ok(k::Expr::Literal(k::Literal::Bool(value.value)))
            }
            s::Expr::Lit(s::Lit::Str(_)) => Ok(k::Expr::Literal(k::Literal::Text(string(expr)?))),
            _ => Err(invalid(
                "unsupported generated expression",
                Some(expr.span()),
            )),
        }
    }
    pub(super) fn atom(&mut self, expr: &s::Expr) -> Result<k::Atom, Diagnostic> {
        match self.expression(expr)? {
            k::Expr::Literal(value) => Ok(k::Atom::Literal(value)),
            k::Expr::Variable(name) => Ok(k::Atom::Variable(name)),
            _ => Err(invalid(
                "an action operand must be a variable or literal",
                Some(expr.span()),
            )),
        }
    }
    pub(super) fn rhs(&mut self, expr: &s::Expr) -> Result<k::Rhs, Diagnostic> {
        if let Some(action) = self.native_action(expr) {
            action.map(super::native::NativeAction::rhs)
        } else {
            self.expression(expr).map(|expr| match expr {
                k::Expr::Act(act) => match act.as_action() {
                    Some(action) => k::Rhs::Action(action),
                    None => k::Rhs::Expr(k::Expr::Act(act)),
                },
                expr => k::Rhs::Expr(expr),
            })
        }
    }
}

/// An awaited async scoped-expression wrapper is syntax for Expr::Let,
/// rather than another kernel action.
pub(super) fn async_scope(expr: &s::Expr) -> bool {
    let s::Expr::Call(call) = expr else {
        return false;
    };
    let s::Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let mut callee = callee.as_ref();
    while let s::Expr::Paren(paren) = callee {
        callee = &paren.expr;
    }
    matches!(callee, s::Expr::Arrow(arrow) if arrow.is_async)
}
