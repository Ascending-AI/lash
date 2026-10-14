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
            .contains("        if ready:\n            break\n        else:\n            pass\n"),
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
    for (name, expected) in [("λ", "k.finish(λ)"), ("return", "k.finish(kernel_name_1)")] {
        spelling(finish(variable(name)), expected);
    }
    spelling(
        finish(field(variable("object_value"), "title")),
        "k.finish(object_value.title)",
    );
    spelling(
        finish(field(variable("object_value"), "return")),
        "with k.field(\"return\"):",
    );
    spelling(
        finish(field(closure(), "title")),
        "with k.field(\"title\"):",
    );
    spelling(
        finish(index(variable("items"), int(-1))),
        "k.finish(items[-1])",
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
                "return ready\n"
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
                "k.print(ready)"
            } else {
                "with k.print():"
            },
        );
        spelling(
            k::Stmt::Finish {
                value: expr.clone(),
            },
            if native {
                "k.finish(ready)"
            } else {
                "with k.finish():"
            },
        );
        spelling(
            k::Stmt::Fail { value: expr },
            if native {
                "k.fail(ready)"
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
        "else:\n            if second:\n",
    );
    let id = k::FunctionId::from_bytes([31; 32]);
    // Helper identity is in the envelope once; labels do not drive the reader.
    spelling(
        finish(k::Expr::Call {
            function: id,
            args: vec![int(3), variable("λ")],
        }),
        "k.finish(helper_function(3, λ))",
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
            let expected = if library {
                format!("k.{op}(helper_function(3))")
            } else {
                format!("k.{op}(k.value(\"helper_function\"), [3])")
            };
            spelling(k::Stmt::Do { action }, &expected);
        }
    }
    spelling(finish(int(-7)), "k.finish(-7)");
    spelling(
        finish(k::Expr::Literal(k::Literal::Function("-7".into()))),
        "k.finish(k.function(\"-7\"))",
    );
    spelling(
        finish(k::Expr::Literal(k::Literal::Float(k::Float::new(-0.0)))),
        "k.finish(-0.0)",
    );
    spelling(
        finish(k::Expr::Literal(k::Literal::Float(k::Float::new(
            f64::INFINITY,
        )))),
        "k.finish(k.float(\"inf\"))",
    );
    spelling(k::Stmt::Break, "        break\n");
    spelling(k::Stmt::Continue, "        continue\n");
}
/// An independently written reader fixture pins tree shape without using the printer.
#[test]
fn independent_readable_source_preserves_elif_blocks_and_call_kinds() {
    let source = r#"k.document("lash.generated.python", "1", "{\"kernel\":1,\"numbers\":\"float\"}", "{}", [], "[[\"x\",\"x\"],[\"y\",\"y\"],[\"error\",\"error\"]]", "[[\"helper_foreign\",\"1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f\"]]")
with k.main():
    with k.block():
        if x.field[helper_foreign(-0.0)]:
            pass
        elif y:
            return 7
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
    let decoded = crate::compile_generated(source, Default::default()).unwrap();
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
                    assert!(on.source.contains("helper_same_label_2"));
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
                assert!(slice.contains("k."));
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
    let bad = printed.source.replace("k.finish(x", "k.finish(x.title");
    assert_eq!(
        crate::compile_generated(&bad, Default::default())
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceDepth.as_str()
    );
    let header = super::HAND_WRITTEN.split("with k.declare").next().unwrap();
    for body in [
        "if answer:\n            pass\n", // else is required, even when empty
        "try:\n            pass\n",       // handler-less try remains explicit
        "if answer:\n            pass\n        else:\n            pass\n        else:\n            pass\n",
        "for answer in answer:\n            pass\n        else:\n            pass\n",
        "k.finish(answer.return)\n",
        "k.finish(answer())\n",
        "k.finish(helper_unknown())\n",
        "k.finish(07)\n",
        "k.finish(7e999)\n",
        "k.finish(1.00)\n",
        "with k.do():\n            k.call(helper_unknown(answer.field))\n",
    ] {
        let source = format!("{header}with k.main():\n    with k.block():\n        {body}");
        assert_eq!(
            crate::compile_generated(&source, Default::default())
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
                    .contains(&format!("k.finish({text})"))
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
                    .contains(&format!("k.finish({literal})"))
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
            "k.bytes(\"00ff\")",
        ),
        (k::Literal::Function("None".into()), "k.function(\"None\")"),
        (
            k::Literal::Float(k::Float::new(f64::NAN)),
            "k.float(\"nan\")",
        ),
        (
            k::Literal::Float(k::Float::new(f64::NEG_INFINITY)),
            "k.float(\"-inf\")",
        ),
    ] {
        spelling_literal(literal, spelling);
    }
}
fn spelling_literal(literal: k::Literal, expected: &str) {
    spelling(
        finish(k::Expr::Literal(literal)),
        &format!("k.finish({expected})"),
    );
}
