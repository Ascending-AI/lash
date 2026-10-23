//! Ordinary lexical function construction and invocation for source dialect ABIs.
use super::*;
use lash_vocabulary::{Param, Signature, Type};

pub(super) fn helper(helpers: &BuildHelpers, name: &str) -> Result<FunctionId, MaterializeRefusal> {
    helpers
        .functions()
        .named(name)
        .find(|id| helpers.contains(id))
        .ok_or_else(|| MaterializeRefusal::MissingHelper { name: name.into() })
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
    known: bool,
    helpers: &BuildHelpers,
    fresh: &mut impl FnMut(String) -> Name,
) -> Result<Expr, MaterializeRefusal> {
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
    if matches!(abi, Abi::Python)
        && let Some(last) = params.last()
    {
        let check = Expr::If {
            cond: Box::new(Expr::Call {
                function: helper(helpers, "same")?,
                args: vec![
                    Expr::Variable(last.name.clone()),
                    Expr::Literal(Literal::Absent),
                ],
            }),
            then: Box::new(Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(Callee::Local(helper(helpers, "py.arg_missing")?)),
                args: vec![Expr::Literal(Literal::Text(name.clone()))],
            }))),
            r#else: Box::new(Expr::Literal(Literal::Null)),
        };
        if let Some(Stmt::Assign {
            value: Rhs::Expr(value),
            ..
        }) = block.first_mut()
        {
            *value = Expr::Let {
                name: fresh("saved_argument_check".into()),
                value: Box::new(check),
                body: Box::new(value.clone()),
            };
        }
    }
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
                await_fn: FunctionTarget::Local(helper(helpers, "ts.await")?),
                same_fn: FunctionTarget::Local(helper(helpers, "same")?),
            },
            FunctionRepresentation::Token {
                tag: "ts.function".into(),
                callable: 4,
            },
        )
    } else {
        (
            params,
            StartConvention::Direct,
            FunctionRepresentation::Bare,
        )
    };

    let definition =
        lexical::definition(name.clone(), private, Type::Any, block, document, helpers)?;
    let id = definition.identity();
    document.functions.insert(id, definition);
    let closure = Expr::Closure(Box::new(Closure {
        function: id,
        presentation: Some(CallablePresentation {
            signature,
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

/// Project the exact installed helper closure after adding ordinary ABI glue.
pub(super) fn requirements(
    document: &mut Document,
    helpers: &BuildHelpers,
) -> Result<(), MaterializeRefusal> {
    let mut pending: Vec<_> = document.library.iter().copied().collect();
    let mut seen = BTreeSet::new();
    document.requires.natives.clear();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let header = helpers
            .header(&id)
            .ok_or(MaterializeRefusal::MissingHelper {
                name: id.to_string(),
            })?;
        if header.has_native() {
            document.requires.natives.insert(id);
        }
        document.requires.kernels.insert(header.kernel);
        let (callees, imports) = if let Some(registered) = helpers.registered(&id) {
            (registered.callees().clone(), registered.imports().clone())
        } else if let Some(body) = helpers.definition(&id).and_then(|d| d.body()) {
            (
                body.functions.keys().copied().collect(),
                body.imports.clone(),
            )
        } else {
            (BTreeSet::new(), BTreeMap::new())
        };
        pending.extend(callees);
        for (slot, import) in imports {
            if document
                .imports
                .get(&slot)
                .is_some_and(|previous| *previous != import)
            {
                return Err(MaterializeRefusal::Document {
                    message: format!("incompatible helper import {slot:?}"),
                });
            }
            document.imports.insert(slot, import);
        }
    }
    Ok(())
}
