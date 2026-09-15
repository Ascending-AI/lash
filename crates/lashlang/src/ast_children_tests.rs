//! `Expr::children` / `Expr::children_mut` parity (FIG-3118).
//!
//! The two walks are hand-written matches over the same enum, and the workflow
//! lens splices rendered process bodies through `children_mut`, so a walk that
//! disagrees with `children` is a silently wrong edit rather than a compile
//! error. These tests pin the agreement over a corpus that reaches every
//! variant.

use super::*;

fn var(name: &str) -> Expr {
    Expr::Variable(name.into())
}

/// One expression per `Expr` variant, each composite carrying distinct
/// `Variable` children so a walk's order is readable off the names.
///
/// The `children` / `children_mut` pair is two hand-written matches over
/// the same enum, and the splice the workflow lens runs over `children_mut`
/// is silently wrong wherever they disagree, so the parity test below needs
/// a corpus that reaches every arm. A new variant added to `Expr` makes
/// both matches fail to compile until it is handled; adding it here is what
/// then proves the two handlings agree.
fn every_expr_variant() -> Vec<Expr> {
    let function = || {
        Box::new(FunctionExpr {
            name: None,
            params: vec!["p".into()],
            captures: Vec::new(),
            body: Box::new(var("fn_body")),
        })
    };
    vec![
        Expr::Null,
        Expr::Undefined,
        Expr::Bool(true),
        Expr::Number(1.0),
        Expr::String("s".into()),
        var("leaf"),
        Expr::Break,
        Expr::Continue,
        Expr::WaitSignal {
            name: "ready".into(),
        },
        Expr::ProcessRef {
            process: "proc".into(),
        },
        Expr::ResourceRef(ResourceRefExpr::unresolved(vec!["display".into()])),
        Expr::TypeLiteral(Box::new(TypeExpr::Str)),
        Expr::Block(vec![var("b0"), var("b1")]),
        Expr::Tuple(vec![var("t0"), var("t1")]),
        Expr::List(vec![var("l0"), var("l1")]),
        Expr::ListComprehension {
            element: Box::new(var("element")),
            clauses: vec![
                ListComprehensionClause::For {
                    binding: "item".into(),
                    iterable: var("iterable"),
                },
                ListComprehensionClause::If {
                    condition: var("condition"),
                },
            ],
        },
        Expr::LabelAnnotated {
            label: LabelMetadata {
                title: "title".into(),
                description: None,
            },
            expr: Box::new(var("labelled")),
        },
        Expr::Record(vec![("k0".into(), var("r0")), ("k1".into(), var("r1"))]),
        Expr::Assign {
            target: AssignTarget {
                root: "root".into(),
                steps: vec![
                    AssignPathStep::Field("field".into()),
                    AssignPathStep::Index(var("index")),
                ],
            },
            expr: Box::new(var("assigned")),
        },
        Expr::If {
            condition: Box::new(var("cond")),
            then_block: Box::new(var("then")),
            else_block: Box::new(var("else")),
        },
        Expr::For {
            binding: "item".into(),
            iterable: Box::new(var("for_iterable")),
            body: Box::new(var("for_body")),
        },
        Expr::While {
            condition: Box::new(var("while_cond")),
            body: Box::new(var("while_body")),
        },
        Expr::HostDescriptorConstructor {
            type_name: "Sleep".into(),
            input: Box::new(var("descriptor_input")),
        },
        Expr::ReceiverCall {
            receiver: Box::new(var("receiver")),
            operation: "op".into(),
            args: vec![var("recv_arg")],
        },
        Expr::Await(Box::new(var("awaited"))),
        Expr::SleepFor(Box::new(var("sleep_for"))),
        Expr::SleepUntil(Box::new(var("sleep_until"))),
        Expr::ResultUnwrap(Box::new(var("unwrapped"))),
        Expr::Print(Box::new(var("printed"))),
        Expr::Yield(Box::new(var("yielded"))),
        Expr::Fail(Box::new(var("failed"))),
        Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(var("unary")),
        },
        Expr::JavaScriptUnary {
            op: JavaScriptUnaryOp::TypeOf,
            expr: Box::new(var("js_unary")),
        },
        Expr::Return(Box::new(var("returned"))),
        Expr::Finish(Box::new(var("finished"))),
        Expr::BuiltinCall {
            name: "len".into(),
            args: vec![var("builtin_arg")],
        },
        Expr::FunctionCall {
            function: "f".into(),
            args: vec![var("call_arg")],
        },
        Expr::Function(function()),
        Expr::ProcessLiteral(Box::new(ProcessLiteralExpr {
            params: Vec::new(),
            hidden_args: Vec::new(),
            body: Box::new(var("literal_body")),
        })),
        Expr::Call {
            function: Box::new(var("callee")),
            args: vec![var("arg")],
        },
        Expr::Map {
            items: Box::new(var("items")),
            function: Box::new(var("mapper")),
        },
        Expr::Try(Box::new(TryExpr {
            body: Box::new(var("try_body")),
            catch: Some(CatchClause {
                binding: "err".into(),
                body: Box::new(var("catch_body")),
            }),
            finally: Some(Box::new(var("finally_body"))),
        })),
        Expr::Try(Box::new(TryExpr {
            body: Box::new(var("bare_try_body")),
            catch: None,
            finally: None,
        })),
        Expr::Throw(Box::new(var("thrown"))),
        Expr::Field {
            target: Box::new(var("field_target")),
            field: "f".into(),
        },
        Expr::Index {
            target: Box::new(var("index_target")),
            index: Box::new(var("index_index")),
        },
        Expr::Binary {
            left: Box::new(var("bin_left")),
            op: BinaryOp::Add,
            right: Box::new(var("bin_right")),
        },
        Expr::JavaScriptBinary {
            left: Box::new(var("js_bin_left")),
            op: JavaScriptBinaryOp::StrictEqual,
            right: Box::new(var("js_bin_right")),
        },
        Expr::JavaScriptLogical {
            left: Box::new(var("js_log_left")),
            op: JavaScriptLogicalOp::NullishCoalesce,
            right: Box::new(var("js_log_right")),
        },
    ]
}

