#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the corpus law reads declared fixture data"
)]
fn design_8_corpus_pins_readable_per_dialect_outputs() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let outputs = workflow_corpus::render(root)?;
    for (relative, expected) in outputs {
        let path = root.join(&relative);
        let actual = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!("{relative}: {error}; run the corpus bless command")
        })?;
        anyhow::ensure!(
            actual == expected,
            "{relative} differs; run the corpus bless command and review the diff"
        );
    }
    Ok(())
}

/// FIG-6402 rev2 §15 SHAPE over every admitted L5 Python input.
/// Author structure counts statements and except clauses. Each comprehension
/// adds one loop per for clause, one statement per if clause, and one collect /
/// result binding. A loop target needed after its loop permits one operand pin;
/// otherwise binding it belongs to the header. These rules apply to both
/// dialects where the construct exists (owner ruling 437657). An inline
/// closure containing only the return of a deferred source call contributes
/// no extra statement (437803); a named thunk or any other body counts normally.
/// Run-root task cleanup is pinned separately as entry_cleanup (437760).
/// A `while` test and an assignment's place keep their actions nested
/// (FIG-6657), so no action is hoisted for the kernel. The cleanup column is
/// not an operand-pin allowance.
#[test]
fn every_python_corpus_case_has_one_body_per_source_loop_and_no_statement_inflation() {
    use ruff_python_ast::visitor::{self, Visitor};
    #[derive(Default)]
    struct SourceShape {
        statements: usize,
        loops: usize,
    }
    impl<'a> Visitor<'a> for SourceShape {
        fn visit_stmt(&mut self, stmt: &'a ruff_python_ast::Stmt) {
            self.statements += 1;
            self.loops += usize::from(matches!(
                stmt,
                ruff_python_ast::Stmt::For(_) | ruff_python_ast::Stmt::While(_)
            ));
            visitor::walk_stmt(self, stmt);
        }
        fn visit_except_handler(&mut self, handler: &'a ruff_python_ast::ExceptHandler) {
            self.statements += 1;
            visitor::walk_except_handler(self, handler);
        }
        fn visit_expr(&mut self, expr: &'a ruff_python_ast::Expr) {
            let generators = match expr {
                ruff_python_ast::Expr::ListComp(comp) => Some(&comp.generators),
                ruff_python_ast::Expr::SetComp(comp) => Some(&comp.generators),
                ruff_python_ast::Expr::DictComp(comp) => Some(&comp.generators),
                ruff_python_ast::Expr::Generator(comp) => Some(&comp.generators),
                _ => None,
            };
            if let Some(generators) = generators {
                self.loops += generators.len();
                self.statements += 1 + generators
                    .iter()
                    .map(|generator| 1 + generator.ifs.len())
                    .sum::<usize>();
            }
            visitor::walk_expr(self, expr);
        }
    }
    let sources = [
        ("straight-tool", include_str!("../straight-tool/source.py")),
        ("branch", include_str!("../branch/source.py")),
        (
            "iteration-typed",
            include_str!("../iteration-typed/source.py"),
        ),
        (
            "iteration-untyped",
            include_str!("../iteration-untyped/source.py"),
        ),
        ("try-finally", include_str!("../try-finally/source.py")),
        ("fan-out", include_str!("../fan-out/source.py")),
        ("sleep", include_str!("../sleep/source.py")),
        (
            "closures-helpers",
            include_str!("../closures-helpers/source.py"),
        ),
        ("early-return", include_str!("../early-return/source.py")),
        ("non-ascii", include_str!("../non-ascii/source.py")),
        (
            "entry-boundary",
            include_str!("../entry-boundary/source.py"),
        ),
        ("early-finish", include_str!("../early-finish/source.py")),
        (
            "capturing-cell",
            include_str!("../capturing-cell/source.py"),
        ),
    ];
    let catalog: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../catalog.json")).unwrap();
    let admitted: std::collections::BTreeSet<_> = catalog
        .iter()
        .filter(|case| {
            case.get("refusal").is_none()
                && case["dialects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|dialect| dialect == "py")
        })
        .map(|case| case["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        sources
            .iter()
            .map(|(name, _)| *name)
            .collect::<std::collections::BTreeSet<_>>(),
        admitted,
        "every admitted Python corpus case is measured"
    );
    let mut effects = std::collections::BTreeMap::new();
    for name in ["corpus.echo", "finish"] {
        effects.insert(
            lash_kernel_doc::EffectName::new(name).unwrap(),
            lash_kernel_doc::Signature {
                params: vec![lash_kernel_doc::Param {
                    name: lash_kernel_doc::Name::new("input"),
                    ty: lash_kernel_doc::Type::Any,
                    optional: true,
                }],
                result: lash_kernel_doc::Type::Any,
            },
        );
    }
    let controls = std::collections::BTreeMap::from([(
        lash_kernel_doc::EffectName::new("finish").unwrap(),
        std::collections::BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let roots = std::collections::BTreeSet::from([lash_kernel_doc::Name::new("corpus")]);
    let embedding = lash_vm_worker::built_dialects(&Default::default()).unwrap();
    let environment = lash_kernel_dialect::Environment {
        library: embedding.library(),
        effects: &effects,
        controls: &controls,
        tool_roots: &roots,
    };
    let mut failures = Vec::new();
    for (name, source) in sources {
        let parsed = ruff_python_parser::parse_module(source).unwrap();
        let mut author = SourceShape::default();
        for stmt in &parsed.syntax().body {
            author.visit_stmt(stmt);
        }
        let cell = matches!(name, "early-finish" | "capturing-cell");
        let lowered = lash_dialect_python::lower(
            source,
            &environment,
            if cell {
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            } else {
                lash_kernel_dialect::Mode::Module
            },
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let document = &lowered.document;
        let mut statements = 0;
        let mut loops = 0;
        let mut entry_cleanup = 0;
        let mut deferred_calls = 0;
        let mut pending = if cell {
            vec![lash_kernel_doc::Node::Block(&document.main)]
        } else {
            document
                .entries
                .keys()
                .map(|name| lash_kernel_doc::Node::Block(&document.functions[name].body))
                .collect()
        };
        while let Some(node) = pending.pop() {
            use lash_kernel_doc::{ActKind, Callee, Expr, Node, Stmt};
            if let Node::Stmt(Stmt::Try(attempt)) = node
                && attempt.catch.is_none()
                && let Some(
                    [
                        Stmt::Do {
                            value: Expr::Act(action),
                        },
                    ],
                ) = attempt.finally.as_deref()
                && let ActKind::Call(Callee::Library(function)) = &action.kind
                && document
                    .manifest
                    .functions
                    .get(function)
                    .is_some_and(|name| name.as_str() == "tasks.cancel_all")
            {
                entry_cleanup += 2;
                pending.push(Node::Block(&attempt.body));
                continue;
            }
            if let Node::Expr(Expr::Act(action)) = node
                && let ActKind::Spawn(Callee::Library(function)) = &action.kind
                && document
                    .manifest
                    .functions
                    .get(function)
                    .is_some_and(|name| name.as_str() == "py.task.run")
                && let [Expr::Closure(closure)] = action.args.as_slice()
                && closure.params.is_empty()
                && let [
                    Stmt::Return {
                        value: Expr::Act(call),
                    },
                ] = closure.body.as_slice()
            {
                deferred_calls += 1;
                pending.extend(call.args.iter().map(Node::Expr));
                continue;
            }
            statements += usize::from(matches!(node, lash_kernel_doc::Node::Stmt(_)));
            loops += usize::from(matches!(
                node,
                lash_kernel_doc::Node::Stmt(
                    lash_kernel_doc::Stmt::For { .. } | lash_kernel_doc::Stmt::While { .. }
                )
            ));
            pending.extend(node.children());
        }
        println!(
            "{name}: author={} lowered={statements} source_loops={} lowered_loops={loops} entry_cleanup={entry_cleanup} deferred_calls={deferred_calls}",
            author.statements, author.loops
        );
        let expected = match name {
            "straight-tool" => (1, 0, 0, 0),
            "branch" => (3, 0, 0, 0),
            "iteration-typed" | "iteration-untyped" => (4, 1, 0, 0),
            "try-finally" => (8, 0, 0, 0),
            "fan-out" => (3, 0, 2, 2),
            "sleep" => (2, 0, 0, 0),
            "closures-helpers" => (6, 1, 0, 0),
            "early-return" => (4, 0, 0, 0),
            "non-ascii" | "entry-boundary" | "early-finish" => (2, 0, 0, 0),
            "capturing-cell" => (6, 0, 0, 0),
            _ => unreachable!(),
        };
        assert_eq!(
            (statements, loops, entry_cleanup, deferred_calls),
            expected,
            "{name}: pinned per-case shape"
        );
        if statements > author.statements || loops != author.loops {
            failures.push(format!("{name}: {statements} statements / {loops} loops for {} author statements / {} loops\n{}", author.statements, author.loops, lash_kernel_doc::print_document(document)));
        }
    }
    assert!(failures.is_empty(), "SHAPE: {}", failures.join("\n"));
}
