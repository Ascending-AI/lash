//! G-SOURCE-READABLE: native syntax has a direct exact inverse, never authored lowering.
use super::{k, roundtrip};
fn variable(name: &str) -> k::Expr {
    k::Expr::Variable(name.into())
}
fn document(stmt: k::Stmt) -> k::Document {
    k::Document::new(k::NumberPolicy::BySpelling, vec![stmt])
}
#[test]
fn readable_if_has_a_direct_exact_inverse() {
    let doc = document(k::Stmt::If {
        condition: variable("ready"),
        then_block: vec![k::Stmt::Break],
        else_block: vec![],
    });
    let printed = roundtrip(&doc, true);
    assert!(
        printed
            .source
            .contains("    if ready:\n        break\n    else:\n        pass\n"),
        "{}",
        printed.source
    );
}
fn closure() -> k::Expr {
    k::Expr::Closure(Box::new(k::Closure {
        params: vec![],
        body: vec![],
    }))
}
fn int(n: i64) -> k::Expr {
    k::Expr::Literal(k::Literal::Int(n.into()))
}
fn finish(value: k::Expr) -> k::Stmt {
    k::Stmt::Finish { value }
}
fn field(target: k::Expr, field: &str) -> k::Expr {
    k::Expr::Member(Box::new(k::Member::Field {
        target,
        field: field.into(),
    }))
}
fn index(target: k::Expr, index: k::Expr) -> k::Expr {
    k::Expr::Member(Box::new(k::Member::Index { target, index }))
}
fn spelling(stmt: k::Stmt, expected: &str) {
    let printed = roundtrip(&document(stmt), false);
    assert!(
        printed.source.contains(expected),
        "expected {expected:?}\n{}",
        printed.source
    );
}
/// Paired witnesses for every conditional readable rule in the adjacent inventory.
#[test]
fn paired_readable_rules_pin_positive_and_near_miss_spellings() {
    // Naming and reserved collisions use the landed NamePlan, including shadowing.
    for (name, expected) in [("λ", "return λ"), ("return", "return kernel_name_1")] {
        spelling(finish(variable(name)), expected);
    }
    spelling(
        finish(field(variable("object_value"), "title")),
        "return object_value.title",
    );
    spelling(
        finish(field(variable("object_value"), "return")),
        "return getattr(object_value, \"return\")",
    );
    spelling(
        finish(field(closure(), "title")),
        "with k.field(\"title\"):",
    );
    spelling(
        finish(index(variable("items"), int(-1))),
        "return items[-1]",
    );
    spelling(
        finish(index(variable("items"), closure())),
        "with k.index():",
    );
    // Headers require inline expressions; generic suites keep arbitrary children.
    for (expr, native) in [(variable("ready"), true), (closure(), false)] {
        spelling(
            k::Stmt::If {
                condition: expr.clone(),
                then_block: vec![],
                else_block: vec![],
            },
            if native {
                "if ready:\n"
            } else {
                "with k.if_():"
            },
        );
        spelling(
            k::Stmt::For {
                binding: "item".into(),
                iterable: expr.clone(),
                body: vec![],
            },
            if native {
                "for item in ready:\n"
            } else {
                "with k.for_(item):"
            },
        );
        spelling(
            k::Stmt::While {
                condition: expr.clone(),
                body: vec![],
            },
            if native {
                "while ready:\n"
            } else {
                "with k.while_():"
            },
        );
        spelling(
            k::Stmt::Return {
                value: expr.clone(),
            },
            if native {
                "return returned(ready)\n"
            } else {
                "with k.return_():"
            },
        );
        spelling(
            k::Stmt::Throw {
                value: expr.clone(),
            },
            if native {
                "raise ready\n"
            } else {
                "with k.throw():"
            },
        );
        spelling(
            k::Stmt::Print {
                value: expr.clone(),
            },
            if native {
                "print(ready)"
            } else {
                "with k.print():"
            },
        );
        spelling(
            k::Stmt::Finish {
                value: expr.clone(),
            },
            if native {
                "return ready"
            } else {
                "with k.finish():"
            },
        );
        spelling(
            k::Stmt::Fail { value: expr },
            if native {
                "raise failure(ready)"
            } else {
                "with k.fail():"
            },
        );
    }
    for (handlers, expected) in [(true, "try:\n"), (false, "with k.try_(None, False):")] {
        spelling(
            k::Stmt::Try(k::TryStmt {
                body: vec![],
                catch: None,
                finally: handlers.then(Vec::new),
            }),
            expected,
        );
    }
    let nested = k::Stmt::If {
        condition: variable("second"),
        then_block: vec![],
        else_block: vec![],
    };
    spelling(
        k::Stmt::If {
            condition: variable("first"),
            then_block: vec![],
            else_block: vec![nested.clone()],
        },
        "elif second:\n",
    );
    spelling(
        k::Stmt::If {
            condition: variable("first"),
            then_block: vec![],
            else_block: vec![nested, k::Stmt::Continue],
        },
        "else:\n        if second:\n",
    );
    let id = k::FunctionId::from_bytes([31; 32]);
    // Helper identity is in the base document once; labels do not drive the reader.
    spelling(
        finish(k::Expr::Call {
            function: id,
            args: vec![int(3), variable("λ")],
        }),
        "return helper_function(3, λ)",
    );
    spelling(
        finish(k::Expr::Call {
            function: id,
            args: vec![closure()],
        }),
        "with k.invoke(helper_function):",
    );
    for spawn in [false, true] {
        for library in [false, true] {
            let callee = if library {
                k::Callee::Library(id)
            } else {
                k::Callee::Value("helper_function".into())
            };
            let args = vec![k::Atom::Literal(k::Literal::Int(3.into()))];
            let action = if spawn {
                k::Action::Spawn { callee, args }
            } else {
                k::Action::Call { callee, args }
            };
            let op = if spawn { "spawn" } else { "call" };
            let expected = if !spawn {
                if library {
                    "await helper_function(3)".to_owned()
                } else {
                    "await kernel_name_1(3)".to_owned()
                }
            } else if library {
                format!("k.{op}(helper_function(3))")
            } else {
                format!("k.{op}(k.value(\"helper_function\"), [3])")
            };
            spelling(k::Stmt::Do { action }, &expected);
        }
    }
    spelling(finish(int(-7)), "return -7");
    spelling(
        finish(k::Expr::Literal(k::Literal::Function("-7".into()))),
        "return declared.kernel_name_1",
    );
    spelling(
        finish(k::Expr::Literal(k::Literal::Float(k::Float::new(-0.0)))),
        "return -0.0",
    );
    spelling(
        finish(k::Expr::Literal(k::Literal::Float(k::Float::new(
            f64::INFINITY,
        )))),
        "return float(\"inf\")",
    );
    spelling(k::Stmt::Break, "    break\n");
    spelling(k::Stmt::Continue, "    continue\n");
}
/// An independently written reader fixture pins tree shape without using the printer.
#[test]
fn independent_readable_source_preserves_elif_blocks_and_call_kinds() {
    let source = r#"def main():
    if x.field[helper_foreign(-0.0)]:
        pass
    elif y:
        return returned(7)
    else:
        try:
            raise "λ\n\""
        except BaseException as error:
            with k.do():
                k.call(helper_foreign(7, x))
        finally:
            k.finish(k.function("x"))
"#;
    let id = k::FunctionId::from_bytes([31; 32]);
    let doc = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::If {
            condition: index(
                field(variable("x"), "field"),
                k::Expr::Call {
                    function: id,
                    args: vec![k::Expr::Literal(k::Literal::Float(k::Float::new(-0.0)))],
                },
            ),
            then_block: vec![],
            else_block: vec![k::Stmt::If {
                condition: variable("y"),
                then_block: vec![k::Stmt::Return { value: int(7) }],
                else_block: vec![k::Stmt::Try(k::TryStmt {
                    body: vec![k::Stmt::Throw {
                        value: k::Expr::Literal(k::Literal::Text("λ\n\"".into())),
                    }],
                    catch: Some(k::Catch {
                        binding: "error".into(),
                        body: vec![k::Stmt::Do {
                            action: k::Action::Call {
                                callee: k::Callee::Library(id),
                                args: vec![
                                    k::Atom::Literal(k::Literal::Int(7.into())),
                                    k::Atom::Variable("x".into()),
                                ],
                            },
                        }],
                    }),
                    finally: Some(vec![finish(k::Expr::Literal(k::Literal::Function(
                        "x".into(),
                    )))]),
                })],
            }],
        }],
    );
    // The base document, not the source, binds `helper_foreign` to its identity.
    let mut doc = doc;
    doc.manifest
        .functions
        .insert(id, k::FunctionName::new("foreign").unwrap());
    let decoded = crate::read(source, &doc, Default::default()).unwrap();
    assert_eq!(
        doc.canonical_bytes().unwrap(),
        decoded.canonical_bytes().unwrap()
    );
    roundtrip(&doc, true);
}
/// Domain: 6 names × 7 literals × 8 control contexts × 2 block choices = 672 documents.
/// Each includes same-label foreign helpers, reserved aliases, shadowing and UTF-8 escapes.
#[test]
fn bounded_readable_interactions_preserve_exact_bytes_and_maps() {
    let names = [
        "item",
        "λ",
        "for",
        "a b",
        "helper_same_label",
        "kernel_name_1",
    ];
    let literals = [
        k::Literal::Null,
        k::Literal::Int((-1).into()),
        k::Literal::Float(k::Float::new(-0.0)),
        k::Literal::Float(k::Float::new(1e20)),
        k::Literal::Float(k::Float::new(f64::NAN)),
        k::Literal::Text("λ\n\"\\\u{2028}".into()),
        k::Literal::Absent,
    ];
    let ids = [
        k::FunctionId::from_bytes([21; 32]),
        k::FunctionId::from_bytes([22; 32]),
    ];
    let mut count = 0;
    for name in names {
        for literal in &literals {
            for context in 0..8 {
                for empty in [false, true] {
                    let value = k::Expr::Call {
                        function: ids[0],
                        args: vec![index(
                            field(variable(name), "title"),
                            k::Expr::Literal(literal.clone()),
                        )],
                    };
                    let body = if empty {
                        vec![]
                    } else {
                        vec![
                            k::Stmt::Let {
                                name: name.into(),
                                value: k::Rhs::Expr(value.clone()),
                            },
                            finish(value.clone()),
                        ]
                    };
                    let stmt = match context {
                        0 => k::Stmt::If {
                            condition: value,
                            then_block: body,
                            else_block: vec![],
                        },
                        1 => k::Stmt::If {
                            condition: value,
                            then_block: vec![],
                            else_block: vec![k::Stmt::If {
                                condition: variable(name),
                                then_block: body,
                                else_block: vec![],
                            }],
                        },
                        2 => k::Stmt::For {
                            binding: name.into(),
                            iterable: value,
                            body,
                        },
                        3 => k::Stmt::While {
                            condition: value,
                            body,
                        },
                        4..=6 => k::Stmt::Try(k::TryStmt {
                            body,
                            catch: (context != 5).then(|| k::Catch {
                                binding: name.into(),
                                body: vec![],
                            }),
                            finally: (context != 4).then(Vec::new),
                        }),
                        _ => finish(k::Expr::Closure(Box::new(k::Closure {
                            params: vec![name.into()],
                            body,
                        }))),
                    };
                    let mut doc = document(stmt);
                    for id in ids {
                        doc.manifest
                            .functions
                            .insert(id, k::FunctionName::new("same.label").unwrap());
                    }
                    let off = roundtrip(&doc, false);
                    let on = roundtrip(&doc, true);
                    assert_eq!(off.source, on.source);
                    assert!(off.source_map.is_none());
                    check_map(&doc, &on);
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 672);
    eprintln!("readable bounded domain: names=6 literals=7 contexts=8 blocks=2 documents={count}");
}
fn check_map(doc: &k::Document, printed: &crate::PrintedSource) {
    let map = printed.source_map.as_ref().unwrap();
    let mut expected = std::collections::BTreeMap::new();
    for (unit, node) in std::iter::once((k::Unit::Main, k::Node::Block(&doc.main))).chain(
        doc.functions
            .iter()
            .map(|(name, f)| (k::Unit::Function(name.clone()), k::Node::Block(&f.body))),
    ) {
        let mut stack = vec![(k::Site::new(unit, vec![]), node)];
        while let Some((site, node)) = stack.pop() {
            for (i, child) in node.children().into_iter().enumerate() {
                stack.push((site.child(i as u32), child));
            }
            expected.insert(site, node);
        }
    }
    assert_eq!(map.sites.len(), expected.len());
    let mut actual = std::collections::BTreeSet::new();
    for entry in &map.sites {
        assert!(actual.insert(entry.site.clone()));
        let node = expected
            .get(&entry.site)
            .expect("only real Nodes get Sites");
        assert_eq!(entry.implicit, None);
        assert!(!entry.ranges.is_empty());
        for range in &entry.ranges {
            assert!(range.start < range.end);
            let slice = printed.source.get(range.start..range.end).unwrap();
            if matches!(node, k::Node::Action(_)) {
                assert!(slice.contains("k.") || slice.starts_with("await "));
            }
            if matches!(node, k::Node::Expr(k::Expr::Variable(n)) if n.as_str() == "λ") {
                assert_eq!(slice.trim(), "λ");
            }
        }
    }
    assert_eq!(actual, expected.into_keys().collect());
}
/// Limits apply to real inline nodes, not just indentation depth.
#[test]
fn readable_inline_depth_and_malformed_syntax_refuse() {
    let mut expr = variable("x");
    for _ in 0..62 {
        expr = field(expr, "title");
    }
    let doc = document(finish(expr));
    let printed = roundtrip(&doc, true);
    check_map(&doc, &printed);
    let bad = printed.source.replace("return x", "return x.title");
    assert_eq!(
        crate::read(&bad, &doc, Default::default())
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceDepth.as_str()
    );
    let base = super::hand_document();
    for body in [
        "try:\n        pass\n", // handler-less try remains explicit
        "if answer:\n        pass\n    else:\n        pass\n    else:\n        pass\n",
        "for answer in answer:\n        pass\n    else:\n        pass\n",
        "k.finish(answer.return)\n",
        "k.finish(answer())\n",
        "k.finish(helper_unknown())\n",
        "k.finish(07)\n",
        "k.finish(7e999)\n",
        "k.finish(1.00)\n",
        "with k.do():\n        k.call(helper_unknown(answer.field))\n",
    ] {
        let source = format!("def main():\n    {body}");
        assert_eq!(
            crate::read(&source, &base, Default::default())
                .unwrap_err()
                .code,
            crate::GeneratedSourceCode::InvalidGeneratedSource.as_str(),
            "{source}"
        );
    }
}
/// Native numeric variants are selected by spelling, independent of manifest policy.
#[test]
fn native_literals_preserve_variants_extremes_and_explicit_fallbacks() {
    let integers = [
        "0",
        "-7",
        "9223372036854775808",
        "123456789012345678901234567890",
    ];
    let floats = [
        0.0,
        -0.0,
        1.0,
        -7.0,
        1e20,
        1e-20,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
    ];
    for numbers in [k::NumberPolicy::Float, k::NumberPolicy::BySpelling] {
        for text in integers {
            let doc = k::Document::new(
                numbers,
                vec![finish(k::Expr::Literal(k::Literal::Int(
                    k::Integer::parse(text).unwrap(),
                )))],
            );
            assert!(
                roundtrip(&doc, false)
                    .source
                    .contains(&format!("return {text}"))
            );
        }
        for value in floats {
            let literal = k::Float::new(value);
            let doc = k::Document::new(
                numbers,
                vec![finish(k::Expr::Literal(k::Literal::Float(literal)))],
            );
            assert!(
                roundtrip(&doc, false)
                    .source
                    .contains(&format!("return {literal}"))
            );
        }
    }
    for (literal, spelling) in [
        (k::Literal::Null, "None"),
        (k::Literal::Bool(false), "False"),
        (k::Literal::Text("λ\n\"\\".into()), "\"λ\\n\\\"\\\\\""),
        (k::Literal::Absent, "k.absent"),
        (
            k::Literal::Bytes(k::Bytes::parse_hex("00ff").unwrap()),
            "bytes.fromhex(\"00ff\")",
        ),
        (
            k::Literal::Function("None".into()),
            "declared.kernel_name_1",
        ),
        (k::Literal::Float(k::Float::new(f64::NAN)), "float(\"nan\")"),
        (
            k::Literal::Float(k::Float::new(f64::NEG_INFINITY)),
            "float(\"-inf\")",
        ),
    ] {
        spelling_literal(literal, spelling);
    }
}
fn spelling_literal(literal: k::Literal, expected: &str) {
    spelling(
        finish(k::Expr::Literal(literal)),
        &format!("return {expected}"),
    );
}

/// G-SOURCE-READABLE: ordinary bindings, collections and function bodies never use suites.
#[test]
fn ordinary_python_body_uses_native_bindings_collections_and_definitions() {
    let mut doc = document(k::Stmt::Let {
        name: "items".into(),
        value: k::Rhs::Expr(k::Expr::List(vec![int(1), int(2)])),
    });
    doc.functions.insert(
        "worker".into(),
        k::Function {
            params: vec!["incoming".into()],
            body: vec![k::Stmt::Return {
                value: variable("incoming"),
            }],
        },
    );
    doc.main.push(k::Stmt::Assign {
        place: k::Place::Variable("items".into()),
        value: k::Rhs::Action(k::Action::Call {
            callee: k::Callee::Declared("worker".into()),
            args: vec![k::Atom::Variable("items".into())],
        }),
    });
    doc.main.push(finish(variable("items")));
    let printed = roundtrip(&doc, true);
    assert!(
        printed.source.contains("def worker(incoming):"),
        "{}",
        printed.source
    );
    assert!(printed.source.contains("def main():"), "{}", printed.source);
    assert!(
        printed.source.contains("items = [1, 2]"),
        "{}",
        printed.source
    );
    assert!(
        printed
            .source
            .contains("items = await declared.worker(items)"),
        "{}",
        printed.source
    );
    assert!(
        !printed.source.lines().any(|line| line.contains("k.")),
        "{}",
        printed.source
    );
    check_map(&doc, &printed);
}

/// G-SOURCE-COLLECTIONS: each native delimiter retains its kernel variant and order.
#[test]
fn native_collections_preserve_variants_order_and_expression_fallbacks() {
    for (expr, expected) in [
        (k::Expr::List(vec![]), "return []"),
        (k::Expr::List(vec![int(1), int(2)]), "return [1, 2]"),
        (k::Expr::Tuple(vec![]), "return ()"),
        (k::Expr::Tuple(vec![int(1)]), "return (1,)"),
        (k::Expr::Tuple(vec![int(1), int(2)]), "return (1, 2)"),
        (k::Expr::Set(vec![]), "return set()"),
        (k::Expr::Set(vec![int(2), int(1)]), "return {2, 1}"),
        (k::Expr::Map(vec![]), "return {}"),
        (
            k::Expr::Map(vec![k::MapEntry {
                key: int(1),
                value: int(2),
            }]),
            "return {1: 2}",
        ),
        (
            k::Expr::Record(vec![k::RecordEntry {
                field: "x".into(),
                value: int(2),
            }]),
            "return record({\"x\": 2})",
        ),
        (k::Expr::List(vec![closure()]), "with k.list():"),
        (k::Expr::Tuple(vec![closure()]), "with k.tuple():"),
        (k::Expr::Set(vec![closure()]), "with k.set():"),
        (
            k::Expr::Map(vec![k::MapEntry {
                key: int(1),
                value: closure(),
            }]),
            "with k.map():",
        ),
        (
            k::Expr::Record(vec![k::RecordEntry {
                field: "x".into(),
                value: closure(),
            }]),
            "with k.record(",
        ),
    ] {
        let doc = document(finish(expr));
        let printed = roundtrip(&doc, true);
        assert!(
            printed.source.contains(expected),
            "{expected}\n{}",
            printed.source
        );
        check_map(&doc, &printed);
    }
    // Duplicate dictionary keys are syntax, not a dictionary evaluation: the
    // inverse preserves every entry, including duplicates and their order.
    for expr in [
        k::Expr::Map(vec![
            k::MapEntry {
                key: int(1),
                value: int(2),
            },
            k::MapEntry {
                key: int(1),
                value: int(3),
            },
        ]),
        k::Expr::Record(vec![
            k::RecordEntry {
                field: "x".into(),
                value: int(2),
            },
            k::RecordEntry {
                field: "x".into(),
                value: int(3),
            },
        ]),
    ] {
        roundtrip(&document(finish(expr)), true);
    }
    for place in [
        k::Place::Member(k::Member::Field {
            target: int(1),
            field: "title".into(),
        }),
        k::Place::Member(k::Member::Field {
            target: variable("items"),
            field: "return".into(),
        }),
        k::Place::Member(k::Member::Index {
            target: variable("items"),
            index: int(0),
        }),
    ] {
        let doc = document(k::Stmt::Assign {
            place,
            value: k::Rhs::Expr(int(4)),
        });
        check_map(&doc, &roundtrip(&doc, true));
    }
    spelling(
        k::Stmt::Remove {
            member: k::Member::Index {
                target: variable("items"),
                index: int(0),
            },
        },
        "del items[0]",
    );
}

/// G-SOURCE-BINDINGS: lexical inference never collapses Let, Assign or Closure.
#[test]
fn binding_inference_preserves_shadowing_initial_assignments_and_closures() {
    let mut doc = k::Document::new(
        k::NumberPolicy::BySpelling,
        vec![
            k::Stmt::Let {
                name: "x".into(),
                value: k::Rhs::Expr(int(1)),
            },
            k::Stmt::Assign {
                place: k::Place::Variable("x".into()),
                value: k::Rhs::Expr(int(2)),
            },
            k::Stmt::Let {
                name: "x".into(),
                value: k::Rhs::Expr(int(3)),
            },
            k::Stmt::Assign {
                place: k::Place::Variable("earlier".into()),
                value: k::Rhs::Expr(int(4)),
            },
            k::Stmt::Let {
                name: "nested".into(),
                value: k::Rhs::Expr(k::Expr::Closure(Box::new(k::Closure {
                    params: vec!["x".into()],
                    body: vec![
                        k::Stmt::Assign {
                            place: k::Place::Variable("x".into()),
                            value: k::Rhs::Expr(int(5)),
                        },
                        k::Stmt::Return {
                            value: variable("x"),
                        },
                    ],
                }))),
            },
            k::Stmt::Assign {
                place: k::Place::Variable("nested".into()),
                value: k::Rhs::Expr(closure()),
            },
            k::Stmt::Let {
                name: "nested".into(),
                value: k::Rhs::Expr(closure()),
            },
            finish(variable("x")),
        ],
    );
    doc.main.push(k::Stmt::Do {
        action: k::Action::Call {
            callee: k::Callee::Value("toolshed".into()),
            args: vec![],
        },
    });
    doc.functions.insert(
        "main".into(),
        k::Function {
            params: vec![],
            body: vec![finish(int(9))],
        },
    );
    let printed = roundtrip(&doc, true);
    for expected in [
        "x = 1",
        "x = 2",
        "x: Final = 3",
        "earlier: reassigned = 4",
        "def nested(x):",
        "def nested():",
        "def nested() -> Final:",
        "return x",
        "return finish(9)",
    ] {
        assert!(
            printed.source.contains(expected),
            "{expected}\n{}",
            printed.source
        );
    }
    check_map(&doc, &printed);
    // Parameters are already bound; first assignment must stay Assign.
    let mut doc = document(finish(int(0)));
    doc.functions.insert(
        "worker".into(),
        k::Function {
            params: vec!["x".into()],
            body: vec![k::Stmt::Assign {
                place: k::Place::Variable("x".into()),
                value: k::Rhs::Expr(int(1)),
            }],
        },
    );
    roundtrip(&doc, true);
}

/// G-SOURCE-OPERATORS: surface operators retain the base manifest's full identity.
#[test]
fn native_operators_require_unique_identity_and_exact_arity() {
    let labels = [
        ("add", "+"),
        ("sub", "-"),
        ("mul", "*"),
        ("eq", "=="),
        ("same", "is"),
        ("lt", "<"),
        ("le", "<="),
        ("gt", ">"),
        ("ge", ">="),
        ("bool.not", "not"),
        ("neg", "neg"),
    ];
    for (i, (label, token)) in labels.into_iter().enumerate() {
        let id = k::FunctionId::from_bytes([i as u8 + 1; 32]);
        let unary = matches!(token, "not" | "neg");
        let args = if unary {
            vec![variable("x")]
        } else {
            vec![variable("x"), int(2)]
        };
        let mut doc = document(finish(k::Expr::Call { function: id, args }));
        doc.manifest
            .functions
            .insert(id, k::FunctionName::new(label).unwrap());
        let printed = roundtrip(&doc, true);
        let expected = match token {
            "not" => "not x".to_owned(),
            "neg" => "-x".to_owned(),
            _ => format!("x {token} 2"),
        };
        assert!(printed.source.contains(&expected), "{}", printed.source);
        check_map(&doc, &printed);
        let changed = printed.source.replace(&expected, "x");
        let decoded = crate::read(&changed, &doc, Default::default()).unwrap();
        assert_ne!(
            doc.canonical_bytes().unwrap(),
            decoded.canonical_bytes().unwrap()
        );
        // An arity mismatch cannot use the native operator inverse.
        doc.main = vec![finish(k::Expr::Call {
            function: id,
            args: vec![],
        })];
        assert!(roundtrip(&doc, false).source.contains("return helper_"));
    }
}

/// FIG-6272: operators print with Python's own precedence and only the
/// parentheses it needs, and read back to the same tree: grouping, left and
/// right associativity, unchained comparisons, `-(7)` against `-7`, and a
/// unary base of `**`.
#[test]
fn python_operators_print_minimal_parentheses_with_an_exact_inverse() {
    let labels = ["add", "sub", "mul", "py.pow", "neg", "bool.not", "eq", "lt"];
    let ids: Vec<_> = (0..labels.len())
        .map(|i| k::FunctionId::from_bytes([i as u8 + 40; 32]))
        .collect();
    let op = |label: &str, args: Vec<k::Expr>| k::Expr::Call {
        function: ids[labels.iter().position(|l| *l == label).unwrap()],
        args,
    };
    let x = || variable("x");
    let cases = [
        (
            op("mul", vec![op("add", vec![int(1), int(2)]), int(3)]),
            "(1 + 2) * 3",
        ),
        (
            op("add", vec![int(1), op("mul", vec![int(2), int(3)])]),
            "1 + 2 * 3",
        ),
        (
            op("sub", vec![int(1), op("sub", vec![int(2), int(3)])]),
            "1 - (2 - 3)",
        ),
        (
            op("sub", vec![op("sub", vec![int(1), int(2)]), int(3)]),
            "1 - 2 - 3",
        ),
        (
            op("py.pow", vec![int(2), op("py.pow", vec![int(3), int(2)])]),
            "2 ** 3 ** 2",
        ),
        (
            op("py.pow", vec![op("py.pow", vec![int(2), int(3)]), int(2)]),
            "(2 ** 3) ** 2",
        ),
        (op("neg", vec![op("py.pow", vec![x(), int(2)])]), "-x ** 2"),
        (
            op("py.pow", vec![op("neg", vec![x()]), int(2)]),
            "(-x) ** 2",
        ),
        (op("py.pow", vec![int(-7), int(2)]), "(-7) ** 2"),
        (op("py.pow", vec![int(2), int(-7)]), "2 ** -7"),
        (op("neg", vec![int(7)]), "-(7)"),
        (int(-7), "-7"),
        (
            op("bool.not", vec![op("eq", vec![x(), int(1)])]),
            "not x == 1",
        ),
        (
            op("eq", vec![op("bool.not", vec![x()]), int(1)]),
            "(not x) == 1",
        ),
        (
            op("eq", vec![op("lt", vec![x(), int(1)]), int(1)]),
            "(x < 1) == 1",
        ),
        (op("add", vec![field(x(), "y"), int(1)]), "x.y + 1"),
        (field(op("add", vec![x(), int(1)]), "y"), "(x + 1).y"),
        (index(op("add", vec![x(), int(1)]), int(0)), "(x + 1)[0]"),
    ];
    for (expr, spelled) in cases {
        let mut doc = document(finish(expr));
        for (id, label) in ids.iter().zip(labels) {
            doc.manifest
                .functions
                .insert(*id, k::FunctionName::new(label).unwrap());
        }
        let printed = roundtrip(&doc, true);
        assert!(
            printed.source.contains(&format!("return {spelled}\n")),
            "{spelled}: {}",
            printed.source
        );
        check_map(&doc, &printed);
    }
    // Chained comparisons and an ungrouped unary base have no exact inverse.
    let mut doc = document(finish(int(0)));
    for (id, label) in ids.iter().zip(labels) {
        doc.manifest
            .functions
            .insert(*id, k::FunctionName::new(label).unwrap());
    }
    for body in ["return x < 1 == 1", "return -7 ** 2", "return -7 .real"] {
        assert!(
            crate::read(
                &format!("def main():\n    {body}\n"),
                &doc,
                Default::default()
            )
            .is_err(),
            "{body}"
        );
    }
}

/// G-SOURCE-EFFECTS: dotted and nonidentifier names preserve result metadata.
#[test]
fn effect_calls_preserve_names_atomic_arguments_and_result_types() {
    for name in [
        "research.search",
        "tools.search",
        "class.query",
        "tools.class",
    ] {
        let effect = k::EffectName::new(name).unwrap();
        for declared in [true, false] {
            for result in [k::Type::Any, k::Type::List(Box::new(k::Type::Text))] {
                let mut doc = document(k::Stmt::Let {
                    name: "answer".into(),
                    value: k::Rhs::Action(k::Action::Perform {
                        effect: effect.clone(),
                        args: vec![
                            k::Atom::Variable("query".into()),
                            k::Atom::Literal(k::Literal::Int(1.into())),
                        ],
                        result,
                    }),
                });
                if declared {
                    doc.manifest.effects.insert(
                        effect.clone(),
                        k::Signature {
                            params: vec![],
                            result: k::Type::Any,
                        },
                    );
                }
                let printed = roundtrip(&doc, true);
                assert!(
                    printed.source.contains("answer = await tools"),
                    "{}",
                    printed.source
                );
                assert!(
                    !printed.source.lines().any(|line| line.contains("k.")),
                    "{}",
                    printed.source
                );
                check_map(&doc, &printed);
            }
        }
    }
}

/// G-SOURCE-INVERSE: independently authored generated syntax pins Let/Assign and call kinds.
#[test]
fn independent_native_source_preserves_binding_and_call_kinds() {
    let source = r#"def worker(incoming):
    incoming = 3
    return incoming
def main():
    items = [1, 2]
    items = (2,)
    items: Final = {1: 2}
    answer = await declared.worker(items)
    return answer
"#;
    let mut doc = k::Document::new(
        k::NumberPolicy::BySpelling,
        vec![
            k::Stmt::Let {
                name: "items".into(),
                value: k::Rhs::Expr(k::Expr::List(vec![int(1), int(2)])),
            },
            k::Stmt::Assign {
                place: k::Place::Variable("items".into()),
                value: k::Rhs::Expr(k::Expr::Tuple(vec![int(2)])),
            },
            k::Stmt::Let {
                name: "items".into(),
                value: k::Rhs::Expr(k::Expr::Map(vec![k::MapEntry {
                    key: int(1),
                    value: int(2),
                }])),
            },
            k::Stmt::Let {
                name: "answer".into(),
                value: k::Rhs::Action(k::Action::Call {
                    callee: k::Callee::Declared("worker".into()),
                    args: vec![k::Atom::Variable("items".into())],
                }),
            },
            finish(variable("answer")),
        ],
    );
    doc.functions.insert(
        "worker".into(),
        k::Function {
            params: vec!["incoming".into()],
            body: vec![
                k::Stmt::Assign {
                    place: k::Place::Variable("incoming".into()),
                    value: k::Rhs::Expr(int(3)),
                },
                k::Stmt::Return {
                    value: variable("incoming"),
                },
            ],
        },
    );
    let decoded = crate::read(source, &doc, Default::default()).unwrap();
    assert_eq!(
        doc.canonical_bytes().unwrap(),
        decoded.canonical_bytes().unwrap()
    );
    roundtrip(&doc, true);
}

/// G-SOURCE-EXACT: a generic member place and native action share the RHS inverse.
#[test]
fn generic_member_assignments_decode_native_await_rhs() {
    for indexed in [false, true] {
        for declared in [false, true] {
            let target = k::Expr::Read(Box::new(k::ProjectionRead {
                handle: variable("handle"),
                request: k::Expr::Literal(k::Literal::Null),
            }));
            let member = if indexed {
                k::Member::Index {
                    target,
                    index: int(0),
                }
            } else {
                k::Member::Field {
                    target,
                    field: "answer".into(),
                }
            };
            let action = if declared {
                k::Action::Call {
                    callee: k::Callee::Declared("worker".into()),
                    args: vec![],
                }
            } else {
                k::Action::Perform {
                    effect: k::EffectName::new("echo").unwrap(),
                    args: vec![],
                    result: k::Type::Any,
                }
            };
            let mut doc = document(k::Stmt::Assign {
                place: k::Place::Member(member),
                value: k::Rhs::Action(action),
            });
            doc.manifest.effects.insert(
                k::EffectName::new("echo").unwrap(),
                k::Signature {
                    params: vec![],
                    result: k::Type::Any,
                },
            );
            roundtrip(&doc, true);
        }
    }
}

/// G-SOURCE-READABLE: typed arithmetic and text concat keep their distinct identities.
#[test]
fn mixed_numeric_text_and_dynamic_operators_are_native_and_exact() {
    let doc = super::super::machine::lower(
        "n = 1 + 2\ns = 'a' + 'b'\nx = n + 3\ny = s + 'c'\ndef plus(a, b):\n return a + b\n",
    )
    .unwrap()
    .document;
    let printed = roundtrip(&doc, true);
    assert!(
        !printed.source.contains("helper_add(") && !printed.source.contains("helper_text_concat("),
        "{}",
        printed.source
    );
}

/// G-SOURCE-NAMES: different kernel names remain different CPython identifiers.
#[test]
fn normalization_collisions_receive_distinct_reversible_aliases() {
    let doc = k::Document::new(
        k::NumberPolicy::BySpelling,
        vec![
            k::Stmt::Let {
                name: "ﬁle".into(),
                value: k::Rhs::Expr(int(1)),
            },
            k::Stmt::Let {
                name: "file".into(),
                value: k::Rhs::Expr(int(2)),
            },
            finish(k::Expr::List(vec![variable("ﬁle"), variable("file")])),
        ],
    );
    let printed = roundtrip(&doc, true);
    assert!(!printed.source.contains("ﬁle"), "{}", printed.source);
}
