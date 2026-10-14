//! G-TS-READABLE: each native spelling has an exact inverse and a paired fallback.
use super::generated_source::roundtrip;
use lash_kernel_doc as k;

fn variable(name: &str) -> k::Expr {
    k::Expr::Variable(name.into())
}
fn literal(value: k::Literal) -> k::Expr {
    k::Expr::Literal(value)
}
fn integer(value: i64) -> k::Expr {
    literal(k::Literal::Int(value.into()))
}
fn spelling(expr: k::Expr) -> String {
    roundtrip(
        &k::Document::new(
            k::NumberPolicy::Float,
            vec![k::Stmt::Finish { value: expr }],
        ),
        false,
    )
    .source
}

#[test]
fn native_numbers_preserve_variants_bits_and_safe_integer_boundary() {
    for (value, expected) in [
        (k::Literal::Int(7.into()), "k.finish(7);"),
        (k::Literal::Int((-7).into()), "k.finish(-7);"),
        (
            k::Literal::Int(9_007_199_254_740_991i64.into()),
            "k.finish(9007199254740991);",
        ),
        (
            k::Literal::Int(9_007_199_254_740_992i64.into()),
            "k.int(\"9007199254740992\")",
        ),
        (k::Literal::Float(k::Float::new(7.0)), "k.finish(7.0);"),
        (k::Literal::Float(k::Float::new(-0.0)), "k.finish(-0.0);"),
        (
            k::Literal::Float(k::Float::new(f64::INFINITY)),
            "k.float(\"inf\")",
        ),
        (
            k::Literal::Float(k::Float::new(f64::NAN)),
            "k.float(\"nan\")",
        ),
        (k::Literal::Absent, "k.finish(k.absent);"),
        (k::Literal::Null, "k.finish(null);"),
    ] {
        let source = spelling(literal(value));
        assert!(source.contains(expected), "{source}");
    }
    for value in [
        0.0,
        -1.25,
        1e-300,
        f64::from_bits(1),
        f64::MAX,
        f64::NEG_INFINITY,
    ] {
        spelling(literal(k::Literal::Float(k::Float::new(value))));
    }
}

#[test]
fn native_collections_keep_variants_order_and_record_applicability() {
    let items = vec![integer(1), integer(2)];
    assert!(spelling(k::Expr::List(items.clone())).contains("k.finish([1, 2]);"));
    assert!(spelling(k::Expr::Tuple(items.clone())).contains("k.tuple(1, 2)"));
    assert!(spelling(k::Expr::Set(items)).contains("k.set(1, 2)"));
    assert!(spelling(k::Expr::Map(vec![])).contains("k.map()"));
    assert!(spelling(k::Expr::Record(vec![])).contains("k.finish({});"));
    let record = |fields: &[&str]| {
        k::Expr::Record(
            fields
                .iter()
                .enumerate()
                .map(|(i, field)| k::RecordEntry {
                    field: (*field).into(),
                    value: integer(i as i64),
                })
                .collect(),
        )
    };
    assert!(spelling(record(&["λ", "ready"])).contains("{λ: 0, ready: 1}"));
    for fields in [
        vec!["not a field"],
        vec!["__proto__"],
        vec!["ready", "ready"],
    ] {
        assert!(spelling(record(&fields)).contains("k.record("));
    }
}

#[test]
fn native_members_preserve_field_index_and_assignment_targets() {
    let field = |name: &str| k::Member::Field {
        target: variable("page"),
        field: name.into(),
    };
    assert!(spelling(k::Expr::Member(Box::new(field("ready")))).contains("(page).ready"));
    assert!(
        spelling(k::Expr::Member(Box::new(field("not a field"))))
            .contains("k.field(page, \"not a field\")")
    );
    let index = k::Member::Index {
        target: variable("page"),
        index: literal(k::Literal::Text("ready".into())),
    };
    assert!(spelling(k::Expr::Member(Box::new(index.clone()))).contains("(page)[\"ready\"]"));
    let document = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Assign {
                place: k::Place::Member(field("value")),
                value: k::Rhs::Expr(integer(1)),
            },
            k::Stmt::Assign {
                place: k::Place::Member(field("not a field")),
                value: k::Rhs::Expr(integer(2)),
            },
            k::Stmt::Assign {
                place: k::Place::Member(index),
                value: k::Rhs::Expr(integer(3)),
            },
            k::Stmt::Remove {
                member: field("ready"),
            },
        ],
    );
    let source = roundtrip(&document, true).source;
    assert!(source.contains("(page).value = 1;"));
    assert!(source.contains("k.field(page, \"not a field\").value = 2;"));
    assert!(source.contains("(page)[\"ready\"] = 3;"));
}

