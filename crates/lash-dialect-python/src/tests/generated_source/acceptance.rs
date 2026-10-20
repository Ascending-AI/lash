//! Independent readability detection over real mapped kernel Nodes.
use super::k;
use lash_kernel_dialect::readability::{self, Case, Dialect, Unavoidable};

fn block_expression(node: k::Node<'_>) -> bool {
    matches!(
        node,
        k::Node::Expr(
            k::Expr::Closure(_) | k::Expr::Read(_) | k::Expr::If { .. } | k::Expr::Let { .. }
        )
    ) || node.children().into_iter().any(block_expression)
}
fn generic(node: k::Node<'_>, source: &str) -> Option<&'static str> {
    let source = source.trim_start();
    if source.starts_with("with k.") {
        return Some(match node {
            k::Node::Block(_) if source.starts_with("with k.block(") => "explicit-block",
            k::Node::Block(_) => return None, // A direct Block starts with its first child.
            k::Node::Expr(k::Expr::Closure(_)) => "block-expression",
            k::Node::Expr(k::Expr::Read(_)) => "projection-read",
            k::Node::Expr(_) if block_expression(node) => "expression-child-suite",
            k::Node::Stmt(k::Stmt::Try(scope))
                if scope.catch.is_none() && scope.finally.is_none() =>
            {
                "handlerless-try"
            }
            k::Node::Stmt(
                k::Stmt::Let {
                    value: k::Rhs::Expr(k::Expr::Closure(_)),
                    ..
                }
                | k::Stmt::Assign {
                    place: k::Place::Variable(_),
                    value: k::Rhs::Expr(k::Expr::Closure(_)),
                    ..
                },
            ) => "avoidable",
            k::Node::Stmt(_) if block_expression(node) => "statement-child-suite",
            _ => "avoidable",
        });
    }
    if source.starts_with("k.") {
        return Some(match node {
            k::Node::Expr(k::Expr::Literal(k::Literal::Absent)) => "absent",
            k::Node::Expr(k::Expr::Clock) => "clock",
            k::Node::Expr(k::Expr::Random) => "random",
            k::Node::Action(
                k::Action::Spawn { .. }
                | k::Action::Sleep { .. }
                | k::Action::Join { .. }
                | k::Action::JoinMany { .. }
                | k::Action::Cancel { .. }
                | k::Action::Yield,
            ) => "task-action",
            k::Node::Block(_)
            | k::Node::Stmt(k::Stmt::Do { .. })
            | k::Node::Expr(k::Expr::Member(_)) => return None, // The Action owns its spelling.
            _ => "avoidable",
        });
    }
    // Atomic operands have no child Sites. Detect their absent spelling here,
    // skipping quoted strings so text containing "k.absent" never flags a form.
    if let k::Node::Action(action) = node {
        let mut quoted = false;
        let mut escaped = false;
        for (i, ch) in source.char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quoted = false;
                }
            } else if ch == '"' {
                quoted = true;
            } else if source[i..].starts_with("k.absent") {
                let absent = |atom: &k::Atom| matches!(atom, k::Atom::Literal(k::Literal::Absent));
                return Some(match action {
                    k::Action::Call { args, .. } | k::Action::Perform { args, .. }
                        if args.iter().any(absent) =>
                    {
                        "absent-atom"
                    }
                    _ => "avoidable",
                });
            }
        }
    }
    None
}
/// Python's kernel-surface printer.
struct Surface;
impl lash_kernel_dialect::readability::SurfacePrinter for Surface {
    fn print(
        &self,
        document: &k::Document,
        captions: Option<&k::Captions>,
        options: lash_kernel_dialect::PrintOptions,
    ) -> Result<lash_kernel_dialect::PrintedSource, lash_kernel_dialect::PrintError> {
        crate::print_kernel(document, captions, options)
    }
}
static PRINTER: Surface = Surface;
fn dialect(unavoidable: &[Unavoidable]) -> Dialect<'_> {
    Dialect {
        name: "python",
        printer: &PRINTER,
        decode: crate::read_kernel,
        generic,
        unavoidable,
    }
}

