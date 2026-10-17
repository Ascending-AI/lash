//! G-SOURCE-EXACT, G-SOURCE-FORMAT, G-SOURCE-RESOURCES and G-SOURCE-MAP.
use crate::{GeneratedSourceLimits, PrintOptions};
use arbitrary::{Arbitrary, Unstructured};
use lash_kernel_doc as k;
use std::collections::{BTreeMap, BTreeSet};

fn roundtrip(document: &k::Document, maps: bool) -> crate::PrintedSource {
    let printed = crate::print(
        document,
        PrintOptions {
            source_map: maps,
            ..Default::default()
        },
    )
    .expect("valid in-budget document prints");
    let decoded = crate::read(&printed.source, document, Default::default())
        .expect("exported source reads back against its base without a library");
    assert_eq!(document.identity(), decoded.identity());
    assert_eq!(
        document.canonical_bytes(),
        decoded.canonical_bytes(),
        "{}",
        printed.source
    );
    printed
}
// Independently written syntax, not obtained from the printer or its rule
// table, using the explicit body-level variants. Nothing but code: the base
// document holds the environment, including the rename of `odd name`.
const HAND_WRITTEN: &str = r#"def worker():
    with k.return_():
        k.int("7")
def main():
    with k.let(kernel_name_1):
        k.float("-0.0")
    with k.finish():
        kernel_name_1