#[test]
fn helper_calls_keep_expression_action_and_callee_kinds_unambiguous() {
    let id = k::FunctionId::from_bytes([31; 32]);
    let other = k::FunctionId::from_bytes([32; 32]);
    let mut document = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Let {
                name: "result".into(),
                value: k::Rhs::Expr(k::Expr::Call {
                    function: id,
                    args: vec![integer(1)],
                }),
            },
            k::Stmt::Let {
                name: "action".into(),
                value: k::Rhs::Action(k::Action::Call {
                    callee: k::Callee::Library(id),
                    args: vec![k::Atom::Literal(k::Literal::Int(1.into()))],
                }),
            },
            k::Stmt::Do {
                action: k::Action::Spawn {
                    callee: k::Callee::Library(other),
                    args: vec![],
                },
            },
            k::Stmt::Do {
                action: k::Action::Call {
                    callee: k::Callee::Declared("result".into()),
                    args: vec![],
                },
            },
            k::Stmt::Do {
                action: k::Action::Call {
                    callee: k::Callee::Value("result".into()),
                    args: vec![],
                },
            },
        ],
    );
    for function in [id, other] {
        document
            .manifest
            .functions
            .insert(function, k::FunctionName::new("foreign.same").unwrap());
    }
    let source = roundtrip(&document, true).source;
    assert!(source.contains("let result = helper_foreign_same(1);"));
    assert!(source.contains("let action = k.call(helper_foreign_same, [1]);"));
    assert!(source.contains("k.spawn(helper_foreign_same_2, []);"));
    assert!(source.contains("k.call(k.declared(\"result\"), []);"));
    assert!(source.contains("k.call(k.value(\"result\"), []);"));
    assert_eq!(source.matches(&id.to_string()).count(), 2); // manifest and alias, never call sites
}

#[test]
fn readable_controls_names_and_implicit_else_preserve_statement_boundaries() {
    let document = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Let {
                name: "λ".into(),
                value: k::Rhs::Expr(integer(1)),
            },
            k::Stmt::For {
                binding: "item".into(),
                iterable: k::Expr::List(vec![variable("λ")]),
                body: vec![k::Stmt::If {
                    condition: variable("item"),
                    then_block: vec![k::Stmt::Print {
                        value: variable("item"),
                    }],
                    else_block: vec![],
                }],
            },
            k::Stmt::While {
                condition: literal(k::Literal::Bool(false)),
                body: vec![k::Stmt::Break],
            },
            k::Stmt::Try(k::TryStmt {
                body: vec![],
                catch: Some(k::Catch {
                    binding: "error".into(),
                    body: vec![],
                }),
                finally: Some(vec![]),
            }),
            k::Stmt::Try(k::TryStmt {
                body: vec![],
                catch: None,
                finally: None,
            }),
        ],
    );
    let off = roundtrip(&document, false);
    let on = roundtrip(&document, true);
    assert_eq!(off.source, on.source);
    assert!(
        off.source.contains(
            "  for (let item of [λ]) {\n    if (item) {\n      k.print(item);\n    }\n  }"
        ),
        "{}",
        off.source
    );
    assert!(!off.source.contains(" else "));
    assert!(off.source.contains("while (false)"));
    assert!(off.source.contains("catch (error)"));
    assert!(off.source.contains("finally"));
    assert!(off.source.contains("k.try(function()"));
    let anchors: Vec<_> = on
        .source_map
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| s.implicit.is_some())
        .collect();
    assert_eq!(anchors.len(), 1);
    assert_eq!(
        anchors[0].site,
        k::Site::new(k::Unit::Main, vec![1, 1, 0, 2])
    );
    assert!(off.source.is_char_boundary(anchors[0].implicit.unwrap()));
    let mut nonempty = document;
    if let k::Stmt::For { body, .. } = &mut nonempty.main[1]
        && let k::Stmt::If { else_block, .. } = &mut body[0]
    {
        else_block.push(k::Stmt::Do {
            action: k::Action::Yield,
        });
    }
    assert!(
        roundtrip(&nonempty, true)
            .source
            .contains(" else {\n      k.yield();")
    );
}

