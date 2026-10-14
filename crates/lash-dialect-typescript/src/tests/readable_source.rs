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
        (k::Literal::Int(7.into()), "finish(7);"),
        (k::Literal::Int((-7).into()), "finish(-7);"),
        (
            k::Literal::Int(9_007_199_254_740_991i64.into()),
            "finish(9007199254740991);",
        ),
        (
            k::Literal::Int(9_007_199_254_740_992i64.into()),
            "9007199254740992n",
        ),
        (k::Literal::Float(k::Float::new(7.0)), "finish(7.0);"),
        (k::Literal::Float(k::Float::new(-0.0)), "finish(-0.0);"),
        (k::Literal::Float(k::Float::new(f64::INFINITY)), "Infinity"),
        (k::Literal::Float(k::Float::new(f64::NAN)), "NaN"),
        (k::Literal::Absent, "finish(undefined);"),
        (k::Literal::Null, "finish(null);"),
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
    assert!(spelling(k::Expr::List(items.clone())).contains("finish([1, 2]);"));
    assert!(spelling(k::Expr::Tuple(items.clone())).contains("[1, 2] as const"));
    assert!(spelling(k::Expr::Set(items)).contains("new Set([1, 2])"));
    assert!(spelling(k::Expr::Map(vec![])).contains("new Map([])"));
    assert!(spelling(k::Expr::Record(vec![])).contains("finish({});"));
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
    assert!(spelling(record(&["not a field"])).contains("{\"not a field\": 0}"));
    assert!(spelling(record(&["__proto__"])).contains("{[\"__proto__\"]: 0}"));
    assert!(spelling(record(&["ready", "ready"])).contains("Object.fromEntries(["));
}

#[test]
fn native_members_preserve_field_index_and_assignment_targets() {
    let field = |name: &str| k::Member::Field {
        target: variable("page"),
        field: name.into(),
    };
    assert!(spelling(k::Expr::Member(Box::new(field("ready")))).contains("page.ready"));
    assert!(
        spelling(k::Expr::Member(Box::new(field("not a field"))))
            .contains("page[\"not a field\" as const]")
    );
    let index = k::Member::Index {
        target: variable("page"),
        index: literal(k::Literal::Text("ready".into())),
    };
    assert!(spelling(k::Expr::Member(Box::new(index.clone()))).contains("page[\"ready\"]"));
    let tuple_index = k::Expr::Member(Box::new(k::Member::Index {
        target: variable("page"),
        index: k::Expr::Tuple(vec![integer(1), integer(2)]),
    }));
    assert!(spelling(tuple_index).contains("page[[1, 2] as const]"));
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
    assert!(source.contains("page.value = 1;"));
    assert!(source.contains("page[\"not a field\" as const] = 2;"));
    assert!(source.contains("page[\"ready\"] = 3;"));
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
    assert!(source.contains("let action = await helper_foreign_same(1);"));
    assert!(source.contains("spawn(helper_foreign_same_2, []);"));
    assert!(source.contains("await result();"));
    assert!(source.contains("await (result)();"));
    // Identities stay in the base document, never in the source.
    assert_eq!(source.matches(&id.to_string()).count(), 0);
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
            "  for (let item of [λ]) {\n    if (item) {\n      console.log(item);\n    }\n  }"
        ),
        "{}",
        off.source
    );
    assert!(!off.source.contains(" else "));
    assert!(off.source.contains("while (false)"));
    assert!(off.source.contains("catch (error)"));
    assert!(off.source.contains("finally"));
    assert!(off.source.contains("k.try(async function("));
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
            .contains(" else {\n      await yieldTask();")
    );
}