"#;
fn hand_document() -> k::Document {
    let mut document = k::Document::new(
        k::NumberPolicy::BySpelling,
        vec![
            k::Stmt::Let {
                name: "odd name".into(),
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Float(k::Float::new(-0.0)))),
            },
            k::Stmt::Finish {
                value: k::Expr::Variable("odd name".into()),
            },
        ],
    );
    document.functions.insert(
        "worker".into(),
        k::Function {
            params: vec![],
            body: vec![k::Stmt::Return {
                value: k::Expr::Literal(k::Literal::Int(7.into())),
            }],
            start: None,
        },
    );
    document.entries.insert(
        "worker".into(),
        lash_kernel_doc::Entry {
            signature: k::Signature {
                params: vec![],
                result: k::Type::Any,
            },
            convention: lash_kernel_doc::StartConvention::Direct,
        },
    );
    document.private_bindings.insert("odd name".into());
    document
}
/// The environment is the base document's: the same source read against a
/// base that differs only outside the code gives that base back.
#[test]
fn independent_source_reads_its_environment_from_the_base_document() {
    let expected = hand_document();
    let decoded = crate::read(HAND_WRITTEN, &expected, Default::default()).unwrap();
    assert_eq!(decoded.canonical_bytes(), expected.canonical_bytes());
    roundtrip(&expected, false);
    let mut controls = vec![];
    let mut changed = expected.clone();
    changed.manifest.numbers = k::NumberPolicy::Float;
    controls.push(changed);
    let mut changed = expected.clone();
    changed.manifest.effects.insert(
        k::EffectName::new("unused.effect").unwrap(),
        k::Signature {
            params: vec![],
            result: k::Type::Any,
        },
    );
    controls.push(changed);
    let mut changed = expected.clone();
    changed.entries.clear();
    controls.push(changed);
    let mut changed = expected.clone();
    changed.private_bindings.clear();
    controls.push(changed);
    let mut changed = expected.clone();
    changed.functions.insert(
        "unused".into(),
        k::Function {
            params: vec!["param".into()],
            body: vec![k::Stmt::Return {
                value: k::Expr::Variable("param".into()),
            }],
            start: None,
        },
    );
    controls.push(changed);
    let mut changed = expected.clone();
    changed.manifest.functions.insert(
        k::FunctionId::from_bytes([11; 32]),
        k::FunctionName::new("unused.helper").unwrap(),
    );
    controls.push(changed);
    let mut changed = expected.clone();
    for byte in [12, 13] {
        changed.manifest.functions.insert(
            k::FunctionId::from_bytes([byte; 32]),
            k::FunctionName::new("same.label").unwrap(),
        );
    }
    controls.push(changed);
    for control in controls {
        assert_ne!(expected.canonical_bytes(), control.canonical_bytes());
        roundtrip(&control, false);
    }
}
/// G-SOURCE-EXACT: fixed Smith inputs and foreign-origin dependencies need no decoder library.
#[test]
fn smith_and_dialect_batches_reconstruct_without_installed_helpers() {
    for seed in 0..64u64 {
        let mut state = seed + 1;
        let bytes: Vec<u8> = (0..512)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let smith =
            lash_kernel_conformance::smith::Smith::arbitrary(&mut Unstructured::new(&bytes))
                .unwrap();
        roundtrip(&smith.document, false);
    }
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../conformance/manifest.json")).unwrap();
    let mut count = 0;
    for case in manifest["cases"].as_array().unwrap() {
        if case["status"] != "admitted" {
            continue;
        }
        let source = case["cell"]
            .as_str()
            .or_else(|| case["source"].as_str())
            .unwrap();
        let lowered =
            super::machine::lower(source).unwrap_or_else(|e| panic!("{}: {e}", case["path"]));
        roundtrip(&lowered.document, false);
        count += 1;
    }
    let mut registry = k::FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry).unwrap();
    lash_kernel_lib::register_numbers(&mut registry).unwrap();
    lash_kernel_lib::register_text_json(&mut registry).unwrap();
    lash_kernel_lib::register_collections(&mut registry).unwrap();
    lash_ext_date_ecma::register(&mut registry).unwrap();
    lash_ext_url_whatwg::register(&mut registry).unwrap();
    lash_ext_regex_ecma::register(
        &mut registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(32)),
    )
    .unwrap();
    let mut library = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    lash_dialect_typescript::define_helpers(&mut library).unwrap();
    let env = lash_kernel_dialect::Environment {
        library: &library,
        effects: &BTreeMap::new(),
        controls: &BTreeMap::new(),
        tool_roots: &BTreeSet::new(),
    };
    let ts_batch = [
        "const x = [1,2]; console.log(x[0] + 3);",
        "function twice(x) { return x + x; } console.log(twice(4));",
        "try { throw 'x'; } catch(e) { console.log(e); }",
        "const f = (x) => x + 1; console.log(f(2));",
    ];
    for source in ts_batch {
        let doc = lash_dialect_typescript::lower(
            source,
            &env,
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .unwrap()
        .document;
        roundtrip(&doc, false);
    }
    eprintln!(
        "generated corpus: smith_seeds=64 python_documents={count} typescript_documents={}",
        ts_batch.len()
    );
}
#[test]
fn bounded_name_literal_control_domain_is_exact() {
    // Exhaustive over 14 names × 10 literal variants × 4 control contexts = 560 documents.
    let names = [
        "value",
        "print",
        "BaseExceptionGroup",
        "PythonFinalizationError",
        "__build_class__",
        "eval",
        "arguments",
        "k",
        "return",
        "λ",
        "a b",
        "helper_1",
        "kernel_name_1",
        "a\"\\\n",
    ];
    let literals = [
        k::Literal::Null,
        k::Literal::Absent,
        k::Literal::Bool(true),
        k::Literal::Int((-7).into()),
        k::Literal::Float(k::Float::new(-0.0)),
        k::Literal::Float(k::Float::new(0.0)),
        k::Literal::Float(k::Float::new(f64::NEG_INFINITY)),
        k::Literal::Text("λ\n\"\\\u{2028}\u{2029}".into()),
        k::Literal::Bytes(k::Bytes::parse_hex("00ff").unwrap()),
        k::Literal::Function("declared".into()),
    ];
    for name in names {
        for literal in &literals {
            for context in 0..4 {
                let body = vec![
                    k::Stmt::Let {
                        name: name.into(),
                        value: k::Rhs::Expr(k::Expr::Literal(literal.clone())),
                    },
                    k::Stmt::Print {
                        value: k::Expr::Variable(name.into()),
                    },
                ];
                let main = match context {
                    0 => body,
                    1 => vec![k::Stmt::If {
                        condition: k::Expr::Literal(k::Literal::Bool(true)),
                        then_block: body,
                        else_block: vec![],
                    }],
                    2 => vec![k::Stmt::Try(k::TryStmt {
                        body,
                        catch: Some(k::Catch {
                            binding: "error".into(),
                            body: vec![],
                        }),
                        finally: Some(vec![]),
                    })],
                    _ => vec![
                        body[0].clone(),
                        k::Stmt::Finish {
                            value: k::Expr::Closure(Box::new(k::Closure {
                                params: vec![name.into()],
                                body: vec![k::Stmt::Return {
                                    value: k::Expr::Variable(name.into()),
                                }],
                                start: None,
                            })),
                        },
                    ],
                };
                let mut document = k::Document::new(k::NumberPolicy::Float, main);
                document
                    .functions
                    .insert("declared".into(), k::Function::default());
                let printed = roundtrip(&document, false);
                if matches!(
                    name,
                    "print"
                        | "BaseExceptionGroup"
                        | "PythonFinalizationError"
                        | "__build_class__"
                        | "eval"
                ) {
                    assert!(
                        printed
                            .source
                            .lines()
                            .any(|line| line.trim_start().starts_with("kernel_name_")
                                && line.contains(" = "))
                    );
                }
                if name == "value" || name == "λ" {
                    assert!(printed.source.contains(&format!("{name} = ")));
                }
            }
        }
    }
    eprintln!("generated bounded domain: names=14 literals=10 contexts=4 documents=560");
}
/// G-SOURCE-TRY-PRESENCE: absent handlers differ from present empty handlers.
#[test]
fn try_handler_presence_and_empty_bodies_reconstruct_exactly() {
    for catch in [false, true] {
        for finally in [false, true] {
            for empty in [false, true] {
                let document = k::Document::new(
                    k::NumberPolicy::Float,
                    vec![k::Stmt::Try(k::TryStmt {
                        body: if empty {
                            vec![]
                        } else {
                            vec![k::Stmt::Print {
                                value: k::Expr::Literal(k::Literal::Null),
                            }]
                        },
                        catch: catch.then(|| k::Catch {
                            binding: "Nonecaught".into(),
                            body: vec![],
                        }),
                        finally: finally.then(Vec::new),
                    })],
                );
                for maps in [false, true] {
                    let printed = roundtrip(&document, maps);
                    assert!(printed.source.contains(if catch || finally {
                        "try:\n"
                    } else {
                        "with k.try_("
                    }));
                    assert!(printed.source.contains("def main():"));
                }
            }
        }
    }
}

