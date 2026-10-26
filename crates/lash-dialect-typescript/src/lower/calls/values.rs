//! Global builtin values and their authored forwarding closures.
use super::*;
use lash_kernel_doc::Rhs;

impl Lowerer<'_> {
    /// A read of a global path: its value row, the token of the built-in
    /// it names (`builtins/mod.rs`), or `globalThis.name` as the name
    /// itself. `None` when no row knows the path, which is then read as a
    /// member of the value before it.
    pub(in crate::lower) fn read_global(
        &mut self,
        path: &str,
        span: Option<SourceSpan>,
    ) -> Option<Lowering<Operand>> {
        match path {
            "undefined" => return Some(Ok(Operand::undefined())),
            "NaN" => return Some(Ok(Operand::number(f64::NAN))),
            "Infinity" => return Some(Ok(Operand::number(f64::INFINITY))),
            _ => {}
        }
        if path == "Intl" || path.starts_with("Intl.") {
            return Some(Err(self.method_refusal(
                format!(
                    "Unsupported: {path}; only fixed en-US toLocaleString formatting is supported"
                ),
                span.or(self.span),
            )));
        }
        if let Some(name) = path.strip_prefix("globalThis.")
            && !name.contains('.')
        {
            if lash_ext_ts_helpers::builtins::is_global(name) {
                return self.read_global(name, span);
            }
            return None;
        }
        if let Some(function) = self.table.values.get(path).copied() {
            return Some(self.invoke(function, &[], Ty::Unknown));
        }
        if let Some(token) = self.token(path) {
            return Some(token);
        }
        (!path.contains('.') && lash_ext_ts_helpers::builtins::is_global(path)).then(|| {
            Err(Diagnostic {
                kind: DiagnosticKind::Refusal,
                ..Diagnostic::with_repair(
                    DiagnosticCode::UnsupportedExpression,
                    format!("Unsupported: `{path}` as a value"),
                    format!("call a function of `{path}` or read one of its constants"),
                    span,
                )
            })
        })
    }

    /// The token a built-in path is as a value (`builtins/mod.rs`), with
    /// the closure that calls it when it is a function.
    pub(super) fn token(&mut self, path: &str) -> Option<Lowering<Operand>> {
        let path = lash_ext_ts_helpers::builtins::canonical(path);
        let builtin = self.table.builtin(path)?;
        Some(self.make_token(path, builtin))
    }

    fn make_token(
        &mut self,
        path: &str,
        builtin: lash_ext_ts_helpers::builtins::Builtin,
    ) -> Lowering<Operand> {
        let text = |value: &str| Expr::Literal(Literal::Text(value.to_string()));
        let items = match builtin {
            lash_ext_ts_helpers::builtins::Builtin::Function { call, length } => {
                let (helper, class) = call.callee(path);
                let function = self.helper_callee(helper)?;
                let this = self.fresh("this");
                let args = self.fresh("args");
                let body = self.block(|lowerer| {
                    let passed = match class {
                        Some(class) => vec![Expr::Literal(Literal::Text(class.to_string()))],
                        None => vec![Expr::Variable(this.clone()), Expr::Variable(args.clone())],
                    };
                    let result = lowerer.action(
                        lash_kernel_doc::ActKind::Call(function.clone()),
                        passed,
                        Ty::Unknown,
                    );
                    lowerer.emit(Stmt::Return {
                        value: result.expr(),
                    });
                    Ok(())
                })?;
                let call = self.emit_closure(vec![this, args], body, None);
                let Some((Rhs::Expr(call), _)) = self.take_binding_of(&call) else {
                    unreachable!("a builtin's callable is a fresh closure");
                };
                vec![
                    text(crate::FUNCTION_TAG),
                    text(path),
                    text(lash_ext_ts_helpers::builtins::function_name(path)),
                    Expr::Literal(Literal::Float(lash_kernel_doc::Float::new(f64::from(
                        length,
                    )))),
                    call,
                ]
            }
            lash_ext_ts_helpers::builtins::Builtin::Object => vec![
                text("ts.object"),
                text(path),
                text(lash_ext_ts_helpers::builtins::object_tag(path)),
            ],
        };
        Ok(self.inline(Expr::Tuple(items), Ty::Builtin))
    }
}