#[test]
fn identifier_aliases_are_injective_across_shadowing_and_reserved_names() {
    for (original, expected) in [
        ("λ", "λ"),
        ("useful", "useful"),
        ("return", "kernel_name_1"),
        ("k", "kernel_name_1"),
        ("main", "kernel_name_1"),
        ("tools", "kernel_name_1"),
        ("finish", "kernel_name_1"),
        ("declared", "kernel_name_1"),
        ("console", "kernel_name_1"),
        ("Math", "kernel_name_1"),
        ("Date", "kernel_name_1"),
        ("Object", "kernel_name_1"),
        ("Promise", "kernel_name_1"),
        ("Uint8Array", "kernel_name_1"),
        ("projection", "kernel_name_1"),
        ("helper_label", "kernel_name_1"),
        ("kernel_name_1", "kernel_name_2"),
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
    assert!(source.contains("let kernel_name_2 = 1;"));
    assert!(source.contains("let kernel_name_3 = 2;"));
}

// Independent reader witness: these native forms are not obtained from print().
// The base document carries the environment; the source is only code.
fn native_base() -> k::Document {
    k::Document::new(k::NumberPolicy::Float, vec![])
}

#[test]
fn independent_native_source_has_exact_numeric_collection_and_member_inverse() {
    let source = "async function main() { finish([7, 7.0, -0.0, [1, 2], {λ: true}, page.λ, page[\"λ\"]] as const); }";
    let decoded = crate::read(source, &native_base(), Default::default()).unwrap();
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
        "page.x++",
        "Date.now(1)",
        "Math.random(1)",
        "projection.read(page)",
        "new Uint8Array([256])",
        "new Set(1)",
        "new Map([[1]])",
        "Object.fromEntries([[1, 2]])",
        "async function(page: any) { return page; }",
        "function named() { return null; }",
    ] {
        let source = format!("async function main() {{ finish({expression}); }}");
        assert!(
            crate::read(&source, &native_base(), Default::default()).is_err(),
            "{source}"
        );
    }
}
// The independently pinned spelling is also a production-reader witness.
const ORDINARY_WORKFLOW: &str = r#"export async function deep_research() {
  let search_input = {topic: "Reducing customer onboarding drop-off", max_sources: 3};
  let evidence = await tools.research.search(search_input);
  let synthesis_input = {topic: evidence.topic, sources: evidence.sources};
  let synthesis = await tools.research.synthesize(synthesis_input);
  let message = {text: synthesis.summary};
  await tools.display.show_message(message);
  return synthesis;
}
async function main() {
  finish(null);
}
"#;

// FIG-6250: ordinary workflows must select native rules and retain every byte.
#[test]
fn ordinary_three_effect_workflow_has_no_generic_body_forms() {
    let document = k::parse_document(
        r#"kernel 1
numbers float
effect research.search(input: Any) -> Any
effect research.synthesize(input: Any) -> Any
effect display.show_message(input: Any) -> Any
entry deep_research() -> Any
fn deep_research() {
  let search_input = {topic: "Reducing customer onboarding drop-off", max_sources: 3}
  let evidence = perform research.search(search_input) as Any
  let synthesis_input = {topic: evidence.topic, sources: evidence.sources}
  let synthesis = perform research.synthesize(synthesis_input) as Any
  let message = {text: synthesis.summary}
  do perform display.show_message(message) as Any
  return synthesis
}
main {
  finish null
}
"#,
    )
    .unwrap();
    let source = roundtrip(&document, true).source;
    assert_eq!(source, ORDINARY_WORKFLOW);
    assert_eq!(
        crate::read(ORDINARY_WORKFLOW, &document, Default::default())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        document.canonical_bytes().unwrap()
    );
    assert!(
        !source.contains("k.") && !source.contains(" as "),
        "{source}"
    );
}

/// FIG-6272: a real agent cell prints as the TypeScript an agent writes: no
/// header, declarations, casts or tables, its typed tool signatures staying
/// in the document, and reads back against it to the same identity.
#[test]
fn deep_research_agent_cell_prints_as_plain_typescript_and_reads_back_exactly() {
    use lash_kernel_dialect::readability::{self, Case, Dialect};
    let document = readability::deep_research().expect("the deep_research document decodes");
    let source = roundtrip(&document, false).source;
    assert_eq!(
        source,
        r#"export async function deep_research() {
  let search_input = {topic: "Reducing customer onboarding drop-off", max_sources: 3};
  let evidence = await tools.research.search(search_input);
  let synthesis_input = {topic: evidence.topic, sources: evidence.sources};
  let report = await tools.research.synthesize(synthesis_input);
  let display_input = {text: report.summary};
  await tools.display.show_message(display_input);
  return report;
}
async function main() {
  finish(null);
}
"#
    );
    let report = readability::check(
        &[Case {
            name: "deep_research",
            document,
        }],
        &[Dialect {
            name: "typescript",
            printer: &crate::TypeScript::default(),
            decode: generated_decoder,
            generic: generic_form,
            unavoidable: &unavoidable(),
        }],
    );
    report.assert_passed();
    assert!(
        report
            .generic_forms
            .iter()
            .all(|form| form.rule == "finish-kind"),
        "{:?}",
        report.generic_forms
    );
}

