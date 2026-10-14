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
fn shared_ordinary_corpus_has_zero_generic_forms_and_exact_bytes() {
    let cases = readability::corpus().expect("valid shared readability corpus");
    let report = readability::check(&cases, &[dialect(&[])]);
    report.assert_passed();
    assert_eq!(report.documents, 7);
    assert!(report.generic_forms.is_empty());
    for case in cases {
        let printed = super::roundtrip(&case.document, false);
        if case.name == "three_effect_workflow" {
            assert_eq!(
                printed.source.split_once('\n').unwrap().1,
                r#"def deep_research():
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
    report.assert_passed();
    assert_eq!(report.documents, 28);
    eprintln!(
        "authored Python: documents={} unavoidable={:?}",
        report.documents, report.generic_forms
    );
}