#[test]
fn maps_cover_real_sites_in_final_utf8_and_never_change_text() {
    let document = inventory_document();
    let off = roundtrip(&document, false);
    let on = roundtrip(&document, true);
    assert_eq!(off.source, on.source);
    assert!(off.source_map.is_none());
    let map = on.source_map.unwrap();
    assert_eq!(map.document, document.identity());
    assert_eq!(map.language, on.format.language());
    map.validate(&document, &on.source).unwrap();
    let mut expected = BTreeSet::new();
    for (unit, root) in std::iter::once((k::Unit::Main, k::Node::Block(document.main.as_slice())))
        .chain(document.functions.iter().map(|(name, f)| {
            (
                k::Unit::Function(name.clone()),
                k::Node::Block(f.body.as_slice()),
            )
        }))
    {
        let mut stack = vec![(k::Site::new(unit, vec![]), root)];
        while let Some((site, node)) = stack.pop() {
            for (index, child) in node.children().into_iter().enumerate() {
                stack.push((site.child(index as u32), child));
            }
            expected.insert(site);
        }
    }
    let actual: BTreeSet<_> = map.sites.iter().map(|entry| entry.site.clone()).collect();
    assert_eq!(map.sites.len(), actual.len());
    assert_eq!(actual, expected);
    for entry in map.sites {
        assert_eq!(entry.implicit, None);
        assert!(!entry.ranges.is_empty());
        for range in entry.ranges {
            assert!(range.start < range.end);
            assert!(on.source.get(range.start..range.end).is_some());
        }
    }
}
/// G-SOURCE-FORMAT: exported modules and generic syntax are closed, with
/// typed refusals naming the construct; nothing falls through to lowering.
#[test]
fn malformed_and_ambiguous_sources_refuse_with_typed_diagnostics() {
    use crate::GeneratedSourceCode as C;
    let base = hand_document();
    for source in [
        HAND_WRITTEN.to_owned() + "def main():\n    pass\n",
        HAND_WRITTEN.replace("def main():", "def main(x):"),
        HAND_WRITTEN.replace("def main():", "x = 1\ndef main():"),
        HAND_WRITTEN.replace("def worker():", "with k.declare(\"worker\", []):"),
        "def worker():\n    pass\n".into(),
    ] {
        assert_eq!(
            crate::read(&source, &base, Default::default())
                .unwrap_err()
                .code,
            C::GeneratedSourceFormat.as_str(),
            "{source}"
        );
    }
    for source in [
        HAND_WRITTEN.replace("k.int(\"7\")", "k.int(\"07\")"),
        HAND_WRITTEN.replace("k.float(\"-0.0\")", "k.float(\"bogus\")"),
        HAND_WRITTEN.replace("k.int(\"7\")", "k.bytes(\"0g\")"),
        HAND_WRITTEN.replace("k.int(\"7\")", "k.unknown()"),
        HAND_WRITTEN.replace("k.int(\"7\")", "k.clock()()"),
        HAND_WRITTEN.replace("k.int(\"7\")", "k.int(\"7\", \"8\")"),
        HAND_WRITTEN.replace("k.int(\"7\")", "k.invoke(helper_missing, [])"),
        HAND_WRITTEN.replace("k.int(\"7\")", "07"),
        HAND_WRITTEN.replace(
            "        kernel_name_1\n",
            "        kernel_name_1\n        kernel_name_1\n",
        ),
        HAND_WRITTEN.replace("    with k.return_():", "   with k.return_():"),
        HAND_WRITTEN.replace("kernel_name_1", "kernel_name_9"),
    ] {
        assert_eq!(
            crate::read(&source, &base, Default::default())
                .unwrap_err()
                .code,
            C::InvalidGeneratedSource.as_str(),
            "{source}"
        );
    }
}
#[test]
fn byte_and_depth_budgets_refuse_before_recursive_work() {
    let document = hand_document();
    let printed = roundtrip(&document, false);
    let limits = GeneratedSourceLimits {
        // Readable source can be smaller than the stored document; both
        // inputs independently obey the same budget.
        max_bytes: printed
            .source
            .len()
            .max(serde_json::to_vec(&document).unwrap().len()),
        ..Default::default()
    };
    let at_limit = crate::print(
        &document,
        PrintOptions {
            limits,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        crate::read(&at_limit.source, &document, limits)
            .unwrap()
            .canonical_bytes(),
        document.canonical_bytes()
    );
    let short = GeneratedSourceLimits {
        max_bytes: printed.source.len() - 1,
        ..limits
    };
    assert!(matches!(
        crate::print(
            &document,
            PrintOptions {
                limits: short,
                ..Default::default()
            }
        ),
        Err(crate::PrintError::Bytes { .. })
    ));
    assert_eq!(
        crate::read(&printed.source, &document, short)
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceBytes.as_str()
    );
    let exact_depth = GeneratedSourceLimits {
        max_depth: 2,
        ..Default::default()
    };
    let accepted = crate::print(
        &document,
        PrintOptions {
            limits: exact_depth,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        crate::read(&accepted.source, &document, exact_depth)
            .unwrap()
            .canonical_bytes(),
        document.canonical_bytes()
    );
    let shallow = GeneratedSourceLimits {
        max_depth: 1,
        ..Default::default()
    };
    assert!(matches!(
        crate::print(
            &document,
            PrintOptions {
                limits: shallow,
                ..Default::default()
            }
        ),
        Err(crate::PrintError::Depth { .. })
    ));
    assert_eq!(
        crate::read(&printed.source, &document, shallow)
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceDepth.as_str()
    );
    let deep = " ".repeat(300) + "with k.tuple():";
    assert_eq!(
        crate::read(&deep, &document, Default::default())
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceDepth.as_str()
    );
    let nested_type = "{\"list\":".repeat(360) + "\"any\"" + &"}".repeat(360);
    let source = format!("async def main():\n    await tools.deep(result={nested_type})\n");
    assert_eq!(
        crate::read(&source, &document, Default::default())
            .unwrap_err()
            .code,
        crate::GeneratedSourceCode::GeneratedSourceDepth.as_str()
    );
    let large_bytes = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Literal(k::Literal::Bytes(k::Bytes::new(vec![0; 2048]))),
        }],
    );
    assert!(matches!(
        crate::print(
            &large_bytes,
            PrintOptions {
                limits: GeneratedSourceLimits {
                    max_bytes: 1024,
                    ..Default::default()
                },
                ..Default::default()
            }
        ),
        Err(crate::PrintError::Bytes { .. })
    ));
    let large_integer = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Literal(k::Literal::Int(
                k::Integer::parse(&"9".repeat(2048)).unwrap(),
            )),
        }],
    );
    assert!(matches!(
        crate::print(
            &large_integer,
            PrintOptions {
                limits: GeneratedSourceLimits {
                    max_bytes: 1024,
                    ..Default::default()
                },
                ..Default::default()
            }
        ),
        Err(crate::PrintError::Bytes { .. })
    ));
    // The native ceiling accepts the deepest valid node, then refuses one extra child.
    let mut boundary = k::Expr::Literal(k::Literal::Null);
    for _ in 0..62 {
        boundary = k::Expr::Tuple(vec![boundary]);
    }
    roundtrip(
        &k::Document::new(
            k::NumberPolicy::Float,
            vec![k::Stmt::Finish {
                value: boundary.clone(),
            }],
        ),
        true,
    );
    let beyond = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Tuple(vec![boundary]),
        }],
    );
    assert!(matches!(
        crate::print(&beyond, Default::default()),
        Err(crate::PrintError::Depth { .. })
    ));
    let mut expr = k::Expr::Literal(k::Literal::Null);
    for _ in 0..80 {
        expr = k::Expr::Tuple(vec![expr]);
    }
    let deep = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish { value: expr }],
    );
    assert!(matches!(
        crate::print(&deep, Default::default()),
        Err(crate::PrintError::Depth { .. })
    ));
}

