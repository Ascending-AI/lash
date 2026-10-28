fn corpus_tool_signatures() -> anyhow::Result<
    std::collections::BTreeMap<
        lash_kernel_value::names::EffectName,
        lash_kernel_value::types::Signature,
    >,
> {
    Ok(workflow_corpus::tools()?
        .into_iter()
        .map(|definition| {
            (
                definition.operation().effect_name(),
                lash_kernel_value::json_schema::tool_signature(
                    &definition.contract.input_schema.canonical,
                    &definition.contract.output_schema.canonical,
                ),
            )
        })
        .collect())
}

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
            anyhow::anyhow!("{relative}: {error}; run the declared corpus regenerator")
        })?;
        anyhow::ensure!(
            actual == expected,
            "{relative} differs; run the declared corpus regenerator and review the diff"
        );
    }
    Ok(())
}

/// FIG-6953 READABLE-FIXPOINT and READABLE-IDENTITY over every TypeScript
/// corpus document and its canvas edit: each print reads back to its
/// document and prints again to itself, and every print lowers as a plain
/// module with no base to that document.
#[test]
fn every_print_reads_back_and_lowers_to_its_document() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let report = workflow_corpus::readable_prints(root)?;
    for language in [
        lash_kernel_doc::SourceLanguage::TypeScript,
        lash_kernel_doc::SourceLanguage::Python,
    ] {
        let report: Vec<_> = report
            .iter()
            .filter(|printed| printed.language == language)
            .collect();
        eprintln!("readable {language:?} print: documents={}", report.len());
        anyhow::ensure!(
            !report.is_empty(),
            "no {language:?} corpus document printed"
        );
    }
    Ok(())
}

/// FIG-7135: replacing a non-ASCII workflow argument through the canvas
/// preserves the document's exact authored Python print/read identity.
#[test]
fn a_canvas_edited_non_ascii_python_document_reads_back_identically() -> anyhow::Result<()> {
    use lash_kernel_doc::{Expr, Literal, Node};
    use lash_kernel_edit::authoring::{DataValue, Operation, ValueTarget};

    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let signatures = corpus_tool_signatures()?;
    let namespaces = workflow_corpus::corpus_namespaces(&signatures, &Default::default())?;
    let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
    let lowered = lash_dialect_python::lower(
        include_str!("../non-ascii/source.py"),
        &environment,
        lash_kernel_dialect::Mode::Module,
    )?;
    let document = &lowered.document;
    let action = document
        .action_ordinals()
        .into_keys()
        .find(|site| {
            matches!(document.node(site), Some(Node::Expr(Expr::Act(action)))
                if matches!(action.kind, lash_kernel_doc::ActKind::Perform { .. }))
        })
        .expect("the authored echo operation");
    let base = document.identity();
    let mut draft = lash_kernel_edit::Draft::open((**document).clone())?;
    let applied = draft.apply(
        &lash_kernel_edit::Transaction {
            base,
            edits: vec![Operation::SetValue {
                base,
                target: ValueTarget::ToolArgument {
                    action,
                    index: 0,
                    path: vec![],
                },
                value: DataValue::Literal(Literal::Text("edited λ".into())),
            }],
        },
        &lash_kernel_check::Environment::new(embedding.library()),
        &[
            lash_dialect_typescript::CONVENTIONS,
            lash_dialect_python::CONVENTIONS,
        ],
    )?;
    let captions = lowered
        .captions
        .rekey(&applied.correspondence, document, draft.document())
        .expect("caption correspondence");
    let printed = lash_dialect_python::print(
        draft.document(),
        Some(&captions.captions),
        embedding.library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )?;
    let (read, read_captions) = lash_dialect_python::read(
        &printed.source,
        draft.document(),
        embedding.library(),
        Default::default(),
        None,
    )?;
    assert_eq!(read.canonical_bytes(), draft.document().canonical_bytes());
    assert_eq!(read_captions, captions.captions);
    assert!(printed.source.contains("edited λ"));
    printed
        .source_map
        .expect("requested map")
        .validate(draft.document(), &printed.source)?;
    Ok(())
}

