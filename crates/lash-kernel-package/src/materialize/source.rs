//! Ordinary lexical function construction and invocation for source dialect ABIs.
use super::*;
use {lash_kernel_value::Param, lash_kernel_value::Signature, lash_kernel_value::Type};

pub(super) fn helper(helpers: &BuildHelpers, name: &str) -> Result<FunctionId, MaterializeRefusal> {
    helpers
        .named_helper(name)
        .ok_or_else(|| MaterializeRefusal::ConstructionHelperMissing {
            name: FunctionName::new(name)
                .unwrap_or_else(|_| unreachable!("qualified construction helper")),
        })
}

pub(super) fn member(name: &Name, index: u32, ts: bool) -> Expr {
    Expr::Member(Box::new(Member::Index {
        target: Expr::Variable(name.clone()),
        index: Expr::Literal(if ts {
            Literal::Float(Float::new(f64::from(index)))
        } else {
            Literal::Int(i64::from(index).into())
        }),
    }))
}

/// Calls use exactly the source language's ordinary value-call ABI.
pub(super) fn call(
    name: &Name,
    args: Vec<Expr>,
    ts: bool,
    py: bool,
    known: bool,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<Expr, MaterializeRefusal> {
    if py {
        return Ok(Expr::Act(Box::new(ActExpr {
            kind: ActKind::Call(Callee::Local(helper(helpers, "py.call")?)),
            args: vec![
                Expr::Variable(name.clone()),
                Expr::List(args),
                Expr::Map(vec![]),
                Expr::Literal(Literal::Absent),
            ],
        })));
    }
    if !ts {
        return Ok(Expr::Act(Box::new(ActExpr {
            kind: ActKind::Call(Callee::Value(name.clone())),
            args,
        })));
    }
    let target = fresh("call_target".into());
    let supplied = fresh("call_arguments".into());
    let callable = fresh("call_body".into());
    Ok(Expr::Let {
        name: target.clone(),
        value: Box::new(Expr::Variable(name.clone())),
        body: Box::new(Expr::Let {
            name: supplied.clone(),
            value: Box::new(Expr::List(args)),
            body: Box::new(Expr::Let {
                name: callable.clone(),
                value: Box::new(if known {
                    member(&target, 4, true)
                } else {
                    Expr::Call {
                        function: helper(helpers, "ts.callable")?,
                        args: vec![Expr::Variable(target.clone())],
                    }
                }),
                body: Box::new(Expr::Act(Box::new(ActExpr {
                    kind: ActKind::Call(Callee::Value(callable)),
                    args: vec![Expr::Literal(Literal::Absent), Expr::Variable(supplied)],
                }))),
            }),
        }),
    })
}

#[derive(Clone, Copy)]
pub(super) enum Abi {
    Kernel,
    TypeScript,
    Python,
}

/// A synthetic function is ordinary source code, with the same packed
/// parameter prologue and token as any authored TypeScript function.
pub(super) fn closure(
    document: &mut Document,
    name: String,
    params: Vec<Param>,
    mut block: Block,
    abi: Abi,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<Expr, MaterializeRefusal> {
    let ts = matches!(abi, Abi::TypeScript);
    let signature = Signature {
        params: params.clone(),
        result: Type::Any,
    };
    let arity = params.len();
    let (private, convention, representation) = if ts {
        let this = fresh("function_this".into());
        let args = fresh("function_arguments".into());
        if !params.is_empty() {
            let list = fresh("function_supplied".into());
            let count = Expr::Literal(Literal::Float(Float::new(params.len() as f64)));
            let mut prologue = vec![binding(
                list.clone(),
                Expr::If {
                    cond: Box::new(Expr::Call {
                        function: helper(helpers, "num.lt")?,
                        args: vec![
                            Expr::Call {
                                function: helper(helpers, "list.len")?,
                                args: vec![Expr::Variable(args.clone())],
                            },
                            count.clone(),
                        ],
                    }),
                    then: Box::new(Expr::Act(Box::new(ActExpr {
                        kind: ActKind::Call(Callee::Local(helper(helpers, "ts.pad")?)),
                        args: vec![Expr::Variable(args.clone()), count],
                    }))),
                    r#else: Box::new(Expr::Variable(args.clone())),
                },
            )];
            for (index, param) in params.iter().enumerate() {
                prologue.push(binding(
                    param.name.clone(),
                    member(&list, index as u32, true),
                ));
            }
            prologue.append(&mut block);
            block = prologue;
        }
        (
            vec![
                Param {
                    name: this,
                    ty: Type::Any,
                    optional: false,
                },
                Param {
                    name: args,
                    ty: Type::Any,
                    optional: false,
                },
            ],
            StartConvention::TsArgsList {
                await_fn: FunctionTarget::Local(helper(helpers, "ts.entry_result")?),
                same_fn: FunctionTarget::Local(helper(helpers, "same")?),
            },
            FunctionRepresentation::Token {
                tag: "ts.function".into(),
                callable: 4,
            },
        )
    } else if matches!(abi, Abi::Python) {
        let positional = fresh("function_positional".into());
        let keywords = fresh("function_keywords".into());
        let active = fresh("function_active".into());
        let slots = fresh("function_slots".into());
        let metadata = params
            .iter()
            .map(|param| {
                Expr::Tuple(vec![
                    Expr::Literal(Literal::Text(param.name.to_string())),
                    Expr::Literal(Literal::Int(1.into())),
                    Expr::Literal(Literal::Bool(param.optional)),
                ])
            })
            .collect::<Vec<_>>();
        let mut prologue = vec![binding(
            slots.clone(),
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Local(helper(helpers, "py.bind")?)),
                args: vec![
                    Expr::Literal(Literal::Text(name.clone())),
                    Expr::Tuple(metadata),
                    Expr::Variable(positional.clone()),
                    Expr::Variable(keywords.clone()),
                    Expr::Literal(Literal::Bool(false)),
                ],
            })),
        )];
        for (i, param) in params.iter().enumerate() {
            prologue.push(binding(param.name.clone(), member(&slots, i as u32, false)));
        }
        prologue.append(&mut block);
        block = prologue;
        (
            vec![positional, keywords, active]
                .into_iter()
                .map(|name| Param {
                    name,
                    ty: Type::Any,
                    optional: false,
                })
                .collect(),
            StartConvention::PyArgsList {
                positional: arity as u32,
            },
            FunctionRepresentation::Token {
                tag: "py.function".into(),
                callable: 1,
            },
        )
    } else {
        (
            params,
            StartConvention::Direct,
            FunctionRepresentation::Bare,
        )
    };

    if matches!(abi, Abi::Python) {
        declarations(&mut block, &private, document);
    }
    let definition =
        lexical::definition(name.clone(), private, Type::Any, block, document, helpers)?;
    let id = definition.identity();
    document.functions.insert(id, definition);
    let closure = Expr::Closure(Box::new(Closure {
        function: id,
        presentation: Some(CallablePresentation {
            signature: signature.clone(),
            convention,
            representation,
        }),
    }));
    Ok(if ts {
        Expr::Tuple(vec![
            Expr::Literal(Literal::Text("ts.function".into())),
            Expr::Literal(Literal::Text(String::new())),
            Expr::Literal(Literal::Text(name)),
            Expr::Literal(Literal::Float(Float::new(arity as f64))),
            closure,
            Expr::Record(vec![]),
        ])
    } else if matches!(abi, Abi::Python) {
        let callable = fresh("packed_callable".into());
        let token = Expr::Tuple(vec![
            Expr::Literal(Literal::Text("py.function".into())),
            Expr::Variable(callable.clone()),
            Expr::Tuple(
                signature
                    .params
                    .iter()
                    .map(|param| {
                        Expr::Tuple(vec![
                            Expr::Literal(Literal::Text(param.name.to_string())),
                            Expr::Literal(Literal::Int(1.into())),
                            Expr::Literal(Literal::Bool(param.optional)),
                        ])
                    })
                    .collect(),
            ),
            Expr::Literal(Literal::Text(name)),
            Expr::Tuple(vec![
                Expr::Literal(Literal::Text("py.callable".into())),
                Expr::Literal(Literal::Bool(false)),
            ]),
        ]);
        Expr::Let {
            name: callable,
            value: Box::new(closure),
            body: Box::new(token),
        }
    } else {
        closure
    })
}
/// Lift a statement's scoped initialization chain into ordered local
/// declarations. Names in authored lowering are unique within a function;
/// refuse to lift a name mentioned elsewhere in the block.
pub(super) fn declarations(
    block: &mut Block,
    params: &[Param],
    source: &Document,
) -> BTreeMap<Vec<u32>, Vec<u32>> {
    fn map(node: Node<'_>, old: Vec<u32>, new: Vec<u32>, paths: &mut BTreeMap<Vec<u32>, Vec<u32>>) {
        paths.insert(old.clone(), new.clone());
        for (i, child) in (0u32..).zip(node.children()) {
            let mut a = old.clone();
            a.push(i);
            let mut b = new.clone();
            b.push(i);
            map(child, a, b, paths);
        }
    }
    let original = std::mem::take(block);
    let names: Vec<BTreeSet<Name>> = original
        .iter()
        .map(|s| {
            let mut names = BTreeSet::new();
            collect_names(Node::Stmt(s), &mut names);
            let mut pending: Vec<_> = Node::Stmt(s).functions_within().into_iter().collect();
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if seen.insert(id)
                    && let Some(body) = source.functions.get(&id).and_then(|f| f.body())
                {
                    collect_names(Node::Block(&body.block), &mut names);
                    pending.extend(Node::Block(&body.block).functions_within());
                }
            }
            names
        })
        .collect();
    let mut paths = BTreeMap::new();
    paths.insert(vec![], vec![]);
    for (i, mut statement) in original.into_iter().enumerate() {
        let old_statement = statement.clone();
        let expression = match &mut statement {
            Stmt::Return { value } | Stmt::ReturnVia { value, .. } => Some(value),
            _ => None,
        };
        let mut old_path = vec![i as u32, 0];
        if let Some(expression) = expression {
            let mut bound = BTreeSet::new();
            let mut cursor: &Expr = expression;
            let mut safe = true;
            while let Expr::Let { name, body, .. } = cursor {
                safe &= bound.insert(name.clone())
                    && !params.iter().any(|p| p.name == *name)
                    && !names
                        .iter()
                        .enumerate()
                        .any(|(index, names)| index != i && names.contains(name));
                cursor = body;
            }
            if !safe {
                map(
                    Node::Stmt(&old_statement),
                    vec![i as u32],
                    vec![block.len() as u32],
                    &mut paths,
                );
                block.push(old_statement);
                continue;
            }
            while let Expr::Let { name, value, body } = expression.clone() {
                let at = block.len() as u32;
                let mut value_path = old_path.clone();
                value_path.push(0);
                map(Node::Expr(&value), value_path, vec![at, 0], &mut paths);
                paths.insert(old_path.clone(), vec![at]);
                block.push(binding(name, *value));
                *expression = *body;
                old_path.push(1);
            }
            map(
                Node::Expr(expression),
                old_path,
                vec![block.len() as u32, 0],
                &mut paths,
            );
            paths.insert(vec![i as u32], vec![block.len() as u32]);
        } else {
            map(
                Node::Stmt(&old_statement),
                vec![i as u32],
                vec![block.len() as u32],
                &mut paths,
            );
        }
        block.push(statement);
    }
    paths
}