/// Every node reachable from `expr`, in `children` pre-order.
fn walk_shared(expr: &Expr, into: &mut Vec<Expr>) {
    into.push(expr.clone());
    for child in expr.children() {
        walk_shared(child, into);
    }
}

/// Every node reachable from `expr`, in `children_mut` pre-order.
fn walk_mut(expr: &mut Expr, into: &mut Vec<Expr>) {
    into.push(expr.clone());
    for child in expr.children_mut() {
        walk_mut(child, into);
    }
}

#[test]
fn children_mut_visits_the_same_nodes_as_children() {
    for expr in every_expr_variant() {
        let mut mutable = expr.clone();
        let shared: Vec<&Expr> = expr.children().collect();
        let visited: Vec<Expr> = mutable.children_mut().map(|child| child.clone()).collect();
        assert_eq!(
            shared.len(),
            visited.len(),
            "child count differs for {expr:?}"
        );
        assert_eq!(
            shared.into_iter().cloned().collect::<Vec<_>>(),
            visited,
            "child order differs for {expr:?}"
        );
    }
}

#[test]
fn children_mut_walks_a_whole_program_in_the_same_order_as_children() {
    // The whole corpus as one tree, so the parity holds recursively and
    // not only one level down: the lens's splice recurses.
    let mut program = Expr::Block(every_expr_variant());
    let mut shared = Vec::new();
    walk_shared(&program, &mut shared);
    let mut mutable = Vec::new();
    walk_mut(&mut program, &mut mutable);
    assert_eq!(shared.len(), mutable.len());
    assert_eq!(shared, mutable);
}

#[test]
fn children_mut_edits_reach_the_expression() {
    let mut expr = Expr::If {
        condition: Box::new(var("cond")),
        then_block: Box::new(var("then")),
        else_block: Box::new(var("else")),
    };
    for child in expr.children_mut() {
        *child = Expr::Null;
    }
    assert_eq!(
        expr,
        Expr::If {
            condition: Box::new(Expr::Null),
            then_block: Box::new(Expr::Null),
            else_block: Box::new(Expr::Null),
        }
    );
}

#[test]
fn children_mut_size_hint_is_exact() {
    let mut block = Expr::Block(vec![var("a"), var("b"), var("c"), var("d")]);
    let iter = block.children_mut();
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.size_hint(), (4, Some(4)));
}