/// `K-DIALECT-001`: a place-first assignment, which this front end does not
/// write, has no generated spelling, so printing one refuses.
#[test]
fn a_place_first_assignment_does_not_print() {
    let xs = || k::Expr::Variable("xs".into());
    let document = k::Document::new(
        k::NumberPolicy::Float,
        vec![
            k::Stmt::Let {
                name: "xs".into(),
                value: k::Rhs::Expr(k::Expr::List(vec![k::Expr::Literal(k::Literal::Null)])),
            },
            k::Stmt::Assign {
                place: k::Place::Member(k::Member::Index {
                    target: xs(),
                    index: k::Expr::Literal(k::Literal::Int(0.into())),
                }),
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Null)),
                order: k::AssignOrder::PlaceFirst,
            },
            k::Stmt::Finish { value: xs() },
        ],
    );
    assert!(matches!(
        crate::print(&document, Default::default()),
        Err(crate::PrintError::Invalid(_))
    ));
}

fn inventory_document() -> k::Document {
    use k::{Action as A, Atom, Callee as C, Expr as E, Literal as L, Rhs, Stmt as S};
    let null = E::Literal(L::Null);
    let v = E::Variable("value".into());
    let id = lash_kernel_dialect::Library::resolve(super::machine::library(), "num.add").unwrap();
    let policy =
        lash_kernel_dialect::Library::resolve(super::machine::library(), "py.await").unwrap();
    let member = k::Member::Field {
        target: v.clone(),
        field: "λ".into(),
    };
    let main = vec![
        S::Let {
            name: "value".into(),
            value: Rhs::Expr(E::Record(vec![k::RecordEntry {
                field: "λ".into(),
                value: null.clone(),
            }])),
        },
        S::Assign {
            place: k::Place::Variable("value".into()),
            value: Rhs::Expr(E::List(vec![null.clone()])),
            order: k::AssignOrder::ValueFirst,
        },
        S::Assign {
            place: k::Place::Member(k::Member::Index {
                target: v.clone(),
                index: E::Literal(L::Int(0.into())),
            }),
            value: Rhs::Action(A::Yield),
            order: k::AssignOrder::ValueFirst,
        },
        S::Remove {
            member: member.clone(),
        },
        S::Print {
            value: E::Tuple(vec![
                E::Set(vec![null.clone()]),
                E::Map(vec![k::MapEntry {
                    key: E::Literal(L::Text("key".into())),
                    value: v.clone(),
                }]),
                E::Member(Box::new(member)),
                E::If {
                    cond: Box::new(E::Literal(L::Bool(true))),
                    then: Box::new(null.clone()),
                    r#else: Box::new(v.clone()),
                },
                E::Let {
                    name: "scoped λ".into(),
                    value: Box::new(v.clone()),
                    body: Box::new(E::Variable("scoped λ".into())),
                },
                E::Clock,
                E::Random,
                E::Read(Box::new(k::ProjectionRead {
                    handle: v.clone(),
                    request: null.clone(),
                })),
                E::Closure(Box::new(k::Closure {
                    params: vec!["argument".into()],
                    body: vec![S::Return {
                        value: E::Variable("argument".into()),
                    }],
                    start: None,
                })),
                E::Call {
                    function: id,
                    args: vec![E::Literal(L::Int(1.into())), E::Literal(L::Int(2.into()))],
                },
            ]),
        },
        S::If {
            condition: E::Literal(L::Bool(true)),
            then_block: vec![],
            else_block: vec![],
        },
        S::For {
            metered: 0,
            binding: "item".into(),
            iterable: E::List(vec![]),
            via: None,
            body: vec![S::Continue, S::Break],
            else_block: vec![S::Print {
                value: E::Literal(L::Bool(true)),
            }],
        },
        S::For {
            metered: 0,
            binding: "item".into(),
            iterable: E::List(vec![]),
            via: Some(k::Protocol {
                open: id,
                next: None,
                item: Some(id),
                check: None,
            }),
            body: vec![S::Print {
                value: E::Variable("item".into()),
            }],
            else_block: vec![],
        },
        S::While {
            metered: 0,
            condition: E::Literal(L::Bool(false)),
            body: vec![S::Break],
            else_block: vec![S::Print {
                value: E::Literal(L::Bool(false)),
            }],
        },
        S::Try(k::TryStmt {
            body: vec![S::Throw {
                value: E::Literal(L::Text("error".into())),
            }],
            catch: Some(k::Catch {
                binding: "error".into(),
                body: vec![S::Print {
                    value: E::Variable("error".into()),
                }],
            }),
            finally: Some(vec![]),
        }),
        S::Fail {
            value: E::Literal(L::Text("reason".into())),
        },
    ];
    let mut document = k::Document::new(k::NumberPolicy::Float, main);
    document
        .manifest
        .functions
        .insert(id, k::FunctionName::new("num.add").unwrap());
    document
        .manifest
        .functions
        .insert(policy, k::FunctionName::new("py.await").unwrap());
    let actions = vec![
        A::Call {
            callee: C::Declared("declared".into()),
            args: vec![Atom::Literal(L::Function("declared".into()))],
        },
        A::Call {
            callee: C::Library(id),
            args: vec![
                Atom::Literal(L::Float(k::Float::new(f64::INFINITY))),
                Atom::Literal(L::Float(k::Float::new(f64::NAN))),
            ],
        },
        A::Spawn {
            callee: C::Value("value".into()),
            args: vec![
                Atom::Variable("value".into()),
                Atom::Literal(L::Bytes(k::Bytes::parse_hex("00ff").unwrap())),
                Atom::Literal(L::Absent),
            ],
        },
        A::Perform {
            effect: k::EffectName::new("echo").unwrap(),
            args: vec![],
            result: inventory_types(),
            then_yield: false,
        },
        A::Sleep {
            duration: Atom::Literal(L::Int(0.into())),
            then_yield: false,
        },
        A::Perform {
            effect: k::EffectName::new("echo").unwrap(),
            args: vec![],
            result: k::Type::Any,
            then_yield: true,
        },
        A::Sleep {
            duration: Atom::Literal(L::Int(1.into())),
            then_yield: true,
        },
        A::Await {
            policy,
            value: Atom::Variable("value".into()),
        },
        A::Join {
            task: Atom::Variable("value".into()),
        },
        A::Yield,
        A::Cancel {
            task: Atom::Variable("value".into()),
        },
    ];
    document
        .main
        .extend(actions.into_iter().enumerate().flat_map(|(index, action)| {
            [
                S::Let {
                    name: k::Name::new(format!("action{index}")),
                    value: k::Rhs::Action(action.clone()),
                },
                S::Do {
                    value: action.into(),
                },
            ]
        }));
    for mode in [
        k::JoinMode::All,
        k::JoinMode::AllSettled,
        k::JoinMode::Race,
        k::JoinMode::Any,
    ] {
        let action = A::JoinMany {
            mode,
            tasks: Atom::Variable("value".into()),
        };
        document.main.push(S::Let {
            name: k::Name::new(format!("joined{mode:?}")),
            value: k::Rhs::Action(action.clone()),
        });
        document.main.push(S::Do {
            value: action.into(),
        });
    }
    document.main.push(S::ReturnVia {
        value: null.clone(),
        policy,
    });
    document.main.push(S::Finish {
        value: null.clone(),
    });
    document.functions.insert(
        "declared".into(),
        k::Function {
            params: vec!["optional".into()],
            body: vec![S::Return { value: null }],
            start: None,
        },
    );
    document.private_bindings.insert("value".into());
    document.entries.insert(
        "declared".into(),
        lash_kernel_doc::Entry {
            signature: k::Signature {
                params: vec![k::Param {
                    name: "optional".into(),
                    ty: k::Type::Any,
                    optional: true,
                }],
                result: k::Type::Any,
            },
            convention: lash_kernel_doc::StartConvention::Direct,
        },
    );
    document.manifest.effects.insert(
        k::EffectName::new("echo").unwrap(),
        k::Signature {
            params: vec![],
            result: inventory_types(),
        },
    );
    document
}

