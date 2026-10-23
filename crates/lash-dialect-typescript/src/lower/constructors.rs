//! Constructor lowering and its proven collection result kinds.
use super::{Lowerer, Lowering, Operand, Ty};
use crate::{Diagnostic, DiagnosticCode, adapter as ast};

impl Lowerer<'_> {
    pub(super) fn lower_new(
        &mut self,
        constructor: &ast::Expr,
        args: &[ast::CallArg],
    ) -> Lowering<Operand> {
        let name = match constructor {
            ast::Expr::Ident(name, _) if !self.is_bound(name) => Some(name.as_str()),
            _ => None,
        };
        if let Some(function) = name
            .and_then(|name| self.table.constructors.get(name))
            .copied()
        {
            let args = self.arguments(args)?;
            let ty = match name {
                Some("Array") => Ty::Array,
                Some("Map") => Ty::Map,
                Some("Set") => Ty::Set,
                _ => Ty::Unknown,
            };
            return self.invoke(function, &[Operand::undefined(), args], ty);
        }
        if let Some(name) = name
            && self.table.builtin(name).is_some()
        {
            return Err(Diagnostic::refusal(
                DiagnosticCode::NewUnsupported,
                format!("`new {name}` is outside the supported builtin constructors"),
                self.span,
            ));
        }
        if let Some((root, path)) = super::calls::static_path(constructor)
            && !self.is_bound(root)
            && self.namespaces.is_root(root)
        {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                format!(
                    "`new {path}` is not in the TypeScript dialect: a package value is built by its type's static"
                ),
                self.span,
            ));
        }
        let constructor = self.lower_expr(constructor)?;
        let constructor = self.pin(constructor);
        let args = self.arguments(args)?;
        self.invoke("ts.construct", &[constructor, args], Ty::Unknown)
    }
}
