//! Independent readability detection over real mapped kernel Nodes.
use super::k;
use lash_kernel_dialect::readability::{self, Case, Dialect, Unavoidable};

fn block_expression(node: k::Node<'_>) -> bool {
    matches!(node, k::Node::Expr(k::Expr::Closure(_) | k::Expr::Read(_)))
        || node.children().into_iter().any(block_expression)
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
static PRINTER: crate::Python = crate::Python {
    limits: crate::SourceLimits::standard(),
};
fn dialect(unavoidable: &[Unavoidable]) -> Dialect<'_> {
    Dialect {
        name: "python",
        printer: &PRINTER,
        decode: crate::compile_generated,
        generic,
        unavoidable,
    }
}

/// G-SOURCE-READABLE: seven ordinary shapes permit no generic body forms.
#[test]
fn shared_ordinary_corpus_has_only_named_distinctions_and_exact_bytes() {
    let cases = readability::corpus().expect("valid shared readability corpus");
    let rules = unavoidable();
    let report = readability::check(&cases, &[dialect(&rules)]);
    report.assert_passed();
    assert_eq!(report.documents, 7);
    eprintln!(
        "shared Python: documents={} fallbacks={:?}",
        report.documents, report.generic_forms
    );
    for case in cases {
        let printed = super::roundtrip(&case.document, false);
        if case.name == "three_effect_workflow" {
            assert_eq!(
                printed.source.split_once('\n').unwrap().1,
                r#"async def deep_research():
    search_input = record({"topic": "Reducing customer onboarding drop-off", "max_sources": 3})
    evidence = await tools.research.search(search_input)
    synthesis_input = record({"topic": evidence.topic, "sources": evidence.sources})
    synthesis = await tools.research.synthesize(synthesis_input)
    message = record({"text": synthesis.summary})
    await tools.display.show_message(message)
    return synthesis
def main():
    return None
"#
            );
        }
        let header = printed.source.lines().next().unwrap();
        assert!(!header.contains("\\\""), "{}: {header}", case.name);
        assert!(
            !printed
                .source
                .lines()
                .skip(1)
                .any(|line| line.contains("k.")),
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
    for rule in [
        "helper_py_add",
        "helper_py_arg_missing",
        "helper_py_chr",
        "helper_py_divmod",
        "helper_py_exc_args",
        "helper_py_exc_matches",
        "helper_py_exc_new",
        "helper_py_fail",
        "helper_py_floordiv",
        "helper_py_getitem",
        "helper_py_iadd",
        "helper_py_int",
        "helper_py_iter",
        "helper_py_iter_guard",
        "helper_py_iter_size",
        "helper_py_length",
        "helper_py_method_append",
        "helper_py_method_clear",
        "helper_py_method_copy",
        "helper_py_method_count",
        "helper_py_method_extend",
        "helper_py_method_get",
        "helper_py_method_insert",
        "helper_py_method_lower",
        "helper_py_method_pop",
        "helper_py_method_remove",
        "helper_py_method_reverse",
        "helper_py_method_setdefault",
        "helper_py_method_split",
        "helper_py_method_upper",
        "helper_py_mod",
        "helper_py_mul",
        "helper_py_name_check",
        "helper_py_neg",
        "helper_py_pow",
        "helper_py_range",
        "helper_py_round",
        "helper_py_setitem",
        "helper_py_show",
        "helper_py_slice",
        "helper_py_to_list",
        "helper_py_type",
    ] {
        rules.push(Unavoidable { rule, node_kind: "Call", reason: "This stored library Action has atomic operands and an await boundary; synchronous Python builtin/operator syntax would decode as Expr and change its Site and execution boundary" });
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
            document: lowered.document,
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
                document: super::super::machine::lower(source).unwrap().document,
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
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
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