// Independent inventory: no codec rule table or wildcard variant/field matches.
fn inventory_node(node: k::Node<'_>, forms: &mut BTreeSet<&'static str>) {
    let tag = match node {
        k::Node::Block(_) => "block",
        k::Node::Stmt(stmt) => match stmt {
            k::Stmt::Let { name, value } => {
                let _ = (name, value);
                "let"
            }
            k::Stmt::Assign {
                place,
                value,
                order,
            } => {
                let _ = (value, order);
                match place {
                    k::Place::Variable(name) => {
                        let _ = name;
                        forms.insert("place.variable");
                    }
                    k::Place::Member(member) => {
                        inventory_member(member);
                        forms.insert("place.member");
                    }
                }
                "assign"
            }
            k::Stmt::Remove { member } => {
                inventory_member(member);
                "remove"
            }
            k::Stmt::Do { value } => {
                let _ = value;
                "do"
            }
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let _ = (condition, then_block, else_block);
                "if"
            }
            k::Stmt::For {
                binding,
                iterable,
                via,
                body,
                else_block,
                metered,
            } => {
                let _ = (binding, iterable, via, body, else_block, metered);
                "for"
            }
            k::Stmt::While {
                condition,
                body,
                else_block,
                metered,
            } => {
                let _ = (condition, body, else_block, metered);
                "while"
            }
            k::Stmt::Break => "break",
            k::Stmt::Continue => "continue",
            k::Stmt::ReturnVia { value, policy } => {
                let _ = (value, policy);
                "return-via"
            }
            k::Stmt::Return { value } => {
                let _ = value;
                "return"
            }
            k::Stmt::Try(k::TryStmt {
                body,
                catch,
                finally,
            }) => {
                let _ = (body, finally);
                if let Some(k::Catch { binding, body }) = catch {
                    let _ = (binding, body);
                }
                "try"
            }
            k::Stmt::Throw { value } => {
                let _ = value;
                "throw"
            }
            k::Stmt::Print { value } => {
                let _ = value;
                "print"
            }
            k::Stmt::Finish { value } => {
                let _ = value;
                "finish"
            }
            k::Stmt::Fail { value } => {
                let _ = value;
                "fail"
            }
        },
        k::Node::Expr(expr) => match expr {
            k::Expr::Literal(l) => {
                inventory_literal(l, forms);
                "literal"
            }
            k::Expr::Variable(name) => {
                let _ = name;
                "variable"
            }
            k::Expr::Tuple(items) => {
                let _ = items;
                "tuple"
            }
            k::Expr::List(items) => {
                let _ = items;
                "list"
            }
            k::Expr::Set(items) => {
                let _ = items;
                "set"
            }
            k::Expr::Map(items) => {
                for k::MapEntry { key, value } in items {
                    let _ = (key, value);
                }
                "map"
            }
            k::Expr::Record(items) => {
                for k::RecordEntry { field, value } in items {
                    let _ = (field, value);
                }
                "record"
            }
            k::Expr::Member(member) => {
                inventory_member(member);
                "member"
            }
            k::Expr::Closure(closure) => {
                // Source spells no start adapter: the reader carries the
                // base's (`carry_start_adapters`).
                let k::Closure {
                    params,
                    body,
                    start,
                } = closure.as_ref();
                let _ = (params, body, start);
                "closure"
            }
            k::Expr::Call { function, args } => {
                let _ = (function, args);
                "invoke"
            }
            k::Expr::If { .. } => "if_expression",
            k::Expr::Let { .. } => "let_expression",
            k::Expr::Clock => "clock",
            k::Expr::Random => "random",
            k::Expr::Read(read) => {
                let k::ProjectionRead { handle, request } = read.as_ref();
                let _ = (handle, request);
                "read"
            }
            k::Expr::Act(act) => {
                let k::ActExpr { kind, args } = act.as_ref();
                let _ = (kind, args);
                "act"
            }
        },
        k::Node::Action(action) => match action {
            k::Action::Call { callee, args } => {
                inventory_callee(callee, forms);
                for a in args {
                    inventory_atom(a, forms);
                }
                "call"
            }
            k::Action::Spawn { callee, args } => {
                inventory_callee(callee, forms);
                for a in args {
                    inventory_atom(a, forms);
                }
                "spawn"
            }
            k::Action::Perform {
                effect,
                args,
                result,
                then_yield,
            } => {
                let _ = (effect, result);
                for a in args {
                    inventory_atom(a, forms);
                }
                if *then_yield {
                    forms.insert("perform.then_yield");
                }
                "perform"
            }
            k::Action::Sleep {
                duration,
                then_yield,
            } => {
                inventory_atom(duration, forms);
                if *then_yield {
                    forms.insert("sleep.then_yield");
                }
                "sleep"
            }
            k::Action::Await { policy, value } => {
                let _ = policy;
                inventory_atom(value, forms);
                "await"
            }
            k::Action::Join { task } => {
                inventory_atom(task, forms);
                "join"
            }
            k::Action::Cancel { task } => {
                inventory_atom(task, forms);
                "cancel"
            }
            k::Action::JoinMany { mode, tasks } => {
                inventory_atom(tasks, forms);
                forms.insert(match mode {
                    k::JoinMode::All => "mode.all",
                    k::JoinMode::AllSettled => "mode.settled",
                    k::JoinMode::Race => "mode.race",
                    k::JoinMode::Any => "mode.any",
                });
                "joinMany"
            }
            k::Action::Yield => "yield",
        },
    };
    forms.insert(tag);
    for child in node.children() {
        inventory_node(child, forms);
    }
}
fn inventory_member(member: &k::Member) {
    match member {
        k::Member::Field { target, field } => {
            let _ = (target, field);
        }
        k::Member::Index { target, index } => {
            let _ = (target, index);
        }
    }
}
fn inventory_callee(callee: &k::Callee, forms: &mut BTreeSet<&'static str>) {
    forms.insert(match callee {
        k::Callee::Declared(name) => {
            let _ = name;
            "callee.declared"
        }
        k::Callee::Value(name) => {
            let _ = name;
            "callee.value"
        }
        k::Callee::Library(id) => {
            let _ = id;
            "callee.library"
        }
    });
}
fn inventory_atom(atom: &k::Atom, forms: &mut BTreeSet<&'static str>) {
    let tag = match atom {
        k::Atom::Variable(name) => {
            let _ = name;
            "atom.variable"
        }
        k::Atom::Literal(l) => {
            inventory_literal(l, forms);
            "atom.literal"
        }
    };
    forms.insert(tag);
}
fn inventory_literal(literal: &k::Literal, forms: &mut BTreeSet<&'static str>) {
    forms.insert(match literal {
        k::Literal::Null => "lit.null",
        k::Literal::Absent => "lit.absent",
        k::Literal::Bool(v) => {
            let _ = v;
            "lit.bool"
        }
        k::Literal::Int(v) => {
            let _ = v;
            "lit.int"
        }
        k::Literal::Float(v) => {
            let _ = v;
            "lit.float"
        }
        k::Literal::Text(v) => {
            let _ = v;
            "lit.text"
        }
        k::Literal::Bytes(v) => {
            let _ = v;
            "lit.bytes"
        }
        k::Literal::Function(v) => {
            let _ = v;
            "lit.function"
        }
    });
}
#[test]
fn independent_inventory_covers_kernel_variants_and_stored_fields() {
    let document = inventory_document();
    let k::Document {
        manifest,
        functions,
        entries,
        private_bindings,
        main,
    } = &document;
    let k::Manifest {
        kernel,
        effects,
        functions: requirements,
        numbers,
    } = manifest;
    let _ = (
        kernel,
        effects,
        requirements,
        numbers,
        entries,
        private_bindings,
    );
    let mut forms = BTreeSet::new();
    inventory_node(k::Node::Block(main), &mut forms);
    for k::Function {
        params,
        body,
        start,
    } in functions.values()
    {
        let _ = (params, start);
        inventory_node(k::Node::Block(body), &mut forms);
    }
    let expected = [
        "block",
        "let",
        "assign",
        "remove",
        "do",
        "if",
        "for",
        "while",
        "break",
        "continue",
        "return",
        "return-via",
        "try",
        "throw",
        "print",
        "finish",
        "fail",
        "literal",
        "variable",
        "if_expression",
        "let_expression",
        "tuple",
        "list",
        "set",
        "map",
        "record",
        "member",
        "closure",
        "invoke",
        "clock",
        "random",
        "read",
        "act",
        "call",
        "spawn",
        "perform",
        "sleep",
        "join",
        "cancel",
        "joinMany",
        "yield",
        "await",
        "perform.then_yield",
        "sleep.then_yield",
        "place.variable",
        "place.member",
        "callee.declared",
        "callee.value",
        "callee.library",
        "atom.variable",
        "atom.literal",
        "mode.all",
        "mode.settled",
        "mode.race",
        "mode.any",
        "lit.null",
        "lit.absent",
        "lit.bool",
        "lit.int",
        "lit.float",
        "lit.text",
        "lit.bytes",
        "lit.function",
    ];
    assert_eq!(forms, expected.into_iter().collect());
    // Rhs is stored even though it is not a Node.
    for stmt in main {
        if let k::Stmt::Let { value, .. } | k::Stmt::Assign { value, .. } = stmt {
            match value {
                k::Rhs::Expr(expr) => {
                    let _ = expr;
                }
                k::Rhs::Action(action) => {
                    let _ = action;
                }
            }
        }
    }
    let mut types = BTreeSet::new();
    for signature in effects
        .values()
        .chain(entries.values().map(|entry| &entry.signature))
    {
        inventory_signature(signature, &mut types);
    }
    assert_eq!(
        types,
        [
            "any",
            "null",
            "absent",
            "bool",
            "int",
            "float",
            "number",
            "text",
            "bytes",
            "timestamp",
            "tuple",
            "list",
            "map",
            "set",
            "record",
            "enum",
            "function",
            "task",
            "error",
            "handle",
            "union"
        ]
        .into_iter()
        .collect()
    );
    roundtrip(&document, false);
}

