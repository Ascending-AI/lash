use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

use super::{Reader, array, invalid, string};
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
        Ok(k::Function {
            params,
            body: self.block(&body.stmts)?,
        })
    }
    pub(super) fn expression(&mut self, expr: &s::Expr) -> Result<k::Expr, Diagnostic> {
        if let Some(native) = self.native_expression(expr) {
            return native;
        }
        if let Some(readable) = self.readable_expression(expr) {
            return readable;
        }
        match expr {
            s::Expr::Paren(value) => self.expression(&value.expr),
            s::Expr::Ident(ident) => Ok(k::Expr::Variable(self.name(ident)?)),
            s::Expr::Fn(_) => {
                let function = self.function(expr)?;
                Ok(k::Expr::Closure(Box::new(k::Closure {
                    params: function.params,
                    body: function.body,
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
    pub(super) fn atoms(&mut self, expr: &s::Expr) -> Result<Vec<k::Atom>, Diagnostic> {
        array(expr)?.iter().map(|expr| self.atom(expr)).collect()
    }
    pub(super) fn rhs(&mut self, expr: &s::Expr) -> Result<k::Rhs, Diagnostic> {
        if let Some(action) = self.native_action(expr) {
            action.map(k::Rhs::Action)
        } else {
            self.expression(expr).map(k::Rhs::Expr)
        }
    }
}
