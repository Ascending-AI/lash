//! Python argument binding lives with each callable, including escaped and
//! saved functions. A single packed closure accepts the argument stream;
//! its presentation retains the finite public signature for typed crossings.
use super::{Buf, Lowerer, Lowering, Operand};
use crate::scope::Ty;
use lash_kernel_doc::{
    CallablePresentation, Expr, FunctionRepresentation, Literal, Member, Rhs, Stmt,
};
use lash_vocabulary::Name;
use ruff_python_ast as ast;

pub(super) struct CallableOptions {
    pub(super) is_async: bool,
    pub(super) skip_required_check: bool,
}

impl Lowerer<'_> {
    pub(super) fn python_callable(
        &mut self,
        parameters: Option<&ast::Parameters>,
        params: Vec<Name>,
        body: Buf,
        start: Option<CallablePresentation>,
        name: &str,
        options: CallableOptions,
    ) -> Lowering<Operand> {
        let mut metadata = Vec::new();
        if let Some(p) = parameters {
            for (kind, group) in [
                (0, p.posonlyargs.as_slice()),
                (1, p.args.as_slice()),
                (2, p.kwonlyargs.as_slice()),
            ] {
                for param in group {
                    metadata.push(Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(param.parameter.name.id.to_string())),
                        Expr::Literal(Literal::Int(kind.into())),
                        Expr::Literal(Literal::Bool(param.default.is_some())),
                    ]));
                }
            }
            for (kind, param) in [(3, p.vararg.as_ref()), (4, p.kwarg.as_ref())] {
                if let Some(param) = param {
                    metadata.push(Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(param.name.id.to_string())),
                        Expr::Literal(Literal::Int(kind.into())),
                        Expr::Literal(Literal::Bool(false)),
                    ]));
                }
            }
        }
        let variadic = parameters.is_some_and(|p| p.vararg.is_some() || p.kwarg.is_some());
        let mut token = self.wrap_python_callable(
            params,
            body,
            metadata,
            if variadic { None } else { start },
            name,
            options.skip_required_check,
        )?;
        if let Expr::Tuple(parts) = &mut token.expr {
            parts[4] = Expr::Tuple(vec![
                Expr::Literal(Literal::Text("py.callable".into())),
                Expr::Literal(Literal::Bool(options.is_async)),
            ]);
        }
        Ok(token)
    }

    pub(super) fn wrap_python_callable(
        &mut self,
        params: Vec<Name>,
        body: Buf,
        metadata: Vec<Expr>,
        mut start: Option<CallablePresentation>,
        name: &str,
        skip_required_check: bool,
    ) -> Lowering<Operand> {
        let positional = self.fresh("positional");
        let keywords = self.fresh("keywords");
        let exception = if params.len() > metadata.len() {
            params
                .last()
                .cloned()
                .unwrap_or_else(|| self.fresh("exception"))
        } else {
            self.fresh("exception")
        };
        let packed_body = self.block(|this| {
            let slots = this.invoke(
                "py.bind",
                &[
                    Operand::text(name),
                    Operand::inline(Expr::Tuple(metadata.clone()), Ty::Tuple),
                    Operand::temp(positional.clone(), Ty::List),
                    Operand::temp(keywords.clone(), Ty::Dict),
                    Operand::literal(Literal::Bool(skip_required_check), Ty::Bool),
                ],
                Ty::List,
            )?;
            let slots = this.pin(slots);
            for (i, param) in params.iter().take(metadata.len()).enumerate() {
                this.emit(Stmt::Let {
                    name: param.clone(),
                    value: Rhs::Expr(Expr::Member(Box::new(Member::Index {
                        target: slots.expr.clone(),
                        index: Expr::Literal(Literal::Int(
                            i64::try_from(i).unwrap_or(i64::MAX).into(),
                        )),
                    }))),
                });
            }
            this.buf.stmts.extend(body.stmts);
            this.buf.notes.extend(body.notes);
            Ok(())
        })?;
        if let Some(start) = &mut start {
            start.representation = FunctionRepresentation::Token {
                tag: "py.function".into(),
                callable: 1,
            };
        }
        if let Some(start) = &mut start {
            let positional = metadata.iter().take_while(|param| matches!(param, Expr::Tuple(parts) if matches!(&parts[1], Expr::Literal(Literal::Int(mode)) if (mode == &lash_kernel_doc::Integer::from(0) || mode == &lash_kernel_doc::Integer::from(1))))).count();
            start.convention = lash_kernel_doc::StartConvention::PyArgsList {
                positional: u32::try_from(positional).unwrap_or(u32::MAX),
            };
        }
        let packed = self.emit_closure(vec![positional, keywords, exception], packed_body, start);
        Ok(Operand::inline(
            Expr::Tuple(vec![
                Expr::Literal(Literal::Text("py.function".into())),
                packed.expr,
                Expr::Tuple(metadata),
                Expr::Literal(Literal::Text(name.into())),
                Expr::Literal(Literal::Null),
            ]),
            Ty::Unknown,
        ))
    }

    /// Expand before evaluating the next source operand: failed expansion
    /// must stop later operands, and a repeated keyword must stop later **.
    pub(super) fn packed_arguments(
        &mut self,
        arguments: &ast::Arguments,
    ) -> Lowering<(Operand, Operand)> {
        if !arguments.args.iter().any(ast::Expr::is_starred_expr)
            && arguments
                .keywords
                .iter()
                .all(|keyword| keyword.arg.is_some())
        {
            let given = self.given(None, arguments)?;
            let positional = Operand::inline(
                Expr::List(given.positional.into_iter().map(|v| v.expr).collect()),
                Ty::List,
            );
            let keywords = Operand::inline(
                Expr::Map(
                    arguments
                        .keywords
                        .iter()
                        .zip(given.keywords)
                        .map(|(kw, v)| {
                            let Some(name) = &kw.arg else {
                                unreachable!("expanded keywords use the ordered builder");
                            };
                            lash_kernel_doc::MapEntry {
                                key: Expr::Literal(Literal::Text(name.id.to_string())),
                                value: v.expr,
                            }
                        })
                        .collect(),
                ),
                Ty::Dict,
            );
            return Ok((positional, keywords));
        }
        let positional = self.let_rhs(Rhs::Expr(Expr::List(Vec::new())), Ty::List);
        for arg in &arguments.args {
            let (value, expand) = match arg {
                ast::Expr::Starred(starred) => (self.expr(&starred.value)?, true),
                arg => (self.expr(arg)?, false),
            };
            if expand {
                self.invoke_do("py.args.extend", &[positional.clone(), value])?;
            } else {
                self.invoke_do("py.args.append", &[positional.clone(), value])?;
            }
        }
        let keywords = self.let_rhs(Rhs::Expr(Expr::Map(Vec::new())), Ty::Dict);
        for keyword in &arguments.keywords {
            let value = self.expr(&keyword.value)?;
            if let Some(name) = &keyword.arg {
                self.invoke_do(
                    "py.kw.put",
                    &[keywords.clone(), Operand::text(name.id.as_str()), value],
                )?;
            } else {
                self.invoke_do("py.kw.extend", &[keywords.clone(), value])?;
            }
        }
        Ok((positional, keywords))
    }
}