fn generated_decoder(
    source: &str,
    base: &k::Document,
    limits: lash_kernel_dialect::GeneratedSourceLimits,
) -> Result<k::Document, lash_kernel_dialect::Diagnostic> {
    crate::read(source, base, limits).map_err(Into::into)
}
fn generic_form(node: k::Node<'_>, source: &str) -> Option<&'static str> {
    let source = source.trim_start();
    match node {
        k::Node::Stmt(k::Stmt::Try(_)) if source.starts_with("k.try(") => Some("handlerless-try"),
        _ if source.starts_with("k.") => Some("undocumented-kernel-form"),
        _ => None,
    }
}
fn unavoidable() -> Vec<lash_kernel_dialect::readability::Unavoidable> {
    use lash_kernel_dialect::readability::Unavoidable;
    let mut rules = vec![];
    for (rule, node_kind, reason) in [
        (
            "finish-kind",
            "Finish",
            "Kernel Finish differs from Return, including in main",
        ),
        (
            "declared-callee",
            "Call",
            "Declared kernel functions differ from variable callees",
        ),
        (
            "helper_foreign_identity",
            "ExpressionCall",
            "The foreign helper has no TypeScript builtin/operator spelling",
        ),
        (
            "helper_same",
            "ExpressionCall",
            "Kernel identity differs from structural eq, which shares the JavaScript strict-equality token",
        ),
        (
            "helper_kind",
            "ExpressionCall",
            "Kernel value tags differ from JavaScript typeof tags",
        ),
        (
            "helper_record_contains",
            "ExpressionCall",
            "Kernel own-record membership differs from prototype-inclusive JavaScript in",
        ),
        (
            "helper_list_len",
            "ExpressionCall",
            "Strict kernel list length differs from a polymorphic JavaScript length field",
        ),
        (
            "record-order",
            "Record",
            "Duplicate ordered Record fields cannot be spelled as a native object literal",
        ),
        (
            "result-type",
            "Perform",
            "An effect result differing from its manifest needs its exact kernel type",
        ),
    ] {
        rules.push(Unavoidable {
            rule,
            node_kind,
            reason,
        });
    }
    rules.push(Unavoidable { rule: "unary-identity", node_kind: "ExpressionCall", reason: "Grouping distinguishes a unary helper Call from a signed Literal without adding stored nodes" });
    for node_kind in ["Spawn", "Sleep", "Join", "JoinMany", "Cancel", "Yield"] {
        rules.push(Unavoidable { rule: "task-action", node_kind, reason: "Kernel task handles, modes and durations are distinct from JavaScript promises and require explicit task primitives" });
    }
    rules.push(Unavoidable {
        rule: "projection-read",
        node_kind: "Read",
        reason: "Kernel projection read is a dedicated expression, not an ordinary library call",
    });
    // `console.log(...args)` and `target.name(...args)` print as authored.
    // These library actions have no authored spelling that is the node's
    // exact inverse.
    for (rule, reason) in [
        (
            "helper_ts_add",
            "`a + b` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_ts_not",
            "`!a` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_ts_get",
            "`a[k]` and `a.k` are the kernel Index and Field members",
        ),
        (
            "helper_ts_receiver",
            "a receiver classification lowering inserts; no source spells it",
        ),
        (
            "helper_ts_pad",
            "argument padding lowering inserts; no source spells it",
        ),
        (
            "helper_ts_iterate",
            "an iteration step lowering inserts; no source spells it",
        ),
        (
            "helper_ts_array_iterator",
            "an iterator lowering inserts; no source spells it",
        ),
        (
            "helper_ts_map_value",
            "a map-entry projection lowering inserts; no source spells it",
        ),
        (
            "helper_ts_member_name",
            "a property-key conversion lowering inserts; no source spells it",
        ),
    ] {
        rules.push(Unavoidable {
            rule,
            node_kind: "Call",
            reason,
        });
    }
    rules
}
#[test]
fn shared_realistic_corpus_has_only_named_distinctions_and_exact_typescript() {
    use lash_kernel_dialect::readability::{self, Dialect};
    let corpus = readability::corpus().expect("valid shared readability corpus");
    let printer = crate::TypeScript::default();
    let report = readability::check(
        &corpus,
        &[Dialect {
            name: "typescript",
            printer: &printer,
            decode: generated_decoder,
            generic: generic_form,
            unavoidable: &unavoidable(),
        }],
    );
    eprintln!(
        "shared readability: documents={} generic_forms={} failures={:?}",
        report.documents,
        report.generic_forms.len(),
        report.failures
    );
    report.assert_passed();
    assert_eq!(report.documents, 7);
    assert!(
        report
            .generic_forms
            .iter()
            .all(|form| form.reason != "no documented unavoidable reason")
    );
    for case in &corpus {
        let source = roundtrip(&case.document, false).source;
        assert!(!source.contains("k."), "{}: {source}", case.name);
    }
}

#[test]
fn authored_typescript_corpus_reprints_without_generic_body_forms() {
    // Reuse representative authored sources from language, array, async_fn and
    // controls laws, including their visible lowered temporaries and tokens.
    let cases: Vec<_> = [
        "let x = 1 + 2 * 3; console.log(x);",
        "function twice(x) { return x + x; } console.log(twice(4));",
        "let rows = [{name: 'first'}, {name: 'second'}]; for (const row of rows) { console.log(row.name); }",
        "let i = 0; while (i < 3) { i = i + 1; if (i === 2) { continue; } console.log(i); }",
        "try { throw 'bad'; } catch (error) { console.log(error); } finally { console.log('done'); }",
        "let result = await echo(4); console.log(result);",
    ].into_iter().map(|source| lash_kernel_dialect::readability::Case { name: "authored-typescript", document: super::lower_with_effects(source).unwrap().document }).collect();
    let report = lash_kernel_dialect::readability::check(
        &cases,
        &[lash_kernel_dialect::readability::Dialect {
            name: "typescript",
            printer: &crate::TypeScript::default(),
            decode: generated_decoder,
            generic: generic_form,
            unavoidable: &unavoidable(),
        }],
    );
    eprintln!(
        "authored TypeScript: documents={} fallbacks={:?}",
        report.documents, report.generic_forms
    );
    report.assert_passed();
}

#[test]
fn native_operator_aliases_preserve_helper_identity_precedence_and_ambiguity() {
    let add = k::FunctionId::from_bytes([41; 32]);
    let multiply = k::FunctionId::from_bytes([42; 32]);
    let negate = k::FunctionId::from_bytes([43; 32]);
    let helper = |function, args| k::Expr::Call { function, args };
    let mut document = k::Document::new(
        k::NumberPolicy::Float,
        vec![k::Stmt::Finish {
            value: k::Expr::Tuple(vec![
                helper(
                    add,
                    vec![integer(1), helper(multiply, vec![integer(2), integer(3)])],
                ),
                helper(
                    multiply,
                    vec![helper(add, vec![integer(1), integer(2)]), integer(3)],
                ),
                helper(
                    add,
                    vec![integer(1), helper(add, vec![integer(2), integer(3)])],
                ),
                helper(negate, vec![integer(7)]),
                integer(-7),
            ]),
        }],
    );
    for (id, name) in [(add, "ts.add"), (multiply, "ts.mul"), (negate, "ts.neg")] {
        document
            .manifest
            .functions
            .insert(id, k::FunctionName::new(name).unwrap());
    }
    let printed = roundtrip(&document, true).source;
    assert!(
        printed.contains("finish([1 + 2 * 3, (1 + 2) * 3, 1 + (2 + 3), -(7), -7] as const);"),
        "{printed}"
    );
    document.manifest.functions.insert(
        k::FunctionId::from_bytes([44; 32]),
        k::FunctionName::new("ts.add").unwrap(),
    );
    let ambiguous = roundtrip(&document, true).source;
    assert!(ambiguous.contains("helper_ts_add(1, 2 * 3)"), "{ambiguous}");
    let independent = "async function main() { finish([1 + 2 * 3, (1 + 2) * 3, 1 + (2 + 3), -(7), -7] as const); }";
    document
        .manifest
        .functions
        .remove(&k::FunctionId::from_bytes([44; 32]));
    assert_eq!(
        crate::read(independent, &document, Default::default())
            .unwrap()
            .canonical_bytes()
            .unwrap(),
        document.canonical_bytes().unwrap()
    );
    // Unary expressions on the left of exponentiation need an outer group;
    // the inner numeric group still distinguishes a helper from a literal.
    let power = k::FunctionId::from_bytes([45; 32]);
    document
        .manifest
        .functions
        .insert(power, k::FunctionName::new("ts.pow").unwrap());
    document.main = vec![k::Stmt::Finish {
        value: k::Expr::List(vec![
            helper(power, vec![helper(negate, vec![integer(7)]), integer(2)]),
            helper(power, vec![integer(-7), integer(2)]),
            helper(
                power,
                vec![integer(2), helper(power, vec![integer(3), integer(2)])],
            ),
        ]),
    }];
    let powered = roundtrip(&document, true).source;
    assert!(
        powered.contains("finish([(-(7)) ** 2, (-7) ** 2, 2 ** 3 ** 2]);"),
        "{powered}"
    );
}

#[test]
fn effect_calls_keep_arities_result_annotations_and_atomic_boundaries() {
    let effect = k::EffectName::new("research.search").unwrap();
    let mut document = k::Document::new(k::NumberPolicy::Float, vec![]);
    document.manifest.effects.insert(
        effect.clone(),
        k::Signature {
            params: vec![],
            result: k::Type::Any,
        },
    );
    for (args, result) in [
        (vec![], k::Type::Any),
        (
            vec![k::Atom::Literal(k::Literal::Text("topic".into()))],
            k::Type::Any,
        ),
        (
            vec![
                k::Atom::Variable("first".into()),
                k::Atom::Variable("second".into()),
            ],
            k::Type::List(Box::new(k::Type::Text)),
        ),
    ] {
        document.main.push(k::Stmt::Do {
            action: k::Action::Perform {
                effect: effect.clone(),
                args,
                result,
            },
        });
    }
    let printed = roundtrip(&document, true).source;
    assert!(
        printed.contains("await tools.research.search();"),
        "{printed}"
    );
    assert!(
        printed.contains("await tools.research.search(\"topic\");"),
        "{printed}"
    );
    assert!(
        printed.contains(
            "await tools.research.search(first, second) as KernelType<{\"list\":\"text\"}>;"
        ),
        "{printed}"
    );
    // The manifest is the base document's: without a signature an `any`
    // result still needs no annotation.
    document.manifest.effects.clear();
    let missing = roundtrip(&document, true).source;
    assert_eq!(missing, printed, "{missing}");
    for expression in [
        "await tools.research.search([1])",
        "await sleep()",
        "await tools.research.search() as string",
        "await (first + second)()",
        "spawn(first, [1,,2])",
    ] {
        let source = format!("async function main() {{ let first = {expression}; }}");
        assert!(
            crate::read(&source, &document, Default::default()).is_err(),
            "{source}"
        );
    }
}

/// G-SOURCE-READABLE: authored typed operators have native exact inverses.
#[test]
fn typed_authored_arithmetic_and_comparison_export_native_operators() {
    for source in [
        "function twice(x) { return x + x; } console.log(twice(4));",
        "let x = 1 + 2 * 3; console.log(x);",
        "let i = 0; while (i < 3) { i = i + 1; }",
        "let s = 'a' + 'b'; let n = 1 + 2; console.log(s, n);",
    ] {
        let document = super::lower_with_effects(source).unwrap().document;
        let printed = roundtrip(&document, true).source;
        assert!(
            !printed.contains("helper_num_") && !printed.contains("helper_text_concat"),
            "{printed}"
        );
    }
}
