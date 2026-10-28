//! Escaping and saved functions retain their argument binder in a packed
//! closure. Proven local positional calls can enter a direct body; callable
//! presentations retain the finite public signature for typed crossings.
use super::{Buf, Lowerer, Lowering, Operand};
use crate::scope::Ty;
use lash_kernel_doc::{
    CallablePresentation, Expr, FunctionRepresentation, Literal, Member, Rhs, Stmt,
};
use lash_kernel_value::Name;
use ruff_python_ast as ast;

pub(super) struct CallableOptions {
    pub(super) is_async: bool,
    pub(super) skip_required_check: bool,
}

fn fixed_parameters(metadata: &[Expr]) -> Option<Vec<bool>> {
    metadata
        .iter()
        .map(|param| {
            let Expr::Tuple(parts) = param else {
                return None;
            };
            let [
                Expr::Literal(Literal::Text(_)),
                Expr::Literal(Literal::Int(mode)),
                Expr::Literal(Literal::Bool(default)),
            ] = parts.as_slice()
            else {
                return None;
            };
            matches!(mode.word()?, 0 | 1).then_some(*default)
        })
        .collect()
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
            parts[crate::constructs::callables::CATEGORY] = Expr::Tuple(vec![
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
        let expression_body =
            body.len() == 1 && matches!(body.last_stmt(), Some(Stmt::Return { .. }));
        let mut packed_body = self.block(|this| {
            if expression_body
                && !skip_required_check
                && let Some(fixed) = fixed_parameters(&metadata)
                && let Some(Stmt::Return { value }) = body.last_stmt()
            {
                let mut fast_value = value.clone();
                for (i, (param, fixed)) in params.iter().zip(&fixed).enumerate().rev() {
                    if !*fixed && !skip_required_check {
                        let missing = this.native(
                            "same",
                            vec![
                                Expr::Variable(param.clone()),
                                Expr::Literal(Literal::Absent),
                            ],
                        )?;
                        let failure = this.invoke(
                            "py.fail",
                            &[
                                Operand::text("TypeError"),
                                Operand::text(format!(
                                    "{name}() missing a required positional argument"
                                )),
                            ],
                            Ty::Unknown,
                        )?;
                        fast_value = Expr::If {
                            cond: Box::new(missing),
                            then: Box::new(failure.expr),
                            r#else: Box::new(fast_value),
                        };
                    }
                    fast_value = Expr::Let {
                        name: param.clone(),
                        value: Box::new(Expr::Member(Box::new(Member::Index {
                            target: Expr::Variable(positional.clone()),
                            index: Expr::Literal(Literal::Int(
                                i64::try_from(i).unwrap_or(i64::MAX).into(),
                            )),
                        }))),
                        body: Box::new(fast_value),
                    };
                }
                if let Some(fast_value) = this.branch_expression(&Buf::default(), fast_value) {
                    let count =
                        this.native("list.len", vec![Expr::Variable(positional.clone())])?;
                    let exact = this.native(
                        "same",
                        vec![
                            count,
                            Expr::Literal(Literal::Int(
                                i64::try_from(fixed.len()).unwrap_or(i64::MAX).into(),
                            )),
                        ],
                    )?;
                    let keyword_count =
                        this.native("map.len", vec![Expr::Variable(keywords.clone())])?;
                    let empty = this.native(
                        "same",
                        vec![keyword_count, Expr::Literal(Literal::Int(0.into()))],
                    )?;
                    let condition = Expr::If {
                        cond: Box::new(exact),
                        then: Box::new(empty),
                        r#else: Box::new(Expr::Literal(Literal::Bool(false))),
                    };
                    this.emit_if(
                        condition,
                        Buf::one(
                            Stmt::Return { value: fast_value },
                            body.notes().next().cloned().unwrap_or_default(),
                        ),
                        Buf::default(),
                    );
                }
            }
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
            this.buf.extend(body);
            Ok(())
        })?;
        if expression_body && let Some(Stmt::Return { value }) = packed_body.last_stmt().cloned() {
            let (statements, mut notes) = packed_body.clone().into_parts();
            let fast = match statements.first() {
                Some(Stmt::If {
                    condition,
                    then_block,
                    else_block,
                }) if else_block.is_empty() => match then_block.as_slice() {
                    [Stmt::Return { value }] => Some((condition.clone(), value.clone())),
                    _ => None,
                },
                _ => None,
            };
            let prefix = Buf::from_parts(
                statements[usize::from(fast.is_some())..statements.len() - 1].to_vec(),
                notes[usize::from(fast.is_some())..notes.len() - 1].to_vec(),
            );
            if let Some(value) = self.branch_expression(&prefix, value)
                && let Some(value) = self.branch_expression(
                    &Buf::default(),
                    match fast {
                        Some((condition, fast)) => Expr::If {
                            cond: Box::new(condition),
                            then: Box::new(fast),
                            r#else: Box::new(value),
                        },
                        None => value,
                    },
                )
            {
                let note = notes.pop().unwrap_or_default();
                packed_body = Buf::one(Stmt::Return { value }, note);
            }
        }

        if let Some(start) = &mut start {
            start.representation = FunctionRepresentation::Token {
                tag: crate::constructs::callables::TAG.into(),
                callable: crate::constructs::callables::CALLABLE as u32,
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
            crate::constructs::callables::token(packed.expr, metadata, name),
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
            parts[crate::constructs::callables::CATEGORY] = Expr::Tuple(vec![
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