impl Lowerer<'_> {
    pub(super) fn exception_value(&mut self, name: &str) -> Lowering<Operand> {
        let args = self.fresh("args");
        let body = self.block(|this| {
            let value = this.raised(
                name,
                Operand::temp(args.clone(), Ty::Tuple),
                Operand::none(),
            )?;
            this.return_value(value);
            Ok(())
        })?;
        let metadata = vec![Expr::Tuple(vec![
            Expr::Literal(Literal::Text("args".into())),
            Expr::Literal(Literal::Int(3.into())),
            Expr::Literal(Literal::Bool(false)),
        ])];
        let mut token = self.wrap_python_callable(vec![args], body, metadata, None, name, false)?;
        let (mode, names) = match self.classes.catches(&[name.to_string()]) {
            crate::exceptions::Catches::Everything => ("all", Vec::new()),
            crate::exceptions::Catches::Exceptions(names) => ("exception", names),
            crate::exceptions::Catches::Named(names) => ("named", names),
        };
        if let Expr::Tuple(parts) = &mut token.expr {
            parts[4] = Expr::Tuple(vec![
                Expr::Literal(Literal::Text("py.exception".into())),
                Expr::Literal(Literal::Text(mode.into())),
                Expr::Tuple(
                    names
                        .into_iter()
                        .map(|name| Expr::Literal(Literal::Text(name)))
                        .collect(),
                ),
            ]);
        }
        Ok(token)
    }
}

/// Installed helpers use the same argument metadata and binder as authored defs.
pub(crate) fn helper_value(name: &str, params: &[(&str, u8, bool)], body: &str) -> String {
    let metadata = params
        .iter()
        .map(|(name, mode, optional)| format!("({name:?}, {mode}, {optional})"))
        .collect::<Vec<_>>()
        .join(", ");
    let metadata = format!("[{metadata}]");
    let bindings = params
        .iter()
        .enumerate()
        .map(|(index, _)| format!("let argument{index} = slots[{index}]\n"))
        .collect::<String>();
    format!(
        "(\"py.function\", fn(args, kwargs, active) {{ let slots = invoke py.bind({name:?}, {metadata}, args, kwargs, false)\n{bindings}{body} }}, {metadata}, {name:?}, null)"
    )
}