/// FIG-7135: module main preserves top-level order and excludes nested
/// declarations from the public entries in both authored printers.
fn module_main_roundtrip(
    source: &str,
    language: lash_kernel_doc::SourceLanguage,
) -> anyhow::Result<()> {
    use lash_kernel_doc::SourceLanguage;
    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let namespaces = Default::default();
    let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
    let lowered = match language {
        SourceLanguage::TypeScript => {
            lash_dialect_typescript::lower(source, &environment, lash_kernel_dialect::Mode::Module)?
        }
        SourceLanguage::Python => {
            lash_dialect_python::lower(source, &environment, lash_kernel_dialect::Mode::Module)?
        }
    };
    assert_eq!(
        lowered
            .document
            .exports
            .keys()
            .map(|name| name.as_str())
            .collect::<Vec<_>>(),
        ["arrow", "declared"]
    );
    let options = lash_kernel_dialect::PrintOptions {
        source_map: true,
        ..Default::default()
    };
    let printed = match language {
        SourceLanguage::TypeScript => lash_dialect_typescript::print(
            &lowered.document,
            Some(&lowered.captions),
            embedding.library(),
            options,
        )?,
        SourceLanguage::Python => lash_dialect_python::print(
            &lowered.document,
            Some(&lowered.captions),
            embedding.library(),
            options,
        )?,
    };
    let (read, captions) = match language {
        SourceLanguage::TypeScript => lash_dialect_typescript::read(
            &printed.source,
            &lowered.document,
            embedding.library(),
            Default::default(),
            None,
        )?,
        SourceLanguage::Python => lash_dialect_python::read(
            &printed.source,
            &lowered.document,
            embedding.library(),
            Default::default(),
            None,
        )?,
    };
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .ok_or_else(|| anyhow::anyhow!("requested map is absent"))?
        .validate(&read, &printed.source)?;
    let mut unspellable = read;
    unspellable
        .main
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("module main is absent"))?
        .push(lash_kernel_doc::Stmt::Try(lash_kernel_doc::TryStmt {
            body: vec![],
            catch: None,
            finally: None,
        }));
    let refused = match language {
        SourceLanguage::TypeScript => {
            lash_dialect_typescript::print(&unspellable, None, embedding.library(), options)
        }
        SourceLanguage::Python => {
            lash_dialect_python::print(&unspellable, None, embedding.library(), options)
        }
    };
    assert!(
        matches!(refused, Err(lash_kernel_dialect::PrintError::Unprintable { site }) if site.unit == lash_kernel_value::sites::Unit::Main)
    );
    Ok(())
}

#[test]
fn typescript_module_main_preserves_block_local_functions() -> anyhow::Result<()> {
    module_main_roundtrip(
        include_str!("../entry-boundary/source.ts"),
        lash_kernel_doc::SourceLanguage::TypeScript,
    )
}

#[test]
fn python_module_main_preserves_block_local_functions() -> anyhow::Result<()> {
    module_main_roundtrip(
        include_str!("../entry-boundary/source.py"),
        lash_kernel_doc::SourceLanguage::Python,
    )
}