fn inventory_types() -> k::Type {
    use k::Type as T;
    T::Tuple(vec![
        T::Any,
        T::Null,
        T::Absent,
        T::Bool,
        T::Int,
        T::Float,
        T::Number,
        T::Text,
        T::Bytes,
        T::Timestamp,
        T::Tuple(vec![]),
        T::List(Box::new(T::Text)),
        T::Map(Box::new(k::MapType {
            key: T::Int,
            value: T::Any,
        })),
        T::Set(Box::new(T::Int)),
        T::Record(k::RecordType {
            fields: vec![k::RecordTypeField {
                name: "field".into(),
                ty: T::Bool,
                optional: true,
            }],
            rest: Some(Box::new(T::Any)),
        }),
        T::Enum(vec!["λ".into(), "other".into()]),
        T::Function(Box::new(k::Signature {
            params: vec![k::Param {
                name: "arg".into(),
                ty: T::Int,
                optional: true,
            }],
            result: T::Text,
        })),
        T::Task(Box::new(T::Any)),
        T::Error,
        T::Handle("projection".into()),
        T::Union(vec![T::Int, T::Float]),
    ])
}
fn inventory_type(ty: &k::Type, variants: &mut BTreeSet<&'static str>) {
    use k::Type as T;
    let tag = match ty {
        T::Any => "any",
        T::Null => "null",
        T::Absent => "absent",
        T::Bool => "bool",
        T::Int => "int",
        T::Float => "float",
        T::Number => "number",
        T::Text => "text",
        T::Bytes => "bytes",
        T::Timestamp => "timestamp",
        T::Tuple(items) => {
            for t in items {
                inventory_type(t, variants);
            }
            "tuple"
        }
        T::Union(items) => {
            for t in items {
                inventory_type(t, variants);
            }
            "union"
        }
        T::List(t) => {
            inventory_type(t, variants);
            "list"
        }
        T::Set(t) => {
            inventory_type(t, variants);
            "set"
        }
        T::Task(t) => {
            inventory_type(t, variants);
            "task"
        }
        T::Map(t) => {
            let k::MapType { key, value } = t.as_ref();
            inventory_type(key, variants);
            inventory_type(value, variants);
            "map"
        }
        T::Record(k::RecordType { fields, rest }) => {
            for k::RecordTypeField { name, ty, optional } in fields {
                let _ = (name, optional);
                inventory_type(ty, variants);
            }
            if let Some(t) = rest {
                inventory_type(t, variants);
            }
            "record"
        }
        T::Enum(values) => {
            let _ = values;
            "enum"
        }
        T::Function(signature) => {
            inventory_signature(signature, variants);
            "function"
        }
        T::Error => "error",
        T::Handle(kind) => {
            let _ = kind;
            "handle"
        }
    };
    variants.insert(tag);
}
fn inventory_signature(signature: &k::Signature, variants: &mut BTreeSet<&'static str>) {
    let k::Signature { params, result } = signature;
    for k::Param { name, ty, optional } in params {
        let _ = (name, optional);
        inventory_type(ty, variants);
    }
    inventory_type(result, variants);
}

