//! G-SOURCE-EXACT, G-SOURCE-FORMAT, G-SOURCE-RESOURCES and G-SOURCE-MAP.
use crate::{GeneratedSourceLimits, PrintOptions};
use arbitrary::{Arbitrary, Unstructured};
use lash_kernel_doc as k;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn roundtrip(document: &k::Document, maps: bool) -> crate::PrintedSource {
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
    assert_eq!(document.identity().unwrap(), decoded.identity().unwrap());
    assert_eq!(
        document.canonical_bytes().unwrap(),
        decoded.canonical_bytes().unwrap(),
        "{}",
        printed.source
    );
    printed
}
// Independently written syntax, not obtained from the printer or its rule table.
// Nothing but code: the base document holds the environment, including the
// rename of the illegal identifier `odd name`.
const HAND_WRITTEN: &str = r#"export async function worker() { return 7; }
async function main() { let kernel_name_1 = -0.0; finish(kernel_name_1); }"#;
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
        },
    );
    document.entries.insert(
        "worker".into(),
        k::Signature {
            params: vec![],
            result: k::Type::Any,
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
    assert_eq!(
        decoded.canonical_bytes().unwrap(),
        expected.canonical_bytes().unwrap()
    );
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
        assert_ne!(
            expected.canonical_bytes().unwrap(),
            control.canonical_bytes().unwrap()
        );
        let printed = roundtrip(&control, false);
        if control.functions == expected.functions && control.entries == expected.entries {
            assert_eq!(printed.source, roundtrip(&expected, false).source);
        }
    }
}
#[test]
fn smith_64_fixed_inputs_and_python_lowered_documents_reconstruct_without_helpers() {
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
    let mut registry = super::kernel_registry();
    let mut library = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    for helper in lash_dialect_python::define_helpers(&mut library).unwrap() {
        registry.register(helper, None).unwrap();
    }
    let library = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    let environment = lash_kernel_dialect::Environment {
        library: &library,
        effects: &BTreeMap::new(),
        controls: &BTreeMap::new(),
        tool_roots: &BTreeSet::new(),
    };
    for source in [
        "x = [1, 2]\nprint(x[0] + 3)",
        "def twice(x):\n    return x + x\nprint(twice(4))",
    ] {
        let lowered = lash_dialect_python::lower(
            source,
            &environment,
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .unwrap();
        roundtrip(&lowered.document, false);
    }
    eprintln!("generated source: smith_seeds=64 python_documents=2");
}
#[test]
fn bounded_name_literal_control_domain_is_exact() {
    // Exhaustive over 10 names × 8 literal variants × 3 control contexts = 240 documents.
    let names = [
        "value",
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
        k::Literal::Text("λ\n\"\\\u{2028}\u{2029}".into()),
        k::Literal::Bytes(k::Bytes::parse_hex("00ff").unwrap()),
        k::Literal::Function("declared".into()),
    ];
    for name in names {
        for literal in &literals {
            for context in 0..3 {
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
                    _ => vec![k::Stmt::Try(k::TryStmt {
                        body,
                        catch: Some(k::Catch {
                            binding: "error".into(),
                            body: vec![],
                        }),
                        finally: Some(vec![]),
                    })],
                };
                let mut document = k::Document::new(k::NumberPolicy::Float, main);
                document
                    .functions
                    .insert("declared".into(), k::Function::default());
                roundtrip(&document, false);
            }
        }
    }
    eprintln!("generated bounded domain: names=10 literals=8 contexts=3 documents=240");
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
                            binding: "caught".into(),
                            body: vec![],
                        }),
                        finally: finally.then(Vec::new),
                    })],
                );
                for maps in [false, true] {
                    let printed = roundtrip(&document, maps);
                    assert_eq!(printed.source.contains("k.try("), !catch && !finally);
                }
            }
        }
    }
}

