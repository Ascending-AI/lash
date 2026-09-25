use super::*;

impl<'module> Linker<'module> {
    pub(super) fn lower_javascript_unary(
        &self,
        op: &crate::ast::JavaScriptUnaryOp,
        expr: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let ty = match op {
            crate::ast::JavaScriptUnaryOp::Not => TypeExpr::Bool,
            crate::ast::JavaScriptUnaryOp::TypeOf | crate::ast::JavaScriptUnaryOp::ToString => {
                TypeExpr::Str
            }
            crate::ast::JavaScriptUnaryOp::Plus
            | crate::ast::JavaScriptUnaryOp::Negate
            | crate::ast::JavaScriptUnaryOp::BitNot => TypeExpr::Float,
        };
        Ok((
            Expr::JavaScriptUnary {
                op: *op,
                expr: Box::new(self.lower_expr(expr, &path.child(0), scope)?.0),
            },
            Binding::Value(ty),
        ))
    }

    pub(super) fn lower_javascript_binary(
        &self,
        left: &Expr,
        op: &crate::ast::JavaScriptBinaryOp,
        right: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let ty = match op {
            crate::ast::JavaScriptBinaryOp::StrictEqual
            | crate::ast::JavaScriptBinaryOp::StrictNotEqual
            | crate::ast::JavaScriptBinaryOp::LooseEqual
            | crate::ast::JavaScriptBinaryOp::LooseNotEqual
            | crate::ast::JavaScriptBinaryOp::Less
            | crate::ast::JavaScriptBinaryOp::LessEqual
            | crate::ast::JavaScriptBinaryOp::Greater
            | crate::ast::JavaScriptBinaryOp::GreaterEqual => TypeExpr::Bool,
            _ => TypeExpr::Any,
        };
        Ok((
            Expr::JavaScriptBinary {
                left: Box::new(self.lower_expr(left, &path.child(0), scope)?.0),
                op: *op,
                right: Box::new(self.lower_expr(right, &path.child(1), scope)?.0),
            },
            Binding::Value(ty),
        ))
    }

    pub(super) fn lower_javascript_logical(
        &self,
        left: &Expr,
        op: &crate::ast::JavaScriptLogicalOp,
        right: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        Ok((
            Expr::JavaScriptLogical {
                left: Box::new(self.lower_expr(left, &path.child(0), scope)?.0),
                op: *op,
                right: Box::new(self.lower_expr(right, &path.child(1), scope)?.0),
            },
            any_binding(),
        ))
    }
}