/// G-SOURCE-RESOURCES: alias compression must not permit unbounded decoded-name allocation.
#[test]
fn alias_expansion_refuses_before_cloning_an_over_budget_document() {
    // The base document's long illegal name is renamed `kernel_name_1`.
    let long: k::Name = "a ".repeat(256).as_str().into();
    let base = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Variable(long),
        }],
    );
    let mut source = "def main():\n".to_owned();
    for _ in 0..100 {
        source.push_str("    with k.finish():\n        kernel_name_1\n");
    }
    source.push_str("    with k.finish():\n        k.unknown()\n");
    let limits = GeneratedSourceLimits {
        max_bytes: source.len(),
        ..Default::default()
    };
    assert_eq!(
        crate::read(&source, &base, limits).unwrap_err().code,
        crate::GeneratedSourceCode::GeneratedSourceBytes.as_str()
    );
}

mod acceptance;
mod readable;

/// A result other than `any` keeps its nested optional type exactly; the
/// manifest stays unprinted.
#[test]
fn non_any_effect_results_keep_nested_optional_types_and_the_manifest_stays_unprinted() {
    let mut document = hand_document();
    let ty = k::Type::Record(k::RecordType {
        fields: vec![k::RecordTypeField {
            name: "True false None".into(),
            ty: k::Type::Bool,
            optional: true,
        }],
        rest: Some(Box::new(k::Type::Any)),
    });
    let effect = k::EffectName::new("tools.inspect").unwrap();
    document.manifest.effects.insert(
        effect.clone(),
        k::Signature {
            params: vec![k::Param {
                name: "input".into(),
                ty: ty.clone(),
                optional: true,
            }],
            result: k::Type::Any,
        },
    );
    document.main.push(k::Stmt::Do {
        value: (k::Action::Perform {
            effect,
            args: vec![],
            result: ty,
            then_yield: false,
        })
        .into(),
    });
    let source = roundtrip(&document, true).source;
    assert!(
        !source.contains("k.") && !source.contains("\"numbers\""),
        "{source}"
    );
    assert!(!source.contains("\\\""), "{source}");
    assert!(source.contains("result={\"record\":"), "{source}");
    assert!(
        source.contains("True false None"),
        "quoted metadata is not token-converted"
    );
}