/// FIG-7135: a terminal operation is recovered from its typed import use,
/// even with a neutral name; a control-looking ordinary operation stays ordinary.
fn terminal_operation_roundtrip(language: lash_kernel_doc::SourceLanguage) -> anyhow::Result<()> {
    use lash_kernel_dialect::{CellEnvironment, Mode};
    use lash_kernel_doc::SourceLanguage;
    use {
        lash_kernel_value::EffectName, lash_kernel_value::Param, lash_kernel_value::Signature,
        lash_kernel_value::Type,
    };

    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let mut signatures = corpus_tool_signatures()?;
    let signature = Signature {
        params: vec![Param {
            name: "input".into(),
            ty: Type::Any,
            optional: false,
        }],
        result: Type::Any,
    };
    signatures.insert(EffectName::new("corpus.stop")?, signature.clone());
    signatures.insert(EffectName::new("control.finish")?, signature);
    for control in [
        lash_kernel_dialect::EffectControl::Finish,
        lash_kernel_dialect::EffectControl::SwitchAgentFrame,
    ] {
        let controls = std::collections::BTreeMap::from([(
            EffectName::new("corpus.stop")?,
            std::collections::BTreeSet::from([control]),
        )]);
        let namespaces = workflow_corpus::corpus_namespaces(&signatures, &controls)?;
        let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
        let (source, lowered) = match language {
            SourceLanguage::TypeScript => {
                let source = "await control.finish(\"ordinary\");\nawait corpus.stop(\"done\");\nawait corpus.echo(\"unreachable\");\n";
                (
                    source,
                    lash_dialect_typescript::lower(
                        source,
                        &environment,
                        Mode::Cell(CellEnvironment::first()),
                    )?,
                )
            }
            SourceLanguage::Python => {
                let source = "await control.finish(\"ordinary\")\nawait corpus.stop(\"done\")\nawait corpus.echo(\"unreachable\")\n";
                (
                    source,
                    lash_dialect_python::lower(
                        source,
                        &environment,
                        Mode::Cell(CellEnvironment::first()),
                    )?,
                )
            }
        };
        let main = lowered
            .document
            .main
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("cell main is absent"))?;
        assert!(matches!(main[1], lash_kernel_doc::Stmt::Finish { .. }));
        assert!(!matches!(main[0], lash_kernel_doc::Stmt::Finish { .. }));
        let options = lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        };
        let printed = match language {
            SourceLanguage::TypeScript => lash_dialect_typescript::print(
                &lowered.document,
                Some(&lowered.captions),
                embedding.library(),
                options,
            )?,
            SourceLanguage::Python => lash_dialect_python::print(
                &lowered.document,
                Some(&lowered.captions),
                embedding.library(),
                options,
            )?,
        };
        let (read, captions) = match language {
            SourceLanguage::TypeScript => lash_dialect_typescript::read(
                &printed.source,
                &lowered.document,
                embedding.library(),
                Default::default(),
                None,
            )?,
            SourceLanguage::Python => lash_dialect_python::read(
                &printed.source,
                &lowered.document,
                embedding.library(),
                Default::default(),
                None,
            )?,
        };
        assert_eq!(
            read.canonical_bytes(),
            lowered.document.canonical_bytes(),
            "{source} -> {}",
            printed.source
        );
        assert_eq!(captions, lowered.captions);
        printed
            .source_map
            .ok_or_else(|| anyhow::anyhow!("requested map is absent"))?
            .validate(&read, &printed.source)?;
    }
    Ok(())
}

#[test]
fn typescript_terminal_operations_read_back_by_import_identity() -> anyhow::Result<()> {
    terminal_operation_roundtrip(lash_kernel_doc::SourceLanguage::TypeScript)
}

#[test]
fn python_terminal_operations_read_back_by_import_identity() -> anyhow::Result<()> {
    terminal_operation_roundtrip(lash_kernel_doc::SourceLanguage::Python)
}

/// FIG-7135: an escaped closure keeps its captured list's read and later mutation.
#[test]
fn typescript_capturing_cell_prints_and_reads_identically() -> anyhow::Result<()> {
    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let signatures = corpus_tool_signatures()?;
    let namespaces = workflow_corpus::corpus_namespaces(&signatures, &Default::default())?;
    let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
    let lowered = lash_dialect_typescript::lower(
        include_str!("../capturing-cell/source.ts"),
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
    )?;
    let printed = lash_dialect_typescript::print(
        &lowered.document,
        Some(&lowered.captions),
        embedding.library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )?;
    let (read, captions) = lash_dialect_typescript::read(
        &printed.source,
        &lowered.document,
        embedding.library(),
        Default::default(),
        None,
    )?;
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .ok_or_else(|| anyhow::anyhow!("requested map is absent"))?
        .validate(&read, &printed.source)?;
    Ok(())
}

/// FIG-7135: recover the typed operation input and indexed mutation in the
/// capturing cell without changing its document, captions or mapped sites.
#[test]
fn python_capturing_cell_prints_and_reads_identically() -> anyhow::Result<()> {
    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let signatures = corpus_tool_signatures()?;
    let namespaces = workflow_corpus::corpus_namespaces(&signatures, &Default::default())?;
    let environment = lash_kernel_dialect::Environment::new(embedding.library(), &namespaces);
    let lowered = lash_dialect_python::lower(
        include_str!("../capturing-cell/source.py"),
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
    )?;
    let printed = lash_dialect_python::print(
        &lowered.document,
        Some(&lowered.captions),
        embedding.library(),
        lash_kernel_dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )?;
    let (read, captions) = lash_dialect_python::read(
        &printed.source,
        &lowered.document,
        embedding.library(),
        Default::default(),
        None,
    )?;
    assert_eq!(read.canonical_bytes(), lowered.document.canonical_bytes());
    assert_eq!(captions, lowered.captions);
    printed
        .source_map
        .ok_or_else(|| anyhow::anyhow!("requested map is absent"))?
        .validate(&read, &printed.source)?;
    Ok(())
}