/// Authored Python gives a once-bound factory one direct body and a packed
/// adapter, and gives dynamic calls the caller's guest namespace provider.
/// Finish construction once every lexical cell is known, before introducing
/// private adapter bindings that are absent from the guest namespace.
pub(super) fn python_authored(
    document: &mut Document,
    block: &mut Block,
    params: &[Param],
    factories: &BTreeSet<Name>,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<(), MaterializeRefusal> {
    let locals: BTreeSet<_> = params
        .iter()
        .map(|p| p.name.clone())
        .chain(block.iter().filter_map(|stmt| match stmt {
            Stmt::Let { name, .. } => Some(name.clone()),
            _ => None,
        }))
        .collect();
    let provider = lexical::definition(
        "materialized.namespace".into(),
        vec![],
        Type::Any,
        vec![Stmt::Return {
            value: Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Local(helper(helpers, "py.namespace")?)),
                args: vec![Expr::Map(
                    locals
                        .into_iter()
                        .map(|name| MapEntry {
                            key: Expr::Literal(Literal::Text(name.to_string())),
                            value: Expr::Variable(name),
                        })
                        .collect(),
                )],
            })),
        }],
        document,
        helpers,
    )?;
    let provider_id = provider.identity();
    let mut direct = BTreeMap::new();
    let mut authored = Vec::new();
    for mut statement in std::mem::take(block) {
        if let Stmt::Let {
            name,
            value: Rhs::Expr(token),
        } = &mut statement
            && factories.contains(name)
        {
            let (prefix, binding) = python_factory(document, token, helpers, fresh)?;
            authored.extend(prefix);
            direct.insert(name.clone(), binding);
        }
        let expression = match &mut statement {
            Stmt::Let {
                value: Rhs::Expr(value),
                ..
            }
            | Stmt::Do { value }
            | Stmt::Return { value } => Some(value),
            _ => None,
        };
        if let Some(expression) = expression
            && let Expr::Act(call) = expression
            && call.kind == ActKind::Call(Callee::Local(helper(helpers, "py.call")?))
        {
            if let Some(binding) = match call.args.first() {
                Some(Expr::Variable(name)) => direct.get(name),
                _ => None,
            } {
                call.kind = ActKind::Call(Callee::Value(binding.clone()));
                call.args = vec![Expr::Literal(Literal::Absent)];
            } else {
                document.functions.insert(provider_id, provider.clone());
                let target = fresh("call_target".into());
                let namespace = fresh("call_namespace".into());
                let callee = std::mem::replace(&mut call.args[0], Expr::Variable(target.clone()));
                call.args.push(Expr::Variable(namespace.clone()));
                *expression = Expr::Let {
                    name: target,
                    value: Box::new(callee),
                    body: Box::new(Expr::Let {
                        name: namespace,
                        value: Box::new(Expr::Closure(Box::new(Closure {
                            function: provider_id,
                            presentation: None,
                        }))),
                        body: Box::new(expression.clone()),
                    }),
                };
            }
        }
        authored.push(match statement {
            Stmt::Let {
                name,
                value: Rhs::Expr(value),
            } => binding(name, value),
            other => other,
        });
    }
    *block = authored;
    Ok(())
}