/// FIG-6332: the authored importer cannot resolve a printed document's helpers.
#[test]
fn authored_import_of_printed_source_needs_its_base() {
    let document =
        super::machine::lower("def digest(inbox):\n    return inbox\nprint(digest('hello'))\n")
            .unwrap()
            .document;
    let printed = crate::print(&document, Default::default()).unwrap();
    let refusal = super::machine::lower(&printed.source).unwrap_err();
    assert_eq!(
        refusal.code,
        crate::Code::PrintedSourceNeedsBase.as_str(),
        "{refusal}"
    );
    assert_eq!(refusal.kind, lash_kernel_dialect::DiagnosticKind::Refusal);
    assert!(refusal.message.contains("python::read(source, base)"));
    assert!(refusal.span.is_some());
    assert!(
        refusal
            .repairs
            .iter()
            .any(|repair| repair.contains("python::read(source, base)"))
    );
    for source in [
        "helper_py_add(1, 2)",
        "f = declared.digest",
        "declared.digest()",
        "x = k.absent",
        "k.int('1')",
        "kernel_name_1",
    ] {
        let refusal = super::machine::lower(source).unwrap_err();
        assert_eq!(
            refusal.code,
            crate::Code::PrintedSourceNeedsBase.as_str(),
            "{source}: {refusal}"
        );
    }
    super::machine::lower(
        "def helper_py_add(x, y):\n    return x + y\ndeclared = 1\nk = 2\nkernel_name_1 = 3\nhelper_py_add(declared, k)\n",
    ).expect("authored bindings with printer spellings still resolve");
    super::machine::lower("text = 'helper_py_add declared.digest k.absent'\n")
        .expect("quoted data is not a printed reference");
    assert_eq!(
        super::machine::lower("missing\n").unwrap_err().code,
        crate::Code::UnknownName.as_str()
    );
    let read = crate::read(&printed.source, &document, Default::default()).unwrap();
    assert_eq!(read.identity(), document.identity());
}

/// FIG-6471 / G-SOURCE-EXACT: discarded expressions have exact source inverses.
#[test]
fn discarded_expressions_have_exact_source_inverses() {
    let document = k::parse_document(
        r#"kernel 1
numbers float
effect show(value: Any) -> Any
fn child() { return null }
main {
  do perform show({text: "x"}) as Any
  do {text: "pure"}
  do clock
  do fn() start() -> Any bare direct { return null }
  do let(x = 1, x)
  do [1, yield]
  do cancel(spawn call child())
  do join all([spawn call child()])
  finish null
}
"#,
    )
    .unwrap();
    let printed = roundtrip(&document, true);
    assert!(printed.source.contains("tools.show"), "{}", printed.source);
}