/// FIG-6953 READABLE-STYLE: a workflow edited through the canvas API prints
/// as the source the authoring guide teaches, with none of the kernel
/// surface's scaffolding.
#[test]
fn a_canvas_edited_workflow_prints_without_kernel_scaffolding() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let report = workflow_corpus::readable_prints(root)?;
    for language in [
        lash_kernel_doc::SourceLanguage::TypeScript,
        lash_kernel_doc::SourceLanguage::Python,
    ] {
        let mut edited = 0;
        for printed in report
            .iter()
            .filter(|printed| printed.edited && printed.language == language)
        {
            for scaffolding in workflow_corpus::SCAFFOLDING {
                anyhow::ensure!(
                    !printed.source.contains(scaffolding),
                    "{}: the canvas-edited print spells `{scaffolding}`\n{}",
                    printed.case,
                    printed.source
                );
            }
            anyhow::ensure!(
                printed.source.contains("edited λ"),
                "{}: the print shows the edit\n{}",
                printed.case,
                printed.source
            );
            edited += 1;
        }
        anyhow::ensure!(edited > 0, "no canvas-edited {language:?} workflow lifted");
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
/// no extra statement (437803). A function or closure whose only statement
/// returns a dialect-helper call taking the author's inline body closure
/// counts zero extra; the body's statements count normally (441660).
/// An entry's run-root task cleanup is its start convention, not a statement
/// (FIG-6643), so entry_cleanup is zero everywhere; a cell's root wrap would
/// still be counted there, never as an operand pin (437760).
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
    let mut effects = corpus_tool_signatures().expect("the shared corpus tool signatures");
    {
        effects.insert(
            lash_kernel_value::names::EffectName::new("control.finish").unwrap(),
            lash_kernel_value::types::Signature {
                params: vec![lash_kernel_value::types::Param {
                    name: lash_kernel_value::names::Name::new("input"),
                    ty: lash_kernel_value::types::Type::Any,
                    optional: true,
                }],
                result: lash_kernel_value::types::Type::Any,
            },
        );
    }
    let controls = std::collections::BTreeMap::from([(
        lash_kernel_value::names::EffectName::new("control.finish").unwrap(),
        std::collections::BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let environment_namespaces = workflow_corpus::corpus_namespaces(&effects, &controls)
        .expect("the test's effects are operations");
    let environment =
        lash_kernel_dialect::Environment::new(embedding.library(), &environment_namespaces);
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
        let mut bound = std::collections::BTreeMap::new();
        for (unit, body) in document.units() {
            let mut nodes = vec![lash_kernel_doc::Node::Block(body)];
            while let Some(node) = nodes.pop() {
                use lash_kernel_doc::{Expr, Node, Rhs, Stmt};
                let binding = match node {
                    Node::Stmt(Stmt::Let {
                        name,
                        value: Rhs::Expr(Expr::Closure(closure)),
                    }) => Some((name, closure.function)),
                    Node::Expr(Expr::Let { name, value, .. }) => match &**value {
                        Expr::Closure(closure) => Some((name, closure.function)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((name, function)) = binding {
                    assert!(
                        bound
                            .insert((unit.clone(), name.clone()), function)
                            .is_none(),
                        "one closure binding per local identity"
                    );
                }
                nodes.extend(node.children());
            }
        }
        let target =
            |unit: &lash_kernel_value::sites::Unit, value: &lash_kernel_doc::Expr| match value {
                lash_kernel_doc::Expr::Closure(closure) => Some(closure.function),
                lash_kernel_doc::Expr::Variable(name) => {
                    bound.get(&(unit.clone(), name.clone())).copied()
                }
                _ => None,
            };
        let is_run = |action: &lash_kernel_doc::ActExpr| {
            matches!(&action.kind,
            lash_kernel_doc::ActKind::Spawn(lash_kernel_doc::Callee::Local(function))
                if lash::vm::helper_release().unwrap().helpers.registered(function).is_some_and(|registered| registered.name() == "py.task.run"))
        };
        let mut deferred = std::collections::BTreeSet::new();
        for (unit, body) in document.units() {
            let mut nodes = vec![lash_kernel_doc::Node::Block(body)];
            while let Some(node) = nodes.pop() {
                if let lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Act(action)) = node
                    && is_run(action)
                    && let [work] = action.args.as_slice()
                    && let Some(function) = target(&unit, work)
                    && document.functions[&function].signature.params.is_empty()
                    && matches!(
                        document.functions[&function]
                            .body()
                            .unwrap()
                            .block
                            .as_slice(),
                        [lash_kernel_doc::Stmt::Return {
                            value: lash_kernel_doc::Expr::Act(_)
                        }]
                    )
                {
                    deferred.insert(function);
                }
                nodes.extend(node.children());
            }
        }
        let mut pending = if cell {
            vec![(
                lash_kernel_value::sites::Unit::Main,
                lash_kernel_doc::Node::Block(document.main.as_ref().unwrap()),
            )]
        } else {
            document
                .exports
                .values()
                .filter_map(|export| match export {
                    lash_kernel_doc::Export::Function(function) => Some((
                        lash_kernel_value::sites::Unit::Library(function.function),
                        lash_kernel_doc::Node::Block(
                            &document.functions[&function.function].body().unwrap().block,
                        ),
                    )),
                    _ => None,
                })
                .collect()
        };
        while let Some((unit, node)) = pending.pop() {
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
                && let ActKind::Call(Callee::Local(function)) = &action.kind
                && document.library.contains(function)
                && lash::vm::helper_release()
                    .unwrap()
                    .helpers
                    .registered(function)
                    .is_some_and(|registered| registered.name() == "tasks.cancel_all")
            {
                entry_cleanup += 2;
                pending.push((unit.clone(), Node::Block(&attempt.body)));
                continue;
            }
            if let Node::Expr(Expr::Act(action)) = node
                && is_run(action)
                && let [work] = action.args.as_slice()
                && let Some(function) = target(&unit, work)
                && deferred.contains(&function)
                && let [
                    Stmt::Return {
                        value: Expr::Act(call),
                    },
                ] = document.functions[&function]
                    .body()
                    .unwrap()
                    .block
                    .as_slice()
            {
                deferred_calls += 1;
                pending.extend(call.args.iter().map(|expr| {
                    (
                        lash_kernel_value::sites::Unit::Library(function),
                        Node::Expr(expr),
                    )
                }));
                continue;
            }
            statements += usize::from(matches!(node, lash_kernel_doc::Node::Stmt(_)));
            loops += usize::from(matches!(
                node,
                lash_kernel_doc::Node::Stmt(
                    lash_kernel_doc::Stmt::For { .. } | lash_kernel_doc::Stmt::While { .. }
                )
            ));
            if let Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node
                && !deferred.contains(&closure.function)
            {
                pending.push((
                    lash_kernel_value::sites::Unit::Library(closure.function),
                    Node::Block(&document.functions[&closure.function].body().unwrap().block),
                ));
            }
            pending.extend(
                node.children()
                    .into_iter()
                    .map(|child| (unit.clone(), child)),
            );
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
            "fan-out" => (3, 0, 0, 2),
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

/// FIG-6402 rev2 §15 SHAPE. Count the source statements (including catch
/// clauses), with one body per source loop. Loop header bindings add no
/// statement; an escaping target adds one operand pin. Comprehensions count
/// each for/if clause and one collect binding (437657). Deferred-call-only
/// inline closures add no statement (437803). A function or closure whose
/// only statement returns a dialect-helper call taking the author's inline
/// body closure counts zero extra; the body's statements count normally
/// (441660). The builtin-forwarder factory passed to ts.copy_boundary is
/// helper-owned; only the copied value contributes authored statements.
/// Retained tested reads are pinned separately as read_ladder,
/// pending FIG-6672/FIG-6643, and never included in operand pins. Kernel
/// action hoists are forbidden (FIG-6657). The four original probes have
/// 8/8/6/5 author statements, against the old 96/87/96/27 lowered baselines.
#[test]
fn every_typescript_corpus_case_has_source_shaped_statements_and_one_body_per_loop() {
    use lash_kernel_doc::{Node, Stmt};

    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let effects = [
        "control.finish",
        "display.show_message",
        "display.set_status",
        "display.add_item",
        "gmail.list_recent",
        "llm.query",
        "web.search",
    ]
    .into_iter()
    .map(|name| {
        (
            lash_kernel_value::names::EffectName::new(name).unwrap(),
            lash_kernel_value::types::Signature {
                params: vec![lash_kernel_value::types::Param {
                    name: "input".into(),
                    ty: lash_kernel_value::types::Type::Any,
                    optional: false,
                }],
                result: lash_kernel_value::types::Type::Any,
            },
        )
    })
    .chain(corpus_tool_signatures().expect("the shared corpus tool signatures"))
    .collect();
    let controls = std::collections::BTreeMap::from([(
        lash_kernel_value::names::EffectName::new("control.finish").unwrap(),
        std::collections::BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let env_namespaces = workflow_corpus::corpus_namespaces(&effects, &controls)
        .expect("the test's effects are operations");
    let env = lash_kernel_dialect::Environment::new(embedding.library(), &env_namespaces);
    let mut failures = Vec::new();
    let sources = [
        (
            "straight-tool",
            include_str!("../straight-tool/source.ts"),
            1,
            0,
        ),
        ("branch", include_str!("../branch/source.ts"), 3, 0),
        (
            "iteration-typed",
            include_str!("../iteration-typed/source.ts"),
            4,
            1,
        ),
        (
            "iteration-untyped",
            include_str!("../iteration-untyped/source.ts"),
            4,
            1,
        ),
        (
            "try-finally",
            include_str!("../try-finally/source.ts"),
            7,
            0,
        ),
        ("fan-out", include_str!("../fan-out/source.ts"), 3, 0),
        ("sleep", include_str!("../sleep/source.ts"), 2, 0),
        (
            "closures-helpers",
            include_str!("../closures-helpers/source.ts"),
            3,
            0,
        ),
        (
            "early-return",
            include_str!("../early-return/source.ts"),
            4,
            0,
        ),
        ("non-ascii", include_str!("../non-ascii/source.ts"), 2, 0),
        (
            "entry-boundary",
            include_str!("../entry-boundary/source.ts"),
            2,
            0,
        ),
        (
            "early-finish",
            include_str!("../early-finish/source.ts"),
            2,
            0,
        ),
        (
            "capturing-cell",
            include_str!("../capturing-cell/source.ts"),
            6,
            0,
        ),
        ("digest", include_str!("../probe-digest/source.ts"), 8, 1),
        ("typed", include_str!("../probe-typed/source.ts"), 8, 1),
        ("inbox", include_str!("../probe-inbox/source.ts"), 6, 1),
        ("flat", include_str!("../probe-flat/source.ts"), 5, 0),
        ("nested", include_str!("../probe-nested/source.ts"), 8, 2),
        ("retry", include_str!("../probe-retry/source.ts"), 10, 1),
        ("helper", include_str!("../probe-helper/source.ts"), 9, 1),
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
                    .any(|dialect| dialect == "ts")
        })
        .map(|case| case["name"].as_str().unwrap())
        .collect();
    let measured: std::collections::BTreeSet<_> = sources
        .iter()
        .map(|(name, ..)| match *name {
            "digest" => "probe-digest",
            "typed" => "probe-typed",
            "inbox" => "probe-inbox",
            "flat" => "probe-flat",
            "nested" => "probe-nested",
            "retry" => "probe-retry",
            "helper" => "probe-helper",
            name => name,
        })
        .collect();
    assert_eq!(
        measured, admitted,
        "every admitted TypeScript corpus case is measured"
    );
    for (name, source, author, source_loops) in sources {
        let cell = matches!(name, "early-finish" | "capturing-cell");
        let lowered = lash_dialect_typescript::lower(
            source,
            &env,
            if cell {
                lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first())
            } else {
                lash_kernel_dialect::Mode::Module
            },
        )
        .unwrap();
        let mut pending: Vec<_> = if cell {
            vec![Node::Block(lowered.document.main.as_ref().unwrap())]
        } else {
            lowered
                .document
                .exports
                .values()
                .filter_map(|export| match export {
                    lash_kernel_doc::Export::Function(function) => Some(Node::Block(
                        &lowered.document.functions[&function.function]
                            .body()
                            .unwrap()
                            .block,
                    )),
                    _ => None,
                })
                .collect()
        };
        let (mut statements, mut loops) = (0, 0);
        let mut adapters = 0;
        while let Some(node) = pending.pop() {
            if let Node::Expr(lash_kernel_doc::Expr::Act(action)) = node
                && let lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(function)) =
                    action.kind
                && lowered.document.library.contains(&function)
                && lash::vm::helper_release()
                    .unwrap()
                    .helpers
                    .registered(&function)
                    .is_some_and(|registered| registered.name() == "ts.copy_boundary")
                && let [value, _factory] = action.args.as_slice()
            {
                // Copy-out owns the forwarding factory, including its body.
                // Still traverse the value and any authored closures within it.
                pending.push(Node::Expr(value));
                continue;
            }
            if let Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node
                && let [
                    Stmt::Return {
                        value: lash_kernel_doc::Expr::Act(action),
                    },
                ] = lowered.document.functions[&closure.function]
                    .body()
                    .unwrap()
                    .block
                    .as_slice()
                && let lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(function)) =
                    action.kind
                && lowered.document.library.contains(&function)
                && lash::vm::helper_release()
                    .unwrap()
                    .helpers
                    .registered(&function)
                    .is_some_and(|registered| registered.name() == "ts.promise.start")
                && matches!(action.args.first(), Some(lash_kernel_doc::Expr::Closure(_)))
            {
                adapters += 1;
                pending.extend(action.args.iter().map(Node::Expr));
                continue;
            }
            statements += usize::from(matches!(node, Node::Stmt(_)));
            loops += usize::from(matches!(
                node,
                Node::Stmt(Stmt::For { .. } | Stmt::While { .. })
            ));
            if let Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node {
                pending.push(Node::Block(
                    &lowered.document.functions[&closure.function]
                        .body()
                        .unwrap()
                        .block,
                ));
            }
            pending.extend(node.children());
        }
        // These are reviewed source counts and measured statement counts,
        // pinned per fixture: no blanket multiplier or spare allowance.
        // Capturing-cell's two parameter bindings remain outside its return
        // solely because the retained read ladder splits that expression;
        // its arithmetic left operand is the one §12 order pin. Helper's
        // two parameter bindings stay outside its query for the same reason.
        let (expected, read_ladder, operand_pins, expected_adapters) = match name {
            "straight-tool" => (1, 0, 0, 0),
            "branch" => (3, 0, 0, 0),
            "iteration-typed" | "iteration-untyped" => (4, 0, 0, 0),
            "try-finally" => (6, 0, 0, 0),
            "fan-out" => (3, 0, 0, 1),
            "sleep" => (2, 0, 0, 0),
            "closures-helpers" => (12, 9, 0, 0),
            "early-return" => (4, 0, 0, 0),
            "non-ascii" | "entry-boundary" | "early-finish" => (2, 0, 0, 0),
            "capturing-cell" => (15, 8, 1, 1),
            "digest" | "typed" => (14, 6, 0, 0),
            "inbox" => (12, 6, 0, 0),
            "flat" => (5, 0, 0, 0),
            "nested" => (14, 6, 0, 0),
            "retry" => (29, 20, 0, 0),
            "helper" => (17, 8, 0, 1),
            _ => unreachable!(),
        };
        let kernel_hoist = 0;
        println!(
            "{name}: statements={statements} author={author} loops={loops} operand_pins={operand_pins} read_ladder={read_ladder} adapters={adapters} kernel_hoist={kernel_hoist}"
        );
        assert_eq!(
            (statements, loops, adapters, kernel_hoist),
            (expected, source_loops, expected_adapters, 0),
            "{name}: pinned SHAPE columns"
        );
        lash_kernel_doc::validate_document(&lowered.document, env.library).unwrap();
        if statements - read_ladder > author + operand_pins || loops != source_loops {
            failures.push(format!(
                "{name}: {statements}/{author} statements, {loops}/{source_loops} loops"
            ));
        }
    }
    assert!(failures.is_empty(), "SHAPE: {}", failures.join("\n"));
}

#[test]
#[ignore = "regenerates examples/workflows"]
#[expect(
    clippy::disallowed_methods,
    reason = "the opt-in recorder writes readable outputs in its supplied workspace"
)]
fn regenerate_readable_workflow_outputs() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("LASH_REGENERATE").as_deref() == Ok("1"),
        "set LASH_REGENERATE=1 to regenerate readable workflow outputs"
    );
    let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY")
        .ok_or_else(|| anyhow::anyhow!("regeneration requires BUILD_WORKSPACE_DIRECTORY"))?;
    let root = std::path::PathBuf::from(workspace).join("examples/workflows");
    for (relative, text) in workflow_corpus::render(&root)? {
        let path = root.join(relative);
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("output parent"))?,
        )?;
        std::fs::write(path, text)?;
    }
    Ok(())
}