/// G-SOURCE-MAP: scoped expressions keep document child indices even though
/// TypeScript writes the function body before its argument.
#[test]
fn scoped_expression_maps_keep_value_and_body_sites_in_document_order() {
    let document = k::parse_document(
        r#"kernel 1
numbers float
main { finish let(value = ["λ"], if(true, value, [null])) }
"#,
    )
    .unwrap();
    let off = roundtrip(&document, false);
    let on = roundtrip(&document, true);
    assert_eq!(off.source, on.source);
    let map = on.source_map.unwrap();
    map.validate(&document, &on.source).unwrap();
    for (path, expected) in [
        (vec![0, 0, 0], r#"["λ"]"#),
        (vec![0, 0, 0, 0], r#""λ""#),
        (vec![0, 0, 1], "(true ? value : [null])"),
        (vec![0, 0, 1, 0], "true"),
        (vec![0, 0, 1, 1], "value"),
        (vec![0, 0, 1, 2], "[null]"),
    ] {
        let site = k::Site::new(k::Unit::Main, path);
        let mapped = map.sites.iter().find(|entry| entry.site == site).unwrap();
        assert_eq!(mapped.ranges.len(), 1, "{site}");
        let span = mapped.ranges[0];
        assert_eq!(&on.source[span.start..span.end], expected, "{site}");
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
    assert_eq!(map.document, document.identity().unwrap());
    assert_eq!(map.language, on.format.language());
    map.validate(&document, &on.source).unwrap();
    use sha2::Digest;
    assert_eq!(
        map.source,
        <[u8; 32]>::from(sha2::Sha256::digest(on.source.as_bytes()))
    );
    let mut expected = BTreeMap::new();
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
            expected.insert(
                site,
                matches!(node, k::Node::Block(body) if body.is_empty()),
            );
        }
    }
    let actual: BTreeSet<_> = map.sites.iter().map(|entry| entry.site.clone()).collect();
    assert_eq!(map.sites.len(), actual.len());
    assert_eq!(actual, expected.keys().cloned().collect());
    let mut implicit = 0;
    for entry in map.sites {
        if let Some(anchor) = entry.implicit {
            implicit += 1;
            assert!(expected[&entry.site], "only empty blocks can be implicit");
            assert!(entry.ranges.is_empty());
            assert!(on.source.is_char_boundary(anchor));
        } else {
            assert!(!entry.ranges.is_empty());
        }
        for range in entry.ranges {
            assert!(range.start < range.end);
            assert!(on.source.get(range.start..range.end).is_some());
        }
    }
    assert_eq!(implicit, 1, "inventory has one omitted empty else");
}
/// An edit the reader has no exact inverse for is a typed refusal naming
/// the construct; nothing falls through to authored lowering.
#[test]
fn malformed_exported_modules_refuse_with_typed_diagnostics() {
    let base = hand_document();
    for (source, expected) in [
        (
            format!("{HAND_WRITTEN}\nlet extra = 1;"),
            "expected native function declarations and main",
        ),
        (
            "export async function worker() { return 7; }".to_owned(),
            "missing main",
        ),
        (
            HAND_WRITTEN.replace("export async", "async"),
            "is an entry in the base document",
        ),
        (
            HAND_WRITTEN.replace("async function main", "export async function main"),
            "main is not an entry",
        ),
        (
            HAND_WRITTEN.replace("let kernel_name_1 =", "var kernel_name_1 ="),
            "not a kernel source statement",
        ),
        (
            HAND_WRITTEN.replace("function worker()", "function worker(): any"),
            "annotat",
        ),
        (
            HAND_WRITTEN.replace("return 7;", "return helper_unknown(7);"),
            "undeclared helper alias",
        ),
        (
            HAND_WRITTEN.replace(
                "let kernel_name_1 = -0.0;",
                "let kernel_name_1 = [await yieldTask()];",
            ),
            "",
        ),
        (
            HAND_WRITTEN.replace(
                "let kernel_name_1 = -0.0;",
                "let kernel_name_1 = await sleep([1]);",
            ),
            "",
        ),
        (
            "let x = 1;".to_owned(),
            "expected native function declarations and main",
        ),
    ] {
        let error = crate::read(&source, &base, Default::default()).unwrap_err();
        assert!(
            matches!(
                error.code,
                crate::DiagnosticCode::GeneratedSourceFormat | crate::DiagnosticCode::InvalidAst
            ),
            "{source}: {error}"
        );
        assert!(error.message.contains(expected), "{source}: {error}");
    }
    let mut extra_entry = base.clone();
    extra_entry.functions.insert(
        "helper".into(),
        k::Function {
            params: vec![],
            body: vec![],
        },
    );
    let printed = roundtrip(&extra_entry, false);
    let exported = printed
        .source
        .replace("async function helper", "export async function helper");
    assert!(
        crate::read(&exported, &extra_entry, Default::default())
            .unwrap_err()
            .message
            .contains("is not an entry in the base document")
    );
}
#[test]
fn byte_and_depth_budgets_refuse_before_recursive_work() {
    let document = hand_document();
    let printed = roundtrip(&document, false);
    let limits = GeneratedSourceLimits {
        max_bytes: printed.source.len().max(document.to_json().unwrap().len()),
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
            .canonical_bytes()
            .unwrap(),
        document.canonical_bytes().unwrap()
    );
    let short = GeneratedSourceLimits {
        max_bytes: limits.max_bytes - 1,
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
        crate::DiagnosticCode::GeneratedSourceBytes
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
            .canonical_bytes()
            .unwrap(),
        document.canonical_bytes().unwrap()
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
        crate::DiagnosticCode::GeneratedSourceDepth
    );
    let deep = "async function main() {".to_owned() + &"(".repeat(300);
    assert_eq!(
        crate::read(&deep, &document, Default::default())
            .unwrap_err()
            .code,
        crate::DiagnosticCode::GeneratedSourceDepth
    );
    let nested_type = "{list: ".repeat(360) + "\"any\"" + &"}".repeat(360);
    let source =
        format!("async function main() {{ await tools.deep() as KernelType<{nested_type}>; }}");
    assert_eq!(
        crate::read(&source, &document, Default::default())
            .unwrap_err()
            .code,
        crate::DiagnosticCode::GeneratedSourceDepth
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

fn inventory_document() -> k::Document {
    use k::{Action as A, Atom, Callee as C, Expr as E, Literal as L, Rhs, Stmt as S};
    let null = E::Literal(L::Null);
    let v = E::Variable("value".into());
    let id = lash_kernel_dialect::Library::resolve(super::library(), "num.add").unwrap();
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
        },
        S::Assign {
            place: k::Place::Member(k::Member::Index {
                target: v.clone(),
                index: E::Literal(L::Int(0.into())),
            }),
            value: Rhs::Action(A::Yield),
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
            binding: "item".into(),
            iterable: E::List(vec![]),
            body: vec![S::Continue, S::Break],
        },
        S::While {
            condition: E::Literal(L::Bool(false)),
            body: vec![S::Break],
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
        },
        A::Sleep {
            duration: Atom::Literal(L::Int(0.into())),
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
        .extend(actions.into_iter().map(|action| S::Do { action }));
    for mode in [
        k::JoinMode::All,
        k::JoinMode::AllSettled,
        k::JoinMode::Race,
        k::JoinMode::Any,
    ] {
        document.main.push(S::Do {
            action: A::JoinMany {
                mode,
                tasks: Atom::Variable("value".into()),
            },
        });
    }
    document.main.push(S::Finish {
        value: null.clone(),
    });
    document.functions.insert(
        "declared".into(),
        k::Function {
            params: vec!["optional".into()],
            body: vec![S::Return { value: null }],
        },
    );
    document.private_bindings.insert("value".into());
    document.entries.insert(
        "declared".into(),
        k::Signature {
            params: vec![k::Param {
                name: "optional".into(),
                ty: k::Type::Any,
                optional: true,
            }],
            result: k::Type::Any,
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
            k::Stmt::Assign { place, value } => {
                let _ = value;
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
            k::Stmt::Do { action } => {
                let _ = action;
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
                body,
            } => {
                let _ = (binding, iterable, body);
                "for"
            }
            k::Stmt::While { condition, body } => {
                let _ = (condition, body);
                "while"
            }
            k::Stmt::Break => "break",
            k::Stmt::Continue => "continue",
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
                let k::Closure { params, body } = closure.as_ref();
                let _ = (params, body);
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
            } => {
                let _ = (effect, result);
                for a in args {
                    inventory_atom(a, forms);
                }
                "perform"
            }
            k::Action::Sleep { duration } => {
                inventory_atom(duration, forms);
                "sleep"
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
    for k::Function { params, body } in functions.values() {
        let _ = params;
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
        "call",
        "spawn",
        "perform",
        "sleep",
        "join",
        "cancel",
        "joinMany",
        "yield",
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
    for signature in effects.values().chain(entries.values()) {
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

/// G-SOURCE-ENVELOPE: native literals carry nested optional types exactly.
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
        action: k::Action::Perform {
            effect,
            args: vec![],
            result: ty,
        },
    });
    let source = roundtrip(&document, true).source;
    assert!(
        !source.contains("k.") && !source.contains("\"numbers\""),
        "{source}"
    );
    assert!(!source.contains("\\\""), "{source}");
    assert!(source.contains("as KernelType<{\"record\":"), "{source}");
    assert!(
        source.contains("True false None"),
        "quoted metadata is not token-converted"
    );
}

/// FIG-6332: printed helper identities need the document they came from.
#[test]
fn authored_import_of_printed_source_needs_its_base() {
    let environment = lash_kernel_dialect::Environment {
        library: super::library(),
        effects: &BTreeMap::new(),
        controls: &BTreeMap::new(),
        tool_roots: &BTreeSet::new(),
    };
    let module = lash_kernel_dialect::Mode::Module;
    let document = crate::lower(
        "export async function digest(inbox: string) { return inbox; }",
        &environment,
        module,
    )
    .unwrap()
    .document;
    let printed = crate::print(&document, Default::default()).unwrap();
    let refusal = crate::lower(&printed.source, &environment, module).unwrap_err();
    assert_eq!(
        refusal.code,
        crate::DiagnosticCode::PrintedSourceNeedsBase,
        "{refusal}"
    );
    assert_eq!(refusal.kind, crate::DiagnosticKind::Refusal);
    assert!(refusal.message.contains("typescript::read(source, base)"));
    assert!(refusal.span.is_some());
    assert!(
        refusal
            .suggestions
            .iter()
            .any(|repair| repair.contains("typescript::read(source, base)"))
    );
    for source in [
        "helper_list_len([]);",
        "const f = declared.digest;",
        "kernel_name_1;",
    ] {
        let refusal = crate::lower(source, &environment, module).unwrap_err();
        assert_eq!(
            refusal.code,
            crate::DiagnosticCode::PrintedSourceNeedsBase,
            "{source}: {refusal}"
        );
    }
    crate::lower(
        "function helper_list_len(x) { return x; } const declared = {digest: 1}; const kernel_name_1 = 1; helper_list_len([declared.digest, kernel_name_1]); export async function main() { return null; }",
        &environment,
        module,
    ).expect("authored bindings with printer spellings still resolve");
    crate::lower(
        "const text = 'helper_list_len declared.digest'; export async function main() { return null; }",
        &environment,
        module,
    )
    .expect("quoted data is not a printed reference");
    assert_eq!(
        crate::lower("missing;", &environment, module)
            .unwrap_err()
            .code,
        crate::DiagnosticCode::UnknownBinding
    );
    let read = crate::read(&printed.source, &document, Default::default()).unwrap();
    assert_eq!(read.identity().unwrap(), document.identity().unwrap());
}