/// FIG-6272: a real agent cell prints as the Python an agent writes: no
/// header, declarations, casts or tables, its typed tool signatures staying
/// in the document, and reads back against it to the same identity.
#[test]
fn deep_research_agent_cell_prints_as_plain_python_and_reads_back_exactly() {
    let document = readability::deep_research().expect("the deep_research document decodes");
    let printed = super::roundtrip(&document, false);
    assert_eq!(
        printed.source,
        r#"async def deep_research():
    search_input = record({"topic": "Reducing customer onboarding drop-off", "max_sources": 3})
    evidence = await research.search(search_input)
    synthesis_input = record({"topic": evidence.topic, "sources": evidence.sources})
    report = await research.synthesize(synthesis_input)
    display_input = record({"text": report.summary})
    await display.show_message(display_input)
    return report
def main():
    return None
"#
    );
    let rules = unavoidable();
    let report = readability::check(
        &[Case {
            name: "deep_research",
            document,
        }],
        &[dialect(&rules)],
    );
    report.assert_passed();
    assert!(
        report
            .generic_forms
            .iter()
            .all(|form| form.rule == "record-kind"),
        "{:?}",
        report.generic_forms
    );
}

/// G-SOURCE-READABLE: seven ordinary shapes permit no generic body forms.
#[test]
fn shared_ordinary_corpus_has_only_named_distinctions_and_exact_bytes() {
    let cases = readability::corpus().expect("valid shared readability corpus");
    let rules = unavoidable();
    let report = readability::check(&cases, &[dialect(&rules)]);
    report.assert_passed();
    assert_eq!(report.documents, 8);
    eprintln!(
        "shared Python: documents={} fallbacks={:?}",
        report.documents, report.generic_forms
    );
    for case in cases {
        let printed = super::roundtrip(&case.document, false);
        if case.name == "three_effect_workflow" {
            assert_eq!(
                printed.source,
                r#"async def deep_research():
    search_input = record({"topic": "Reducing customer onboarding drop-off", "max_sources": 3})
    evidence = await research.search(search_input)
    synthesis_input = record({"topic": evidence.topic, "sources": evidence.sources})
    synthesis = await research.synthesize(synthesis_input)
    message = record({"text": synthesis.summary})
    await display.show_message(message)
    return synthesis
def main():
    return None
"#
            );
        }
        assert!(
            !printed.source.lines().any(|line| line.contains("k.")),
            "{}:\n{}",
            case.name,
            printed.source
        );
    }
}