#[test]
fn identifier_aliases_are_injective_across_shadowing_and_reserved_names() {
    for (original, expected) in [
        ("λ", "λ"),
        ("useful", "useful"),
        ("return", "kernel_name_1"),
        ("k", "kernel_name_1"),
        ("helper_label", "kernel_name_1"),
        ("kernel_name_1", "kernel_name_1"),
        ("odd name", "kernel_name_1"),
    ] {
        let name = k::Name::new(original);
        let mut document = k::Document::new(
            k::NumberPolicy::Float,
            vec![
                k::Stmt::Let {
                    name: name.clone(),
                    value: k::Rhs::Expr(integer(1)),
                },
                k::Stmt::Let {
                    name: "outer".into(),
                    value: k::Rhs::Expr(k::Expr::Closure(Box::new(k::Closure {
                        params: vec![name.clone()],
                        body: vec![k::Stmt::Return {
                            value: variable(original),
                        }],
                    }))),
                },
                k::Stmt::Print {
                    value: literal(k::Literal::Function(name.clone())),
                },
                k::Stmt::Let {
                    name: "inner".into(),
                    value: k::Rhs::Expr(k::Expr::Closure(Box::new(k::Closure {
                        params: vec![],
                        body: vec![k::Stmt::Return {
                            value: variable(original),
                        }],
                    }))),
                },
            ],
        );
        document.functions.insert(
            name.clone(),
            k::Function {
                params: vec![name.clone()],
                body: vec![k::Stmt::Return {
                    value: variable(original),
                }],
            },
        );
        document.private_bindings.insert(name);
        let source = roundtrip(&document, true).source;
        assert!(source.contains(&format!("let {expected} = 1;")), "{source}");
        assert!(
            source.contains(&format!("function({expected})")),
            "{source}"
        );
    }
    let document = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Let {
                name: "kernel_name_1".into(),
                value: k::Rhs::Expr(integer(1)),
            },
            k::Stmt::Let {
                name: "odd name".into(),
                value: k::Rhs::Expr(integer(2)),
            },
        ],
    );
    let source = roundtrip(&document, false).source;
    assert!(source.contains("let kernel_name_1 = 1;"));
    assert!(source.contains("let kernel_name_2 = 2;"));
}

// Independent reader witness: these native forms are not obtained from print().
const NATIVE_HEADER: &str = r#"k.document("lash.generated.typescript", "1", {"kernel":1,"numbers":"float"}, {}, [], [["page","page"]], []);"#;

#[test]
fn independent_native_source_has_exact_numeric_collection_and_member_inverse() {
    let source = format!(
        "{NATIVE_HEADER}\nk.main(function() {{ k.finish(k.tuple(7, 7.0, -0.0, [1, 2], {{λ: true}}, (page).λ, (page)[\"λ\"])); }});"
    );
    let decoded = crate::compile_generated(&source, Default::default()).unwrap();
    let expected = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Tuple(vec![
                integer(7),
                literal(k::Literal::Float(k::Float::new(7.0))),
                literal(k::Literal::Float(k::Float::new(-0.0))),
                k::Expr::List(vec![integer(1), integer(2)]),
                k::Expr::Record(vec![k::RecordEntry {
                    field: "λ".into(),
                    value: literal(k::Literal::Bool(true)),
                }]),
                k::Expr::Member(Box::new(k::Member::Field {
                    target: variable("page"),
                    field: "λ".into(),
                })),
                k::Expr::Member(Box::new(k::Member::Index {
                    target: variable("page"),
                    index: literal(k::Literal::Text("λ".into())),
                })),
            ]),
        }],
    );
    assert_eq!(
        decoded.canonical_bytes().unwrap(),
        expected.canonical_bytes().unwrap()
    );
    roundtrip(&expected, true);
}

#[test]
fn native_near_misses_refuse_without_authored_lowering() {
    for expression in [
        "-0",
        "9007199254740992",
        "0x10",
        "1.00",
        "1e999",
        "[1,,2]",
        "[...page]",
        "{x: 1, x: 2}",
        "{__proto__: null}",
        "{page}",
        "{[page]: 1}",
        "page(1)",
        "(page)?.x",
        "(page).x++",
    ] {
        let source = format!("{NATIVE_HEADER}\nk.main(function() {{ k.finish({expression}); }});");
        assert!(
            crate::compile_generated(&source, Default::default()).is_err(),
            "{source}"
        );
    }
}
