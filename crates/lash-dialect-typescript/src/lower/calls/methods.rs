//! Named method admission and callable checks preserve the receiver and key.
use super::{Key, Lowerer, Lowering, Operand, Ty, ast, declared_receiver};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};
use lash_kernel_doc::{Callee, Expr};

impl Lowerer<'_> {
    /// Calls a function value: `apply f(this, args)`. A value that may not
    /// be a function's token is made callable by `ts.callable`; a function
    /// declaration nothing assigns to always holds its token, whose closure
    /// is called directly.
    pub(in crate::lower) fn apply(
        &mut self,
        function: Operand,
        this: Operand,
        args: Operand,
        declared: bool,
    ) -> Lowering<Operand> {
        self.apply_checked(function, this, args, declared, None)
    }

    pub(in crate::lower) fn apply_method(
        &mut self,
        function: Operand,
        this: Operand,
        name: Operand,
        args: Operand,
    ) -> Lowering<Operand> {
        self.apply_checked(function, this, args, false, Some(name))
    }

    fn apply_checked(
        &mut self,
        function: Operand,
        this: Operand,
        args: Operand,
        declared: bool,
        method: Option<Operand>,
    ) -> Lowering<Operand> {
        // A call gives what the function's declared return type says.
        let returned = function.ty.returned();
        // A function the source made, held where nothing else is ever
        // assigned, is its token (`types::facts`).
        let declared = declared || matches!(function.ty, Ty::Function(_));
        // Read the callee first, then evaluate arguments, then require a
        // callable. In particular a non-callable callee cannot suppress an
        // argument's effect. Scoped lets keep each value exactly once.
        let target = self.temp();
        let supplied = self.temp();
        let held_function = Operand::temp(target.clone(), function.ty.clone());
        let closure = if declared {
            Self::element(&held_function, 4)
        } else if let Some(method) = method {
            self.invoke(
                "ts.method_callable",
                &[held_function, this.clone(), method],
                Ty::Unknown,
            )?
            .expr()
        } else {
            self.invoke("ts.callable", &[held_function], Ty::Unknown)?
                .expr()
        };
        let name = self.temp();
        let call = self.action(
            lash_kernel_doc::ActKind::Call(Callee::Value(name.clone())),
            vec![this.expr(), Expr::Variable(supplied.clone())],
            returned.clone(),
        );
        Ok(self.inline(
            Expr::Let {
                name: target,
                value: Box::new(function.expr()),
                body: Box::new(Expr::Let {
                    name: supplied,
                    value: Box::new(args.expr()),
                    body: Box::new(Expr::Let {
                        name,
                        value: Box::new(closure),
                        body: Box::new(call.expr()),
                    }),
                }),
            },
            returned,
        ))
    }

    pub(in crate::lower) fn method_refusal(
        &self,
        message: String,
        span: Option<SourceSpan>,
    ) -> Diagnostic {
        Diagnostic::with_refusal_repair(
            DiagnosticCode::MethodUnsupported,
            message,
            "use toLocaleString() or toLocaleString('en-US') with no options; Date uses UTC",
            span,
        )
    }

    pub(in crate::lower) fn refuse_intl(
        &self,
        callee: &ast::Expr,
        span: SourceSpan,
    ) -> Lowering<()> {
        if let Some((root, path)) = super::static_path(callee)
            && root == "Intl"
            && !self.is_bound(root)
        {
            return Err(self.method_refusal(
                format!(
                    "Unsupported: {path}; only fixed en-US toLocaleString formatting is supported"
                ),
                Some(span),
            ));
        }
        Ok(())
    }

    pub(in crate::lower) fn check_method(
        &self,
        object: &Operand,
        key: &Key,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<()> {
        let Key::Static(name) = key else {
            return Ok(());
        };
        let receiver = match &object.ty {
            Ty::Map => super::Receiver::Map,
            Ty::Set => super::Receiver::Set,
            ty => match declared_receiver(ty) {
                Some((receiver, _)) => receiver,
                None => return Ok(()),
            },
        };
        if name == "toLocaleString" {
            let bad_locale = matches!(args.first(), Some(ast::CallArg::Value(ast::Expr::String(locale))) if locale != "en-US");
            let options = matches!(args.get(1), Some(ast::CallArg::Value(ast::Expr::Object(_))));
            if bad_locale || options {
                return Err(self.method_refusal(
                    format!("Unsupported: toLocaleString on {receiver:?}"),
                    Some(span),
                ));
            }
        }
        if !self
            .table
            .methods
            .get(name.as_str())
            .is_some_and(|rows| rows.iter().any(|(kind, _)| *kind == receiver))
        {
            return Err(Diagnostic::with_refusal_repair(
                DiagnosticCode::MethodUnsupported,
                format!("Unsupported: method `{name}` on {receiver:?}"),
                format!("use a method the dialect lists for {receiver:?}"),
                Some(span),
            ));
        }
        Ok(())
    }
}
