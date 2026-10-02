use super::*;

impl<'module> Linker<'module> {
    pub(super) fn lower_javascript_unary(
        &self,
        op: &crate::ast::CoercingUnaryOp,
        expr: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let ty = match op {
            crate::ast::CoercingUnaryOp::Not => TypeExpr::Bool,
            crate::ast::CoercingUnaryOp::TypeOf | crate::ast::CoercingUnaryOp::ToString => {
                TypeExpr::Str
            }
            crate::ast::CoercingUnaryOp::Plus
            | crate::ast::CoercingUnaryOp::Negate
            | crate::ast::CoercingUnaryOp::BitNot => TypeExpr::Float,
        };
        Ok((
            Expr::CoercingUnary {
                op: *op,
                expr: Box::new(self.lower_expr(expr, &path.child(0), scope)?.0),
            },
            Binding::Value(ty),
        ))
    }

    pub(super) fn lower_javascript_binary(
        &self,
        left: &Expr,
        op: &crate::ast::CoercingBinaryOp,
        right: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let ty = match op {
            crate::ast::CoercingBinaryOp::StrictEqual
            | crate::ast::CoercingBinaryOp::StrictNotEqual
            | crate::ast::CoercingBinaryOp::LooseEqual
            | crate::ast::CoercingBinaryOp::LooseNotEqual
            | crate::ast::CoercingBinaryOp::Less
            | crate::ast::CoercingBinaryOp::LessEqual
            | crate::ast::CoercingBinaryOp::Greater
            | crate::ast::CoercingBinaryOp::GreaterEqual => TypeExpr::Bool,
            _ => TypeExpr::Any,
        };
        Ok((
            Expr::CoercingBinary {
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
        op: &crate::ast::OperandLogicalOp,
        right: &Expr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        Ok((
            Expr::OperandLogical {
                left: Box::new(self.lower_expr(left, &path.child(0), scope)?.0),
                op: *op,
                right: Box::new(self.lower_expr(right, &path.child(1), scope)?.0),
            },
            any_binding(),
        ))
    }
}