fn python_factory(
    document: &mut Document,
    token: &mut Expr,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<(Block, Name), MaterializeRefusal> {
    let invalid = || MaterializeRefusal::PythonFactoryBodyMissing;
    let Expr::Let { value, .. } = token else {
        return Err(invalid());
    };
    let Expr::Closure(closure) = value.as_mut() else {
        return Err(invalid());
    };
    let definition = document
        .functions
        .remove(&closure.function)
        .ok_or_else(invalid)?;
    let active = definition
        .signature
        .params
        .last()
        .cloned()
        .ok_or_else(invalid)?;
    let mut body = definition.body().ok_or_else(invalid)?.block.clone();
    let Some(Stmt::Let {
        name: slots,
        value: Rhs::Expr(mut bind),
    }) = body.first().cloned()
    else {
        return Err(invalid());
    };
    body.remove(0);
    let Expr::Act(call) = &mut bind else {
        return Err(invalid());
    };
    let Some(skip_required) = call.args.last_mut() else {
        return Err(invalid());
    };
    *skip_required = Expr::Literal(Literal::Bool(true));
    let direct = fresh("factory_body".into());
    let raw_name = fresh("factory_closure".into());
    let raw = lexical::definition(
        format!("{}.body", definition.name),
        vec![active.clone()],
        Type::Any,
        body,
        document,
        helpers,
    )?;
    let raw_id = raw.identity();
    document.functions.insert(raw_id, raw);
    let adapter = lexical::definition(
        definition.name.to_string(),
        definition.signature.params,
        Type::Any,
        vec![Stmt::Return {
            value: Expr::Let {
                name: slots,
                value: Box::new(bind),
                body: Box::new(Expr::Act(Box::new(ActExpr {
                    kind: ActKind::Call(Callee::Value(direct.clone())),
                    args: vec![Expr::Variable(active.name)],
                }))),
            },
        }],
        document,
        helpers,
    )?;
    closure.function = adapter.identity();
    document.functions.insert(adapter.identity(), adapter);
    Ok((
        vec![
            binding(
                raw_name.clone(),
                Expr::Closure(Box::new(Closure {
                    function: raw_id,
                    presentation: None,
                })),
            ),
            binding(direct.clone(), Expr::Variable(raw_name)),
        ],
        direct,
    ))
}