// This inventory is independent of printer eligibility. Every admitted reason
// names the exact Node kind and a kernel/Python distinction from the rule docs.
fn unavoidable() -> Vec<Unavoidable> {
    let mut rules = vec![
        Unavoidable {
            rule: "absent",
            node_kind: "Literal",
            reason: "None is Null; Absent has no native value",
        },
        Unavoidable {
            rule: "block-expression",
            node_kind: "Closure",
            reason: "Python has no statement block expression",
        },
        Unavoidable {
            rule: "projection-read",
            node_kind: "Read",
            reason: "Dedicated kernel projection read is not a Python library call",
        },
        Unavoidable {
            rule: "clock",
            node_kind: "Clock",
            reason: "Kernel clock is a dedicated nondeterministic expression",
        },
        Unavoidable {
            rule: "random",
            node_kind: "Random",
            reason: "Kernel random is a dedicated nondeterministic expression",
        },
        Unavoidable {
            rule: "handlerless-try",
            node_kind: "Try",
            reason: "Python try requires a handler; kernel records its absence",
        },
        Unavoidable {
            rule: "explicit-block",
            node_kind: "Block",
            reason: "A generic parent's child boundaries require an explicit Block",
        },
    ];
    for node_kind in [
        "Tuple",
        "List",
        "Set",
        "Map",
        "Record",
        "Member",
        "ExpressionCall",
        "IfExpression",
        "LetExpression",
        "Call",
        "Perform",
        "Spawn",
        "Sleep",
        "Join",
        "JoinMany",
        "Await",
    ] {
        rules.push(Unavoidable {
            rule: "expression-child-suite",
            node_kind,
            reason: "A block-expression child cannot be embedded without changing boundaries",
        });
    }
    for node_kind in [
        "Let", "Assign", "Remove", "If", "For", "While", "Return", "Throw", "Print", "Finish",
        "Fail",
    ] {
        rules.push(Unavoidable {
            rule: "statement-child-suite",
            node_kind,
            reason: "A block-expression child needs its exact visible child suite",
        });
    }
    for node_kind in ["Sleep", "Join", "JoinMany", "Yield", "Spawn", "Cancel"] {
        rules.push(Unavoidable {
            rule: "task-action",
            node_kind,
            reason: "Kernel task modes, timing units and actions differ from Python coroutines",
        });
    }
    rules.push(Unavoidable {
        rule: "declared-callee",
        node_kind: "Spawn",
        reason: "A declared kernel callee is distinct from a variable with the same name",
    });
    for node_kind in ["Call", "Perform"] {
        rules.push(Unavoidable {
            rule: "absent-atom",
            node_kind,
            reason: "Atomic Absent differs from Null and invents no expression Site",
        });
    }
    for (rule, node_kind, reason) in [
        (
            "record-kind",
            "Record",
            "A dict is Map; ordered kernel Record fields need their own constructor",
        ),
        (
            "declared-callee",
            "Call",
            "A declared kernel callee is distinct from a variable with the same name",
        ),
        (
            "helper_foreign_identity",
            "ExpressionCall",
            "The foreign helper identity has no Python builtin or operator",
        ),
        (
            "binding-kind",
            "Assign",
            "An initial Assign must not decode as the first Let",
        ),
        (
            "binding-kind",
            "Let",
            "A repeated Let must not decode as Assign",
        ),
        (
            "binding-kind",
            "Closure",
            "A closure annotation retains its exceptional enclosing binding kind",
        ),
        (
            "return-kind",
            "Return",
            "Main Return differs from main Finish",
        ),
        (
            "finish-kind",
            "Finish",
            "Function Finish differs from function Return",
        ),
        ("failure-kind", "Fail", "Kernel Fail differs from Throw"),
        (
            "helper_kind",
            "ExpressionCall",
            "Kernel value tags differ from Python class objects returned by type",
        ),
        (
            "helper_text_join",
            "ExpressionCall",
            "Kernel text.join takes an ordered value sequence, with distinct argument order and errors",
        ),
        (
            "helper_list_len",
            "ExpressionCall",
            "Strict list length refuses other collections that Python len accepts",
        ),
    ] {
        rules.push(Unavoidable {
            rule,
            node_kind,
            reason,
        });
    }
    for (rule, reason) in [
        (
            "helper_int_add",
            "Integer-only add and generic numeric add can share operand facts but have distinct FunctionIds",
        ),
        (
            "helper_int_lt",
            "Integer-only and generic numeric comparison can share operand facts but have distinct FunctionIds",
        ),
        (
            "helper_text_concat",
            "Unknown operand facts cannot distinguish this strict text identity from numeric or dynamic addition",
        ),
    ] {
        rules.push(Unavoidable {
            rule,
            node_kind: "ExpressionCall",
            reason,
        });
    }
    rules.push(Unavoidable { rule: "unary-identity", node_kind: "ExpressionCall", reason: "Grouping distinguishes a unary helper Call from a signed Literal without adding stored nodes" });
    // Library actions print as the builtin or method an author writes
    // (`len(x)`, `xs.append(v)`, `x[a:b]`, `type(x).__name__`). These have
    // no authored spelling that is the node's exact inverse.
    for (rule, reason) in [
        (
            "helper_py_add",
            "`x + y` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_py_mul",
            "`x * y` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_py_neg",
            "`-x` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_py_mod",
            "`x % y` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_py_floordiv",
            "`x // y` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        (
            "helper_py_pow",
            "`x ** y` is the expression operator; this is the action lowering emits for dynamic operands",
        ),
        ("helper_py_getitem", "`x[i]` is the kernel Index member"),
        (
            "helper_py_setitem",
            "`x[i] = v` is the kernel member Assign",
        ),
        (
            "helper_py_iadd",
            "`x += y` also assigns; the stored node is only the call",
        ),
        (
            "helper_py_exc_args",
            "`error.args` is the kernel Field member",
        ),
        (
            "helper_py_show",
            "print's text is formatted before the Print statement, which has its own spelling",
        ),
        (
            "helper_py_arg_missing",
            "a missing-argument check lowering inserts; no source spells it",
        ),
        (
            "helper_py_exc_matches",
            "an except-clause class test lowering inserts; no source spells it",
        ),
        (
            "helper_py_exc_new",
            "an exception construction lowering inserts; no source spells it",
        ),
        (
            "helper_py_fail",
            "a typed failure lowering raises; no source spells it",
        ),
        (
            "helper_py_name_check",
            "an unbound-name check lowering inserts; no source spells it",
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

/// G-SOURCE-READABLE: authored documents retain only documented kernel distinctions.
#[test]
fn authored_python_corpus_has_no_avoidable_generic_forms() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../conformance/manifest.json")).unwrap();
    let mut cases = vec![];
    for case in manifest["cases"].as_array().unwrap() {
        if case["status"] != "admitted" {
            continue;
        }
        let source = case["cell"]
            .as_str()
            .or_else(|| case["source"].as_str())
            .unwrap();
        let lowered = super::super::machine::lower(source).unwrap();
        cases.push(Case {
            name: "authored-python",
            document: lowered.document.into_document(),
        });
    }
    let rules = unavoidable();
    let report = readability::check(&cases, &[dialect(&rules)]);
    eprintln!(
        "authored Python: documents={} unavoidable={:?}",
        report.documents, report.generic_forms
    );
    report.assert_passed();
    assert_eq!(report.documents, 28);
}

/// G-SOURCE-VALID: exports are accepted by CPython, not just the paired cursor.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the law is a host: it compiles exported source under CPython"
)]
fn exported_python_corpora_compile_under_cpython() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut cases = readability::corpus().unwrap();
    cases.push(Case {
        name: "deep_research",
        document: readability::deep_research().unwrap(),
    });
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../conformance/manifest.json")).unwrap();
    for case in manifest["cases"].as_array().unwrap() {
        if case["status"] == "admitted" {
            let source = case["cell"]
                .as_str()
                .or_else(|| case["source"].as_str())
                .unwrap();
            cases.push(Case {
                name: "authored-python",
                document: super::super::machine::lower(source)
                    .unwrap()
                    .document
                    .into_document(),
            });
        }
    }
    cases.push(Case {
        name: "variant-inventory",
        document: super::inventory_document(),
    });
    for seed in 0..64u64 {
        use arbitrary::{Arbitrary, Unstructured};
        let mut state = seed + 1;
        let bytes: Vec<_> = (0..512)
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
        cases.push(Case {
            name: "smith",
            document: smith.document,
        });
    }
    let sources: Vec<_> = cases
        .iter()
        .map(|case| {
            let printed = super::roundtrip(&case.document, true);
            (case.name, printed.source)
        })
        .collect();
    let mut child = Command::new("python3").args(["-c", "import json,sys\nfor name,source in json.load(sys.stdin):\n compile(source, name, 'exec')\nprint('CPython exports compiled')"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        // A hard dependency, not a skip: the test executors provide python3
        // (docs/design/generated-source-python.md, "Proof").
        .expect("this law runs CPython: python3 must be on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&sources).unwrap())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    eprintln!("CPython compiled {} corpus documents", sources.len());
}

/// G-SOURCE-EXACT: only a missing argument inverse permits a child suite.
#[test]
fn nested_actions_use_child_suites_only_without_an_inline_inverse() {
    let id = k::FunctionId::from_bytes([91; 32]);
    let closure = k::Expr::Closure(Box::new(k::Closure {
        params: vec![],
        body: vec![k::Stmt::Return {
            value: k::Expr::Literal(k::Literal::Null),
        }],
        start: None,
    }));
    let argument = k::Expr::If {
        cond: Box::new(k::Expr::Literal(k::Literal::Bool(true))),
        then: Box::new(k::Expr::Literal(k::Literal::Int(0.into()))),
        r#else: Box::new(k::Expr::Literal(k::Literal::Int(1.into()))),
    };
    let kinds = [
        k::ActKind::Call(k::Callee::Declared("worker".into())),
        k::ActKind::Call(k::Callee::Value("f".into())),
        k::ActKind::Call(k::Callee::Library(id)),
        k::ActKind::Spawn(k::Callee::Declared("worker".into())),
        k::ActKind::Spawn(k::Callee::Library(id)),
        k::ActKind::Perform {
            effect: k::EffectName::new("host.echo").unwrap(),
            result: k::Type::Text,
            then_yield: true,
        },
        k::ActKind::Sleep { then_yield: true },
        k::ActKind::Join,
        k::ActKind::JoinMany(k::JoinMode::AllSettled),
        k::ActKind::Await(id),
    ];
    let mut cases = vec![];
    for kind in kinds {
        let args = match &kind {
            k::ActKind::Call(_) => vec![closure.clone()],
            k::ActKind::Perform { .. } => vec![k::Expr::Record(vec![k::RecordEntry {
                field: "projection".into(),
                value: k::Expr::Read(Box::new(k::ProjectionRead {
                    handle: k::Expr::Variable("handle".into()),
                    request: k::Expr::Literal(k::Literal::Null),
                })),
            }])],
            k::ActKind::Await(_) => vec![k::Expr::Let {
                name: "scoped".into(),
                value: Box::new(argument.clone()),
                body: Box::new(k::Expr::Variable("scoped".into())),
            }],
            _ => vec![argument.clone()],
        };
        let mut document = k::Document::new(
            k::NumberPolicy::BySpelling,
            vec![k::Stmt::Finish {
                value: k::Expr::Act(Box::new(k::ActExpr { kind, args })),
            }],
        );
        document
            .manifest
            .functions
            .insert(id, k::FunctionName::new("foreign.identity").unwrap());
        let printed = super::roundtrip(&document, true);
        assert!(printed.source.contains("with k."), "{}", printed.source);
        cases.push(Case {
            name: "argument_without_inline_inverse",
            document,
        });
    }
    readability::check(&cases, &[dialect(&unavoidable())]).assert_passed();
}
